package app.airhand.android.host

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import app.airhand.android.AirhandApp
import app.airhand.android.BuildConfig
import app.airhand.android.R
import app.airhand.android.controller.ControllerClient
import app.airhand.android.core.ClipFiles
import app.airhand.android.core.Code
import app.airhand.android.core.Native
import app.airhand.android.core.Problem
import app.airhand.android.core.ServerState
import app.airhand.android.data.HostCode
import app.airhand.android.data.Pairing
import app.airhand.android.ui.MainActivity
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import org.json.JSONArray
import org.json.JSONObject
import kotlin.concurrent.thread

/**
 * 被控端的前台服务：持有屏幕录制授权（MediaProjection），连接服务器拿到控制码，
 * 有人连上来时采集、编码屏幕和声音，把对方的操作交给无障碍服务执行。
 *
 * 屏幕录制授权只能由用户在界面上同意（[MainActivity] 里点"开始共享"），所以服务被系统杀掉后不自动重启。
 */
class HostService : Service() {
    private val app get() = application as AirhandApp
    private val main = Handler(Looper.getMainLooper())
    private var scope: CoroutineScope? = null

    private var projection: MediaProjection? = null
    private var capture: ScreenCapture? = null
    private var audio: AudioCapture? = null
    private var wakeLock: PowerManager.WakeLock? = null

    @Volatile
    private var handle = 0L

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START -> start(intent)
            ACTION_STOP -> stopSharing(null)
            // 通知里的"全部断开"：共享已经停了（通知留着的时候）就没什么可断的
            ACTION_KICK_ALL -> if (projection == null) stopSelf() else kickAll()
            else -> if (projection == null) stopSelf()
        }
        return START_NOT_STICKY
    }

    private fun start(intent: Intent) {
        startForegroundCompat(notification(null, 0))
        if (projection != null) return
        val resultCode = intent.getIntExtra(EXTRA_RESULT_CODE, 0)
        @Suppress("DEPRECATION")
        val data: Intent? = intent.getParcelableExtra(EXTRA_DATA)
        val mpm = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        val p = runCatching { data?.let { mpm.getMediaProjection(resultCode, it) } }.getOrNull()
        if (p == null) {
            Log.w(TAG, "no media projection")
            stopSharing(getString(R.string.share_stopped_no_permission))
            return
        }
        projection = p
        // Android 14 起必须先登记回调再建虚拟显示器；用户在系统里停止录屏时这里收到通知
        p.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                main.post { if (projection === p) stopSharing(getString(R.string.share_stopped_by_system)) }
            }
        }, main)
        instance = this
        HostState.update { HostStatus(running = true) }
        startNative()
        capture = ScreenCapture(this, p) { handle }
        // 无障碍服务开关变化：告诉控制端能不能操作
        scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
        scope?.launch {
            InputService.connected.collect { on ->
                val h = handle
                if (h != 0L) Native.hostCall(h, JSONObject().put("op", "permissions").put("input", on).toString())
            }
        }
        Log.i(TAG, "screen sharing started")
    }

    private fun startNative() {
        val s = app.settings.value
        val saved = app.hostCodes.load()
        val cfg = JSONObject()
            .put("server", s.server)
            .put("fingerprint", s.fingerprint)
            .put("trust", s.trustJson())
            .put("name", app.settings.deviceName())
            .put("os", ControllerClient.osName())
            .put("version", BuildConfig.VERSION_NAME)
            .put("reclaim", saved?.let { JSONObject().put("code", it.code).put("secret", it.secret) } ?: JSONObject.NULL)
            .put("displays", JSONArray().put(ScreenCapture.displayInfo(ScreenCapture.screen(this)).toJson()))
            .put("codecs", JSONArray(ScreenCapture.encodable.map { it.wire }))
            .put("input", InputService.isEnabled(this))
            .put("touch", Injector.touchSupported)
            .put("actions", JSONArray(Injector.supportedActions()))
            // 本机的端到端加密身份：配对过的控制端靠它认出本机
            .put("identity", app.settings.identity())
            // 配对码、配对过的控制端（没有的话 Rust 生成配对码，用 pairing 事件交回来保存）
            .put("pairing", app.pairing.value?.toJson() ?: JSONObject.NULL)
        val h = try {
            Native.hostStart(cfg.toString())
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "can't start the host: ${e.message}")
            HostState.update { it.copy(server = ServerState.Disconnected(Problem("failed", e.message))) }
            return
        }
        handle = h
        saved?.let { c -> HostState.update { it.copy(code = c.code.toString()) } }
        thread(name = "airhand-host-events", isDaemon = true) { pump(h) }
    }

    private fun stopNative() {
        val h = handle
        handle = 0L
        if (h != 0L) Native.hostStop(h)
        audio?.release()
        audio = null
        releaseWakeLock()
        HostState.update { it.copy(viewers = emptyList(), streaming = false, server = ServerState.Connecting) }
    }

    /** 服务器设置变了：用新设置重新连接（屏幕录制授权保留） */
    private fun restartNative() {
        stopNative()
        startNative()
    }

    private fun pump(h: Long) {
        while (handle == h) {
            val text = Native.hostPollEvent(h, 500) ?: continue
            val e = runCatching { JSONObject(text) }.getOrNull() ?: continue
            when (e.optString("type")) {
                // 输入直接交给注入线程，不经过主线程
                "input" -> e.optJSONArray("events")?.let { InputService.inject(it) }
                else -> main.post { if (handle == h) onEvent(e) }
            }
        }
    }

    private fun onEvent(e: JSONObject) {
        when (e.optString("type")) {
            "server" -> {
                val s = ServerState.parse(e.getJSONObject("state"))
                HostState.update { it.copy(server = s) }
                updateNotification()
            }
            "registered" -> {
                val code = e.getInt("code")
                app.hostCodes.save(HostCode(code, e.getString("secret")))
                HostState.update { it.copy(code = code.toString()) }
                updateNotification()
            }
            "released" -> {
                app.hostCodes.save(null)
                HostState.update { it.copy(code = null) }
            }
            "controllers" -> {
                val list = e.optJSONArray("list") ?: JSONArray()
                val viewers = (0 until list.length()).map { list.getJSONObject(it) }.map {
                    Viewer(
                        it.optString("id"),
                        it.optString("key"),
                        it.optString("name"),
                        it.optString("ip"),
                        it.optString("safety"),
                        it.optBoolean("justPaired"),
                    )
                }
                val before = HostState.value.viewers.map { it.id }.toSet()
                HostState.update { it.copy(viewers = viewers) }
                if (viewers.isEmpty()) releaseWakeLock() else acquireWakeLock()
                updateNotification()
                // 有设备连上来了：提醒手机前的人（配对过的连上来不用谁点允许，所以要让人知道）
                for (v in viewers) if (v.id !in before) notifyConnected(v)
            }
            "pairing" -> e.optJSONObject("state")?.let { app.pairing.save(Pairing.parse(it)) }
            "video" -> {
                val active = e.optBoolean("active")
                capture?.setActive(active)
                HostState.update { it.copy(streaming = active) }
            }
            "audio" -> {
                val p = projection
                if (e.optBoolean("active") && audio == null && p != null) {
                    audio = AudioCapture(this, p, { handle }) { failure ->
                        val h = handle
                        if (h != 0L) Native.hostCall(h, JSONObject().put("op", "audio").put("failure", failure ?: JSONObject.NULL).toString())
                        HostState.update { it.copy(audioProblem = failure) }
                    }
                } else if (!e.optBoolean("active")) {
                    audio?.release()
                    audio = null
                }
            }
            "clipboard" -> app.clipboard.write(e.optString("text"))
            // 控制端复制了文件：放进手机的剪贴板（别的应用读的时候才取），通知里可以保存到下载
            "clipboardFiles" -> e.optJSONArray("items")?.let { ClipFiles.apply(this, e.optLong("key"), it) }
            // 第一次连上服务器：记下它的证书指纹（不重新连接）
            "serverTrust" -> app.settings.learnTrust(e.optString("server"), e.optString("fp"))
            "identity" -> app.settings.learnIdentity(e.optString("key"))
        }
    }

    private fun kickAll() {
        val h = handle
        if (h != 0L) Native.hostCall(h, JSONObject().put("op", "kickAll").toString())
    }

    private fun connectionNotificationId(key: String) = CONNECTION_NOTIFICATION + (key.hashCode() and 0xffff)

    /**
     * 提醒手机前的人有设备连上来了：谁、从哪里、能不能操作、安全码，带"全部断开"。
     * 同一台设备（同一个公钥）一条：断线重连时更新那一条，不再响一次。断开之后通知留着，看得到谁来过
     */
    private fun notifyConnected(v: Viewer) {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && nm.getNotificationChannel(CONNECTION_CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CONNECTION_CHANNEL, getString(R.string.connections_channel), NotificationManager.IMPORTANCE_HIGH),
            )
        }
        val open = PendingIntent.getActivity(
            this, 2,
            Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_TAB, MainActivity.TAB_SHARE)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val kick = PendingIntent.getService(
            this, 3, Intent(this, HostService::class.java).setAction(ACTION_KICK_ALL),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val name = v.name.ifBlank { getString(R.string.unknown_device) }
        val what = getString(if (InputService.isEnabled(this)) R.string.connected_can_control else R.string.connected_can_view)
        // 安全码：控制端显示的是同一个，本机用户可以和对方对一下（见 airhand_proto::handshake）
        val text = getString(R.string.connected_text, what, v.ip, v.safety)
        val b = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, CONNECTION_CHANNEL)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this).setPriority(Notification.PRIORITY_HIGH).setDefaults(Notification.DEFAULT_ALL)
        }
        val n = b.setSmallIcon(R.drawable.ic_stat_airhand)
            .setContentTitle(getString(if (v.justPaired) R.string.connected_paired_title else R.string.connected_title, name))
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setCategory(Notification.CATEGORY_STATUS)
            .setShowWhen(true)
            .setAutoCancel(true)
            .setOnlyAlertOnce(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.share_kick_all), kick).build())
            .build()
        nm.notify(connectionNotificationId(v.key), n)
    }
    /** 有人在看时保持屏幕亮着（息屏后录不到画面） */
    @Suppress("DEPRECATION")
    private fun acquireWakeLock() {
        if (wakeLock?.isHeld == true) return
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(
            PowerManager.SCREEN_DIM_WAKE_LOCK or PowerManager.ACQUIRE_CAUSES_WAKEUP,
            "airhand:viewers",
        ).apply { acquire(4 * 60 * 60 * 1000L) }
    }

    private fun releaseWakeLock() {
        wakeLock?.takeIf { it.isHeld }?.release()
        wakeLock = null
    }

    private fun stopSharing(reason: String?) {
        if (instance === this) instance = null
        stopNative()
        capture?.release()
        capture = null
        projection?.stop()
        projection = null
        scope?.cancel()
        scope = null
        HostState.update { HostStatus(running = false, stoppedReason = reason) }
        @Suppress("DEPRECATION")
        stopForeground(true)
        stopSelf()
        Log.i(TAG, "screen sharing stopped${reason?.let { ": $it" } ?: ""}")
    }

    override fun onDestroy() {
        if (projection != null) stopSharing(null)
        super.onDestroy()
    }

    // ───────────────────────────── 通知 ─────────────────────────────

    private fun startForegroundCompat(n: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIFICATION_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(NOTIFICATION_ID, n)
        }
    }

    private fun updateNotification() {
        val st = HostState.value
        if (!st.running) return
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.notify(NOTIFICATION_ID, notification(st.code, st.viewers.size))
    }

    private fun notification(code: String?, viewers: Int): Notification {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && nm.getNotificationChannel(CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL, getString(R.string.notification_channel), NotificationManager.IMPORTANCE_LOW).apply {
                    setShowBadge(false)
                },
            )
        }
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_TAB, MainActivity.TAB_SHARE)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val stop = PendingIntent.getService(
            this, 1, Intent(this, HostService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val title = when {
            viewers > 0 -> resources.getQuantityString(R.plurals.notification_viewers, viewers, viewers)
            else -> getString(R.string.notification_sharing)
        }
        val text = code?.let { getString(R.string.notification_code, Code.format(it)) } ?: getString(R.string.notification_connecting)
        val b = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) Notification.Builder(this, CHANNEL) else
            @Suppress("DEPRECATION") Notification.Builder(this)
        return b.setSmallIcon(R.drawable.ic_stat_airhand)
            .setContentTitle(title)
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.share_stop), stop).build())
            .build()
    }

    companion object {
        private const val TAG = "airhand"
        private const val ACTION_START = "app.airhand.android.START"
        private const val ACTION_STOP = "app.airhand.android.STOP"
        private const val ACTION_KICK_ALL = "app.airhand.android.KICK_ALL"
        private const val CONNECTION_CHANNEL = "connections"
        private const val CONNECTION_NOTIFICATION = 1000
        private const val EXTRA_RESULT_CODE = "resultCode"
        private const val EXTRA_DATA = "data"
        private const val CHANNEL = "sharing"
        private const val NOTIFICATION_ID = 1

        @Volatile
        private var instance: HostService? = null

        /** 用户同意了屏幕录制：开始共享 */
        fun start(context: Context, resultCode: Int, data: Intent) {
            val i = Intent(context, HostService::class.java)
                .setAction(ACTION_START)
                .putExtra(EXTRA_RESULT_CODE, resultCode)
                .putExtra(EXTRA_DATA, data)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) context.startForegroundService(i) else context.startService(i)
        }

        fun stop(context: Context) {
            instance?.stopSharing(null) ?: run {
                HostState.update { it.copy(running = false) }
            }
        }

        fun kickAll() {
            instance?.kickAll()
        }

        /** 换一个配对码（在共享时才有配对码可换） */
        fun newPairingCode() {
            val h = instance?.handle ?: return
            if (h != 0L) Native.hostCall(h, JSONObject().put("op", "newPairingCode").toString())
        }

        /** 忘掉配对过的控制端。没在共享时返回 false：调用方直接改保存的（[AirhandApp.forgetPaired]） */
        fun forgetPaired(): Boolean {
            val h = instance?.handle ?: return false
            if (h == 0L) return false
            Native.hostCall(h, JSONObject().put("op", "forgetPaired").toString())
            return true
        }

        fun newCode() {
            val h = instance?.handle ?: return
            if (h != 0L) Native.hostCall(h, JSONObject().put("op", "newCode").toString())
        }

        fun restartIfRunning(context: Context) {
            instance?.restartNative()
        }

        /** 现在能不能把手机上的文件放进正在控制本机的设备的剪贴板（在共享、有人连着） */
        fun canOfferFiles() = (instance?.handle ?: 0L) != 0L && HostState.value.viewers.isNotEmpty()

        /**
         * 手机上分享给 Airhand 的文件（[ClipFiles.offer]）：放进正在控制本机的设备的剪贴板。文件描述符不管成不成都归这里：
         * 交给 Rust 就由 Rust 关（没人连着时也是），没在共享时在这里关
         */
        fun offerFiles(files: JSONArray): Boolean {
            val h = instance?.handle ?: 0L
            if (h == 0L) {
                ClipFiles.closeFds(files)
                return false
            }
            Native.hostCall(h, JSONObject().put("op", "offerFiles").put("files", files).toString())
            return HostState.value.viewers.isNotEmpty()
        }

        /** 手机自己复制了文本（只有应用在前台时读得到）：发给正在控制本机的设备 */
        fun clipboardChanged(text: String) {
            val h = instance?.handle ?: return
            if (h != 0L && text.isNotEmpty()) Native.hostCall(h, JSONObject().put("op", "clipboard").put("text", text).toString())
        }
    }
}
