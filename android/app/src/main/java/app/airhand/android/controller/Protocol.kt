package app.airhand.android.controller

import app.airhand.android.core.Codec
import app.airhand.android.core.Quality
import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * 媒体包：画面、光标、声音（格式见 `crates/airhand-proto/src/media.rs`；被控端打包，Rust 核心原样交过来）。
 * 样例在 `crates/airhand-link/tests/protocol.json`，ProtocolTest 检查这里的解析和它一致。
 *
 * 24 字节的头（小端）+ 数据：
 * `0 类型 u8（1 画面 / 2 光标 / 3 声音）| 1 标志 u8 | 2 编码格式 u8（光标：槽位）| 4 序号 u32 | 8 宽 u16 | 10 高 u16 |
 *  12 热点 x u16 | 14 热点 y u16 | 16 采集时间 u64（微秒，被控端时钟）`
 *
 * 光标形状按槽位缓存（动画光标反复用到的那几帧只传一次）：标志 bit1 = 新形状（RGBA 在头后面），存进槽位并换成它；
 * bit2 = 换成槽位里已有的形状；都没有 = 形状没变。槽位只在一个会话里有效。
 */
object Media {
    const val HEADER = 24
    const val VIDEO = 1
    const val CURSOR = 2
    const val AUDIO = 3

    data class Video(val seq: Int, val key: Boolean, val codec: Codec, val width: Int, val height: Int, val captureUs: Long)
    data class Audio(val seq: Int, val start: Boolean, val captureUs: Long)
    data class Shape(val width: Int, val height: Int, val hotX: Int, val hotY: Int)

    /**
     * 光标。`shape`：新的形状（RGBA 跟在头后面），存进 `slot` 并换成它；`cached`：换成 `slot` 里已有的形状；
     * 都没有表示形状没变
     */
    data class Cursor(val visible: Boolean, val shape: Shape?, val slot: Int = 0, val cached: Boolean = false)

    /** 包的类型；不到一个头长时为 0 */
    fun kind(b: ByteArray): Int = if (b.size < HEADER) 0 else b[0].toInt() and 0xff

    private fun le(b: ByteArray): ByteBuffer = ByteBuffer.wrap(b).order(ByteOrder.LITTLE_ENDIAN)
    private fun ByteBuffer.u16(i: Int) = getShort(i).toInt() and 0xffff

    /** 画面包的序号（编码格式不认识时也要确认） */
    fun seq(b: ByteArray): Int = le(b).getInt(4)

    /** 画面包的头；编码格式不认识时为 null（和 Rust、桌面控制台一样丢掉这一帧，不猜成 H.264） */
    fun video(b: ByteArray): Video? {
        val bb = le(b)
        return Video(
            seq = bb.getInt(4),
            key = b[1].toInt() and 1 != 0,
            codec = Codec.fromId(b[2].toInt() and 0xff) ?: return null,
            width = bb.u16(8),
            height = bb.u16(10),
            captureUs = bb.getLong(16),
        )
    }

    fun audio(b: ByteArray): Audio {
        val bb = le(b)
        return Audio(seq = bb.getInt(4), start = b[1].toInt() and 1 != 0, captureUs = bb.getLong(16))
    }

    fun cursor(b: ByteArray): Cursor {
        val bb = le(b)
        val flags = b[1].toInt()
        val shape = if (flags and 2 != 0) Shape(bb.u16(8), bb.u16(10), bb.u16(12), bb.u16(14)) else null
        return Cursor(visible = flags and 1 != 0, shape = shape, slot = b[2].toInt() and 0xff, cached = flags and 4 != 0)
    }
}

/**
 * 发给 Rust 核心的命令（`Native.ctlCall`）：`connect` 加上和桌面版、网页版一样的会话命令
 * （Rust 的 `airhand_link::controller::Command`，JSON 以 `cmd` 区分）。画面的确认走 `Native.ctlAck`，不经过 JSON。
 */
object Commands {
    private fun cmd(name: String, sid: Int): JSONObject = JSONObject().put("cmd", name).put("sid", sid)

    /** `resume`：之前连上时得到的重连凭证（十六进制，见 [Resume]） */
    fun connect(code: String, resume: String?, settings: Quality): JSONObject = JSONObject()
        .put("cmd", "connect")
        .put("code", code)
        .put("resume", Resume.toJson(resume))
        .put("settings", settings.toJson())

    fun input(sid: Int, events: JSONArray): JSONObject = cmd("input", sid).put("events", events)
    fun settings(sid: Int, q: Quality): JSONObject = cmd("settings", sid).put("settings", q.toJson())
    fun keyframe(sid: Int): JSONObject = cmd("keyframe", sid)
    fun display(sid: Int, index: Int): JSONObject = cmd("display", sid).put("index", index)
    fun audio(sid: Int, enabled: Boolean): JSONObject = cmd("audio", sid).put("enabled", enabled)
    fun clipboard(sid: Int, text: String): JSONObject = cmd("clipboard", sid).put("text", text)
    fun disconnect(sid: Int): JSONObject = cmd("disconnect", sid)
}

/** 重连凭证：Rust 那边是 16 个字节（JSON 数组），这里存成十六进制字符串（"我的设备"里） */
object Resume {
    fun toJson(hex: String?): Any {
        if (hex == null || hex.length != 32) return JSONObject.NULL
        val a = JSONArray()
        for (i in 0 until 16) a.put(hex.substring(i * 2, i * 2 + 2).toIntOrNull(16) ?: return JSONObject.NULL)
        return a
    }

    fun fromJson(a: JSONArray?): String? {
        if (a == null || a.length() != 16) return null
        return (0 until 16).joinToString("") { "%02x".format(a.optInt(it) and 0xff) }
    }
}
