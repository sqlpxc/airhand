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
import app.airhand.android.core.Code
import app.airhand.android.core.Native
import app.airhand.android.core.Problem
import app.airhand.android.core.ServerState
import app.airhand.android.data.HostCode
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
            ACTION_APPROVE -> intent.getStringExtra(EXTRA_ID)?.let { approve(it, intent.getBooleanExtra(EXTRA_ALLOW, false)) }
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
            .put("approval", s.requireApproval)
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
                val all = (0 until list.length()).map { list.getJSONObject(it) }
                fun pick(key: String) = all.filter { it.optBoolean(key) }.map { Viewer(it.optString("id"), it.optString("name"), it.optString("ip")) }
                val viewers = pick("active")
                val requests = pick("pending")
                HostState.update { it.copy(viewers = viewers, requests = requests) }
                if (viewers.isEmpty()) releaseWakeLock() else acquireWakeLock()
                updateNotification()
                showRequests(requests)
            }
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
            // 第一次连上服务器：记下它的证书指纹（不重新连接）
            "serverTrust" -> app.settings.learnTrust(e.optString("server"), e.optString("fp"))
        }
    }

    /** 回答连接请求 */
    private fun approve(id: String, allow: Boolean) {
        val h = handle
        if (h != 0L) Native.hostCall(h, JSONObject().put("op", "approve").put("id", id).put("allow", allow).toString())
        // 先撤下通知（Rust 那边处理完会再发一次列表）
        cancelRequest(id)
    }

    /** 连接请求的通知：每个请求一条，带"拒绝 / 允许"；处理掉（或者对方走了）就撤下 */
    private val shownRequests = HashSet<String>()

    private fun showRequests(requests: List<Viewer>) {
        val ids = requests.map { it.id }.toSet()
        for (id in shownRequests - ids) cancelRequest(id)
        for (r in requests) if (r.id !in shownRequests) postRequest(r)
    }

    private fun requestNotificationId(id: String) = REQUEST_NOTIFICATION + (id.hashCode() and 0xffff)

    private fun postRequest(r: Viewer) {
        shownRequests += r.id
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && nm.getNotificationChannel(REQUEST_CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(REQUEST_CHANNEL, getString(R.string.request_channel), NotificationManager.IMPORTANCE_HIGH),
            )
        }
        fun answer(allow: Boolean) = PendingIntent.getService(
            this, (r.id.hashCode() shl 1) + if (allow) 1 else 0,
            Intent(this, HostService::class.java).setAction(ACTION_APPROVE).putExtra(EXTRA_ID, r.id).putExtra(EXTRA_ALLOW, allow),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val open = PendingIntent.getActivity(
            this, 2,
            Intent(this, MainActivity::class.java).putExtra(MainActivity.EXTRA_TAB, MainActivity.TAB_SHARE)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val name = r.name.ifBlank { getString(R.string.unknown_device) }
        val what = getString(if (InputService.isEnabled(this)) R.string.request_wants_control else R.string.request_wants_view)
        val text = getString(R.string.request_text, what, r.ip, REQUEST_TIMEOUT_S)
        val b = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, REQUEST_CHANNEL)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this).setPriority(Notification.PRIORITY_HIGH).setDefaults(Notification.DEFAULT_ALL)
        }
        val n = b.setSmallIcon(R.drawable.ic_stat_airhand)
            .setContentTitle(getString(R.string.request_title, name))
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setCategory(Notification.CATEGORY_CALL)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.request_decline), answer(false)).build())
            .addAction(Notification.Action.Builder(null, getString(R.string.request_allow), answer(true)).build())
            .build()
        nm.notify(requestNotificationId(r.id), n)
    }

    private fun cancelRequest(id: String) {
        shownRequests -= id
        (getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).cancel(requestNotificationId(id))
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
        for (id in shownRequests.toList()) cancelRequest(id)
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
        private const val ACTION_APPROVE = "app.airhand.android.APPROVE"
        private const val EXTRA_ID = "id"
        private const val EXTRA_ALLOW = "allow"
        private const val REQUEST_CHANNEL = "requests"
        private const val REQUEST_NOTIFICATION = 1000

        /** 本机用户这么久没回答就当作拒绝（与 Rust 的 APPROVAL_TIMEOUT 相同） */
        const val REQUEST_TIMEOUT_S = 60
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

        /** 回答连接请求（界面上的"允许 / 拒绝"） */
        fun approve(id: String, allow: Boolean) {
            instance?.approve(id, allow)
        }

        fun setApproval(required: Boolean) {
            val h = instance?.handle ?: return
            if (h != 0L) Native.hostCall(h, JSONObject().put("op", "approval").put("required", required).toString())
        }

        fun kickAll() {
            val h = instance?.handle ?: return
            if (h != 0L) Native.hostCall(h, JSONObject().put("op", "kickAll").toString())
        }

        fun newCode() {
            val h = instance?.handle ?: return
            if (h != 0L) Native.hostCall(h, JSONObject().put("op", "newCode").toString())
        }

        fun restartIfRunning(context: Context) {
            instance?.restartNative()
        }

        /** 手机自己复制了文本（只有应用在前台时读得到）：发给正在控制本机的设备 */
        fun clipboardChanged(text: String) {
            val h = instance?.handle ?: return
            if (h != 0L && text.isNotEmpty()) Native.hostCall(h, JSONObject().put("op", "clipboard").put("text", text).toString())
        }
    }
}
