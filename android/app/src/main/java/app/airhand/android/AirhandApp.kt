package app.airhand.android

import android.app.Application
import android.app.UiModeManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.pm.PackageManager
import android.content.res.Configuration
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import app.airhand.android.controller.ControllerClient
import app.airhand.android.controller.RemoteSession
import app.airhand.android.core.Native
import app.airhand.android.data.DeviceStore
import app.airhand.android.data.HostCodeStore
import app.airhand.android.data.Settings
import app.airhand.android.data.SettingsStore
import app.airhand.android.host.HostService
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

class AirhandApp : Application() {
    lateinit var settings: SettingsStore
        private set
    lateinit var devices: DeviceStore
        private set
    lateinit var hostCodes: HostCodeStore
        private set
    lateinit var controller: ControllerClient
        private set
    lateinit var clipboard: Clipboard
        private set

    private val _session = MutableStateFlow<RemoteSession?>(null)

    /** 当前的远程会话（同一时间只有一个） */
    val session: StateFlow<RemoteSession?> = _session.asStateFlow()

    override fun onCreate() {
        super.onCreate()
        instance = this
        Native.init(if (BuildConfig.DEBUG) "debug,quinn=info,rustls=info" else "info,quinn=warn")
        settings = SettingsStore(this)
        devices = DeviceStore(this)
        hostCodes = HostCodeStore(this)
        controller = ControllerClient(settings)
        clipboard = Clipboard(this)

        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = controller.setForeground(true)
            override fun onStop(owner: LifecycleOwner) = controller.setForeground(false)
        })
    }

    /** 连接一台设备（之前的会话会先关掉） */
    fun startSession(code: String, viewOnly: Boolean): RemoteSession {
        android.util.Log.i("airhand", "connecting to $code${if (viewOnly) " (view only)" else ""}")
        _session.value?.close()
        val s = RemoteSession(this, code, devices.get(code)?.resume, viewOnly)
        _session.value = s
        s.start()
        return s
    }

    fun endSession(s: RemoteSession? = _session.value) {
        s?.close()
        if (s != null && _session.value === s) _session.value = null
    }

    /** 改设置。服务器、证书指纹、本机名称变了要重新连接（清掉第一次连接时记下的指纹也算） */
    fun updateSettings(f: (Settings) -> Settings) {
        val old = settings.value
        settings.update(f)
        val now = settings.value
        val trustReset = old.trustedFp.isNotEmpty() && now.trustedFp.isEmpty()
        if (old.server != now.server || old.fingerprint != now.fingerprint || old.deviceName != now.deviceName || trustReset) {
            endSession()
            controller.restart()
            HostService.restartIfRunning(this)
        } else if (old.requireApproval != now.requireApproval) {
            HostService.setApproval(now.requireApproval)
        }
    }

    companion object {
        lateinit var instance: AirhandApp
            private set

        /** 是不是电视（Android TV / Google TV / 电视盒子） */
        fun isTv(context: Context): Boolean {
            val ui = context.getSystemService(Context.UI_MODE_SERVICE) as UiModeManager
            if (ui.currentModeType == Configuration.UI_MODE_TYPE_TELEVISION) return true
            return context.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK)
        }
    }
}

/** 本机剪贴板（Android 10 起只有在前台时才读得到） */
class Clipboard(context: Context) {
    private val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager

    fun read(): String? = runCatching {
        cm.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
    }.getOrNull()

    fun write(text: String, label: String = "Airhand") {
        runCatching { cm.setPrimaryClip(ClipData.newPlainText(label, text)) }
    }
}
