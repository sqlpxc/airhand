package app.airhand.android.controller

import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import app.airhand.android.core.Native
import app.airhand.android.core.Problem
import app.airhand.android.core.ServerState
import app.airhand.android.data.SettingsStore
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONObject
import kotlin.concurrent.thread

/**
 * 控制端与中转服务器的连接（Rust 的 `ControllerService`）：应用在前台、或者有会话时保持连接，
 * 掉线后 Rust 那边自动重连。应用退到后台、也没有会话一段时间后断开，省电。
 */
class ControllerClient(private val settings: SettingsStore) {
    private val lock = Any()

    @Volatile
    private var handle = 0L
    private var foreground = false
    private var users = 0
    private val main = Handler(Looper.getMainLooper())
    private val stopLater = Runnable {
        synchronized(lock) {
            if (!foreground && users == 0) stopLocked()
        }
    }

    private val _server = MutableStateFlow<ServerState>(ServerState.Connecting)
    val server: StateFlow<ServerState> = _server.asStateFlow()

    private val _hostOnline = MutableSharedFlow<String>(extraBufferCapacity = 16)

    /** 在等的被控端（6 位控制码）重新上线了 */
    val hostOnline: SharedFlow<String> = _hostOnline.asSharedFlow()

    /** 应用进入 / 离开前台 */
    fun setForeground(fg: Boolean) = synchronized(lock) {
        foreground = fg
        if (fg) {
            main.removeCallbacks(stopLater)
            startLocked()
        } else if (users == 0) {
            main.removeCallbacks(stopLater)
            main.postDelayed(stopLater, IDLE_STOP_MS)
        }
    }

    /** 会话开始用这条连接（会话结束时 [release]） */
    fun acquire() = synchronized(lock) {
        users++
        main.removeCallbacks(stopLater)
        startLocked()
    }

    fun release() = synchronized(lock) {
        users = (users - 1).coerceAtLeast(0)
        if (users == 0 && !foreground) {
            main.removeCallbacks(stopLater)
            main.postDelayed(stopLater, IDLE_STOP_MS)
        }
    }

    /** 服务器设置变了：用新设置重新连接（正在进行的会话会断开） */
    fun restart() = synchronized(lock) {
        stopLocked()
        if (foreground || users > 0) startLocked()
    }

    private fun startLocked() {
        if (handle != 0L) return
        val s = settings.value
        val cfg = JSONObject()
            .put("server", s.server)
            .put("fingerprint", s.fingerprint)
            .put("trust", s.trustJson())
            .put("name", settings.deviceName())
            .put("os", osName())
            // 本机的端到端加密身份：配对过的电脑靠它认出本机
            .put("identity", settings.identity())
        val h = try {
            Native.ctlStart(cfg.toString())
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "can't start the controller: ${e.message}")
            _server.value = ServerState.Disconnected(Problem("failed", e.message))
            return
        }
        handle = h
        _server.value = ServerState.Connecting
        thread(name = "airhand-ctl-events", isDaemon = true) { pump(h) }
    }

    private fun stopLocked() {
        val h = handle
        if (h == 0L) return
        handle = 0L
        Native.ctlStop(h)
        _server.value = ServerState.Connecting
    }

    private fun pump(h: Long) {
        while (handle == h) {
            val text = Native.ctlPollEvent(h, 500) ?: continue
            val e = runCatching { JSONObject(text) }.getOrNull() ?: continue
            when (e.optString("type")) {
                "server" -> if (handle == h) _server.value = ServerState.parse(e.getJSONObject("state"))
                "hostOnline" -> _hostOnline.tryEmit(e.optInt("code").toString())
                // 第一次连上服务器：记下它的证书指纹（不重新连接）
                "serverTrust" -> settings.learnTrust(e.optString("server"), e.optString("fp"))
                "identity" -> settings.learnIdentity(e.optString("key"))
            }
        }
    }

    /** 当前的句柄，没有连接（例如设置不对）时为 0。会话的命令见 [Commands] */
    fun handle(): Long = handle

    companion object {
        private const val TAG = "airhand"
        private const val IDLE_STOP_MS = 60_000L

        fun osName(): String = "Android ${Build.VERSION.RELEASE}"
    }
}
