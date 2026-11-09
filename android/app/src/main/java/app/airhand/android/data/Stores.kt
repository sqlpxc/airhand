package app.airhand.android.data

import android.content.Context
import android.os.Build
import android.provider.Settings as SystemSettings
import android.util.AtomicFile
import android.util.Log
import app.airhand.android.BuildConfig
import app.airhand.android.core.optStringOrNull
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject
import java.io.File

/**
 * 应用的全部数据：都是 JSON 文件，放在应用自己的数据目录（`filesDir`）里。
 * 没有别的地方存东西：卸载时系统把这个目录整个删掉；清单里也关掉了备份和设备迁移。
 */
private object JsonFile {
    fun read(file: File): String? = try {
        if (file.exists()) String(AtomicFile(file).readFully(), Charsets.UTF_8) else null
    } catch (e: Exception) {
        Log.w("airhand", "can't read ${file.name}: $e")
        null
    }

    fun write(file: File, text: String) {
        val af = AtomicFile(file)
        val out = af.startWrite()
        try {
            out.write(text.toByteArray(Charsets.UTF_8))
            af.finishWrite(out)
        } catch (e: Exception) {
            af.failWrite(out)
            Log.w("airhand", "can't write ${file.name}: $e")
        }
    }
}

/** 操作方式：自动（电脑用触控板，手机用触屏）/ 触控板 / 触屏 */
enum class TouchMode(val id: String) {
    AUTO("auto"), TRACKPAD("trackpad"), TOUCH("touch");

    companion object {
        fun of(id: String?) = entries.firstOrNull { it.id == id } ?: AUTO
    }
}

/** 电视上画面怎样放进屏幕：完整显示（比例不同时有黑边）/ 铺满（保持比例，裁掉超出的部分）/ 拉伸（填满，不保持比例） */
enum class ScaleMode(val id: String) {
    FIT("fit"), FILL("fill"), STRETCH("stretch");

    companion object {
        fun of(id: String?) = entries.firstOrNull { it.id == id } ?: FIT
    }
}

data class Settings(
    /** 中转服务器 host:port */
    val server: String = BuildConfig.DEFAULT_SERVER,
    /** 服务器证书指纹，空字符串表示第一次连上时记下服务器的证书（[trustedFp]），之后只认它 */
    val fingerprint: String = BuildConfig.DEFAULT_SERVER_FP,
    /** 没有填指纹时，第一次连上服务器时记下的指纹（Rust 的 serverTrust 事件），和记下它时的服务器地址 */
    val trustedServer: String = "",
    val trustedFp: String = "",
    /** 告诉对方的本机名称，空字符串表示用系统里的设备名称 */
    val deviceName: String = "",
    /** 默认画质预设 */
    val preset: String = "balanced",
    val touchMode: TouchMode = TouchMode.AUTO,
    /** 连接后播放对方的声音 */
    val audio: Boolean = true,
    /** 看视频时让画面与声音对齐，关闭时优先操作延迟。 */
    val avSync: Boolean = false,
    /** 视频编码格式：空字符串表示自动（双方都支持的压缩率最高的格式），否则是 `Codec.wire` */
    val codec: String = "",
    /** 电视上画面的缩放方式 */
    val tvScale: ScaleMode = ScaleMode.FIT,
    /** 控制手机时显示返回 / 主页 / 最近任务按钮 */
    val navKeys: Boolean = true,
    /** 别人连接本机时先请我同意 */
    val requireApproval: Boolean = true,
    /** 已经看过的手势说明（trackpad / touch / tv） */
    val tipsSeen: Set<String> = emptySet(),
) {
    /** 为现在的服务器记下的指纹（换了服务器就不算） */
    val learnedFp: String? get() = trustedFp.takeIf { it.isNotBlank() && trustedServer.trim() == server.trim() }

    /** 传给 Rust 的 `trust`（`airhand_android::ctl::LinkSettings`） */
    fun trustJson(): Any =
        if (trustedFp.isBlank()) JSONObject.NULL else JSONObject().put("server", trustedServer).put("fp", trustedFp)

    fun toJson(): JSONObject = JSONObject()
        .put("server", server)
        .put("fingerprint", fingerprint)
        .put("trustedServer", trustedServer)
        .put("trustedFp", trustedFp)
        .put("deviceName", deviceName)
        .put("preset", preset)
        .put("touchMode", touchMode.id)
        .put("audio", audio)
        .put("avSync", avSync)
        .put("codec", codec)
        .put("tvScale", tvScale.id)
        .put("navKeys", navKeys)
        .put("requireApproval", requireApproval)
        .put("tipsSeen", JSONArray(tipsSeen.toList()))

    companion object {
        fun parse(o: JSONObject): Settings {
            val d = Settings()
            val tips = o.optJSONArray("tipsSeen")
            return Settings(
                server = o.optString("server", d.server).ifBlank { d.server },
                fingerprint = o.optString("fingerprint", d.fingerprint),
                trustedServer = o.optString("trustedServer", ""),
                trustedFp = o.optString("trustedFp", ""),
                deviceName = o.optString("deviceName", ""),
                preset = o.optString("preset", d.preset),
                touchMode = TouchMode.of(o.optStringOrNull("touchMode")),
                audio = o.optBoolean("audio", d.audio),
                avSync = o.optBoolean("avSync", d.avSync),
                codec = o.optString("codec", d.codec),
                tvScale = ScaleMode.of(o.optStringOrNull("tvScale")),
                navKeys = o.optBoolean("navKeys", d.navKeys),
                requireApproval = o.optBoolean("requireApproval", d.requireApproval),
                tipsSeen = if (tips == null) emptySet() else (0 until tips.length()).map { tips.getString(it) }.toSet(),
            )
        }
    }
}

class SettingsStore(context: Context) {
    private val file = File(context.filesDir, "settings.json")
    private val _flow = MutableStateFlow(load())
    val flow: StateFlow<Settings> = _flow.asStateFlow()
    val value: Settings get() = _flow.value

    private val systemName: String = defaultDeviceName(context)

    private fun load(): Settings =
        JsonFile.read(file)?.let { runCatching { Settings.parse(JSONObject(it)) }.getOrNull() } ?: Settings()

    @Synchronized
    fun update(f: (Settings) -> Settings) {
        val next = f(_flow.value)
        if (next == _flow.value) return
        _flow.value = next
        JsonFile.write(file, next.toJson().toString())
    }

    /** 第一次连上服务器时记下的证书指纹（Rust 的 serverTrust 事件）。设置里填了指纹时用不到，也照样记着 */
    fun learnTrust(server: String, fp: String) {
        if (server.isBlank() || fp.isBlank()) return
        update { it.copy(trustedServer = server, trustedFp = fp) }
    }

    /** 告诉对方的本机名称 */
    fun deviceName(): String = value.deviceName.trim().ifEmpty { systemName }

    companion object {
        fun defaultDeviceName(context: Context): String {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N_MR1) {
                runCatching {
                    SystemSettings.Global.getString(context.contentResolver, SystemSettings.Global.DEVICE_NAME)
                }.getOrNull()?.takeIf { it.isNotBlank() }?.let { return it }
            }
            val model = Build.MODEL ?: ""
            val maker = Build.MANUFACTURER ?: ""
            return if (model.startsWith(maker, ignoreCase = true)) model
            else "${maker.replaceFirstChar { it.uppercase() }} $model".trim()
        }
    }
}

/** 连接过的设备 */
data class SavedDevice(
    /** 6 位控制码 */
    val code: String,
    /** 用户起的名字，null 表示用对方的设备名 */
    val alias: String? = null,
    val hostName: String = "",
    val hostOs: String = "",
    /** 重连凭证（32 位十六进制）：对方不在线时可以等它上线，不算猜码 */
    val resume: String? = null,
    val lastUsed: Long = 0,
) {
    fun title(): String = alias?.takeIf { it.isNotBlank() } ?: hostName.takeIf { it.isNotBlank() } ?: ""

    fun toJson(): JSONObject = JSONObject()
        .put("code", code)
        .put("alias", alias ?: JSONObject.NULL)
        .put("hostName", hostName)
        .put("hostOs", hostOs)
        .put("resume", resume ?: JSONObject.NULL)
        .put("lastUsed", lastUsed)

    companion object {
        fun parse(o: JSONObject) = SavedDevice(
            code = o.getString("code"),
            alias = o.optStringOrNull("alias"),
            hostName = o.optString("hostName"),
            hostOs = o.optString("hostOs"),
            resume = o.optStringOrNull("resume"),
            lastUsed = o.optLong("lastUsed"),
        )
    }
}

class DeviceStore(context: Context) {
    private val file = File(context.filesDir, "devices.json")
    private val _flow = MutableStateFlow(load())
    val flow: StateFlow<List<SavedDevice>> = _flow.asStateFlow()

    private fun load(): List<SavedDevice> {
        val text = JsonFile.read(file) ?: return emptyList()
        return runCatching {
            val a = JSONArray(text)
            (0 until a.length()).mapNotNull { runCatching { SavedDevice.parse(a.getJSONObject(it)) }.getOrNull() }
        }.getOrDefault(emptyList()).sortedByDescending { it.lastUsed }
    }

    @Synchronized
    private fun set(list: List<SavedDevice>) {
        val sorted = list.sortedByDescending { it.lastUsed }.take(MAX)
        _flow.value = sorted
        JsonFile.write(file, JSONArray(sorted.map { it.toJson() }).toString())
    }

    fun get(code: String): SavedDevice? = _flow.value.firstOrNull { it.code == code }

    /** 连上了：记下（或者更新）这台设备 */
    @Synchronized
    fun connected(code: String, hostName: String, hostOs: String, resume: String) {
        val old = get(code)
        val d = (old ?: SavedDevice(code)).copy(
            hostName = hostName.ifBlank { old?.hostName ?: "" },
            hostOs = hostOs.ifBlank { old?.hostOs ?: "" },
            resume = resume,
            lastUsed = System.currentTimeMillis(),
        )
        set(_flow.value.filter { it.code != code } + d)
    }

    @Synchronized
    fun rename(code: String, alias: String?) {
        set(_flow.value.map { if (it.code == code) it.copy(alias = alias?.trim()?.ifEmpty { null }) else it })
    }

    @Synchronized
    fun delete(code: String) = set(_flow.value.filter { it.code != code })

    /** 撤销删除 */
    @Synchronized
    fun restore(d: SavedDevice) = set(_flow.value.filter { it.code != d.code } + d)

    @Synchronized
    fun clear() = set(emptyList())

    companion object {
        const val MAX = 50
    }
}

/** 本机作为被控端时服务器分配的控制码和取回它用的密钥 */
data class HostCode(val code: Int, val secret: String)

class HostCodeStore(context: Context) {
    private val file = File(context.filesDir, "host.json")

    @Synchronized
    fun load(): HostCode? = JsonFile.read(file)?.let {
        runCatching {
            val o = JSONObject(it)
            HostCode(o.getInt("code"), o.getString("secret"))
        }.getOrNull()
    }

    @Synchronized
    fun save(c: HostCode?) {
        if (c == null) {
            AtomicFile(file).delete()
        } else {
            JsonFile.write(file, JSONObject().put("code", c.code).put("secret", c.secret).toString())
        }
    }
}
