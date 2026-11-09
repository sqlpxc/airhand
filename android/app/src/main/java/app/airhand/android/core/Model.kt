package app.airhand.android.core

import org.json.JSONArray
import org.json.JSONObject

/** 视频编码格式，与 Rust 的 `airhand_proto::session::Codec` 对应 */
enum class Codec(val id: Int, val wire: String, val mime: String, val label: String) {
    H264(0, "H264", "video/avc", "H.264"),
    H265(1, "H265", "video/hevc", "H.265 / HEVC"),
    AV1(2, "AV1", "video/av01", "AV1");

    companion object {
        fun fromWire(s: String?): Codec? = entries.firstOrNull { it.wire == s }
        /** 媒体包头里的编号；不认识的格式（对方版本更新）为 null，不猜成 H.264 */
        fun fromId(id: Int): Codec? = entries.firstOrNull { it.id == id }

        /** 压缩率从高到低（与桌面控制台相同：AV1 > HEVC > H.264） */
        val PRIORITY = listOf(AV1, H265, H264)
    }
}

/** 画质设置，与 `QualitySettings` 对应（JSON 字段名是 snake_case） */
data class Quality(
    val fps: Int,
    val kbps: Int,
    val maxShort: Int,
    val adaptive: Boolean = true,
    val codec: Codec = Codec.H264,
) {
    fun toJson(): JSONObject = JSONObject()
        .put("fps", fps)
        .put("bitrate_kbps", kbps)
        .put("max_short", maxShort)
        .put("adaptive", adaptive)
        .put("codec", codec.wire)
}

/** 画质预设：与桌面控制台（client/src/lib/presets.ts）相同 */
enum class Preset(val id: String, val fps: Int, val kbps: Int, val maxShort: Int) {
    SAVER("saver", 24, 1200, 720),
    BALANCED("balanced", 30, 4000, 1080),
    SMOOTH("smooth", 60, 8000, 1080),
    HD("hd", 60, 16000, 1440),
    NATIVE("native", 60, 30000, 0);

    fun quality(codec: Codec) = Quality(fps, kbps, maxShort, true, codec)

    companion object {
        val DEFAULT = BALANCED
        fun of(id: String?) = entries.firstOrNull { it.id == id } ?: DEFAULT
        fun match(q: Quality) = entries.firstOrNull { it.fps == q.fps && it.kbps == q.kbps && it.maxShort == q.maxShort }
    }
}

/**
 * 显示给用户的问题，与 Rust 的 `airhand_link::Problem` 对应（JSON 以 kind 区分）。
 * 文字由 [Messages] 按本机语言生成。
 */
data class Problem(
    val kind: String,
    val detail: String? = null,
    val server: Int? = null,
    val client: Int? = null,
    /** peerIncompatible：对方和本机的会话版本（对方的大就是本机该更新） */
    val peer: Int? = null,
    val ours: Int? = null,
) {
    /** 对方有意结束，或者要用户来决定（对方的身份变了、要配对码、版本不兼容），不应该自动重连 */
    val isFinal get() = kind in FINAL

    /** 端到端握手的问题：要用户输入配对码 */
    val needsPairing get() = kind == "pairingRequired" || kind == "pairingFailed" || kind == "pairingUnavailable"

    companion object {
        private val FINAL = setOf(
            "kicked", "codeChanged", "hostKeyChanged", "pairingRequired", "pairingFailed", "pairingUnavailable", "peerIncompatible",
        )

        fun parse(o: JSONObject?): Problem = if (o == null) Problem("failed", "?") else Problem(
            kind = o.optString("kind", "failed"),
            detail = o.optStringOrNull("detail"),
            server = if (o.has("server")) o.optInt("server") else null,
            client = if (o.has("client")) o.optInt("client") else null,
            peer = if (o.has("peer")) o.optInt("peer") else null,
            ours = if (o.has("ours")) o.optInt("ours") else null,
        )
    }
}

/** 与中转服务器的连接状态 */
sealed interface ServerState {
    data object Connecting : ServerState
    data object Connected : ServerState
    data class Disconnected(val reason: Problem) : ServerState

    companion object {
        fun parse(o: JSONObject): ServerState = when (o.optString("state")) {
            "connected" -> Connected
            "disconnected" -> Disconnected(Problem.parse(o.optJSONObject("reason")))
            else -> Connecting
        }
    }
}

data class DisplayInfo(val index: Int, val name: String, val width: Int, val height: Int, val primary: Boolean) {
    fun toJson(): JSONObject = JSONObject()
        .put("index", index).put("name", name).put("x", 0).put("y", 0)
        .put("width", width).put("height", height).put("primary", primary)

    companion object {
        fun parse(o: JSONObject) = DisplayInfo(
            o.optInt("index"), o.optString("name"), o.optInt("width"), o.optInt("height"), o.optBoolean("primary"),
        )

        fun parseList(a: JSONArray?): List<DisplayInfo> =
            if (a == null) emptyList() else (0 until a.length()).map { parse(a.getJSONObject(it)) }
    }
}

/** 被控端会话开始时的信息（`SessionEvent::Hello`） */
data class HostHello(
    val hostname: String,
    val os: String,
    val version: String,
    val displays: List<DisplayInfo>,
    val current: Int,
    val codecs: List<Codec>,
    val platform: OsKind,
    /** 能接收多点触摸 */
    val touch: Boolean,
    /** 支持的系统操作（返回、主页……） */
    val actions: List<SystemAction>,
)

/** 系统操作，与 Rust 的 `airhand_proto::session::SystemAction` 对应（JSON 里是变体名） */
enum class SystemAction(val wire: String) {
    BACK("Back"),
    HOME("Home"),
    RECENTS("Recents"),
    NOTIFICATIONS("Notifications"),
    QUICK_SETTINGS("QuickSettings"),
    SCREENSHOT("Screenshot"),
    LOCK_SCREEN("LockScreen"),
    POWER_MENU("PowerMenu"),
    VOLUME_UP("VolumeUp"),
    VOLUME_DOWN("VolumeDown");

    companion object {
        fun fromWire(s: String?) = entries.firstOrNull { it.wire == s }
    }
}

data class EncoderInfo(val codec: Codec, val name: String, val hardware: Boolean, val width: Int, val height: Int)

/** 被控端发来的提示（`NoticeView`） */
data class Notice(val kind: String, val detail: String? = null, val screen: Boolean = false, val input: Boolean = false)

data class Stats(val fps: Float, val kbps: Float, val rttMs: Float?, val width: Int, val height: Int)

/**
 * 被控端是哪种系统：决定默认的操作方式、快捷键和导航键。
 * 连上之后以被控端报告的平台（`HostHello.platform`）为准；连上之前（例如"我的设备"里的图标）按系统名称猜
 */
enum class OsKind(val wire: String) {
    WINDOWS("Windows"), MAC("Mac"), LINUX("Linux"), ANDROID("Android"), UNKNOWN("");

    companion object {
        fun fromWire(s: String?): OsKind = entries.firstOrNull { it.wire == s && s.isNotEmpty() } ?: UNKNOWN

        fun of(os: String?): OsKind {
            val s = os?.lowercase() ?: return UNKNOWN
            return when {
                s.startsWith("android") -> ANDROID
                s.contains("windows") -> WINDOWS
                s.contains("mac") || s.contains("os x") -> MAC
                s.isNotBlank() -> LINUX
                else -> UNKNOWN
            }
        }
    }
}

object Code {
    /** 用户输入的控制码 → 6 位数字（允许空格 / 短横线），不合法返回 null。规则与 `airhand_proto::parse_code` 相同 */
    fun parse(input: String): String? {
        val digits = input.filter { !it.isWhitespace() && it != '-' }
        if (digits.length != 6 || !digits.all { it in '0'..'9' } || digits[0] == '0') return null
        return digits
    }

    /** 从任意文本里找出一个控制码（例如粘贴的是"控制码：123 456"） */
    fun find(text: String?): String? {
        if (text == null || text.length > 200) return null
        val m = Regex("(?<!\\d)([1-9]\\d{2})[\\s-]?(\\d{3})(?!\\d)").find(text) ?: return null
        return m.groupValues[1] + m.groupValues[2]
    }

    fun format(code: String): String = if (code.length == 6) code.substring(0, 3) + " " + code.substring(3) else code
}

fun JSONObject.optStringOrNull(key: String): String? = if (has(key) && !isNull(key)) optString(key) else null

fun hex(b: ByteArray): String = b.joinToString("") { "%02x".format(it) }
