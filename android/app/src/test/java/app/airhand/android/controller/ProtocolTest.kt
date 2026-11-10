package app.airhand.android.controller

import app.airhand.android.core.Codec
import app.airhand.android.core.Quality
import app.airhand.android.core.SystemAction
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * 和 Rust、桌面 / 网页控制台共用的协议样例（`crates/airhand-link/tests/protocol.json`，由 Rust 的测试生成）：
 * 媒体包按 [Media] 解析出来要和 Rust 打包时一样；[Input]、[Commands] 拼出来的 JSON 要和 Rust 认的一样。
 */
class ProtocolTest {
    private val repo = File(System.getProperty("airhand.repo") ?: "../..")
    private val fixture = JSONObject(repo.resolve("crates/airhand-link/tests/protocol.json").readText())

    private fun bytes(hex: String) = ByteArray(hex.length / 2) { hex.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
    private fun hex(b: ByteArray, from: Int) = b.drop(from).joinToString("") { "%02x".format(it) }

    /** JSON 值相等（数字按数值比较：org.json 解析出来的类型不一定和拼的时候一样） */
    private fun same(a: Any?, b: Any?): Boolean = when {
        a is JSONObject && b is JSONObject -> a.keySet() == b.keySet() && a.keySet().all { same(a.get(it), b.get(it)) }
        a is JSONArray && b is JSONArray -> a.length() == b.length() && (0 until a.length()).all { same(a.get(it), b.get(it)) }
        a is Number && b is Number -> a.toDouble() == b.toDouble()
        else -> a == b
    }

    private fun assertSame(want: Any?, have: Any?) = assertTrue("want $want\nhave $have", same(want, have))

    @Test
    fun mediaPacketsParse() {
        val media = fixture.getJSONArray("media")
        assertTrue(media.length() >= 7)
        // 有一个包的头比 24 字节长（以后的版本加了字段）：负载从头里写的长度处开始
        assertTrue((0 until media.length()).any { Media.headerLen(bytes(media.getJSONObject(it).getString("hex"))) > Media.HEADER })
        for (i in 0 until media.length()) {
            val m = media.getJSONObject(i)
            val name = m.getString("name")
            val b = bytes(m.getString("hex"))
            assertEquals(name, m.getInt("kind"), Media.kind(b))
            assertEquals(name, m.getString("payload"), hex(b, Media.payload(b)))
            when (Media.kind(b)) {
                Media.VIDEO -> assertEquals(
                    name,
                    Media.Video(
                        m.getInt("seq"), m.getBoolean("key"), Codec.fromWire(m.getString("codec"))!!,
                        m.getInt("width"), m.getInt("height"), m.getLong("captureUs"),
                    ),
                    Media.video(b),
                )
                Media.AUDIO -> assertEquals(name, Media.Audio(m.getInt("seq"), m.getBoolean("start"), m.getLong("captureUs")), Media.audio(b))
                else -> {
                    val s = m.optJSONObject("shape")
                    val shape = s?.let { Media.Shape(it.getInt("width"), it.getInt("height"), it.getInt("hotX"), it.getInt("hotY")) }
                    assertEquals(
                        name,
                        Media.Cursor(m.getBoolean("visible"), shape, m.getInt("slot"), m.getBoolean("cached")),
                        Media.cursor(b),
                    )
                }
            }
        }
        assertEquals(0, Media.kind(ByteArray(10)))
        // 不认识的编码格式：不猜成 H.264
        val first = bytes(media.getJSONObject(0).getString("hex"))
        first[2] = 9
        assertEquals(null, Media.video(first))
        assertEquals(media.getJSONObject(0).getInt("seq"), Media.seq(first))
        // 头的长度不对（比 24 短、比包还长）：不是合法的包
        for (bad in listOf(23, 200)) {
            val p = bytes(media.getJSONObject(0).getString("hex"))
            p[3] = bad.toByte()
            assertEquals("头的长度 $bad", 0, Media.kind(p))
        }
    }

    @Test
    fun inputEventsMatchRust() {
        val want = fixture.getJSONArray("input")
        val have = listOf(
            Input.move(0.5f, 0.25f),
            Input.delta(-3, 7),
            Input.button(Input.RIGHT, true),
            Input.wheel(0, -120),
            Input.key("KeyA", false),
            Input.text("你好"),
            Input.releaseAll(),
            Input.touch(1, Input.DOWN, 0.25f, 0.75f),
            Input.system(SystemAction.BACK),
        )
        assertEquals(want.length(), have.size)
        for (i in have.indices) assertSame(want.get(i), have[i])
    }

    @Test
    fun commandsMatchRust() {
        val want = (0 until fixture.getJSONArray("commands").length()).map { fixture.getJSONArray("commands").getJSONObject(it) }
            .associateBy { it.getString("cmd") }
        val events = JSONArray().put(Input.delta(1, 2)).put(Input.releaseAll())
        assertSame(want["input"], Commands.input(3, events))
        assertSame(want["settings"], Commands.settings(3, Quality(60, 8000, 1080, true, Codec.H265)))
        assertSame(want["keyframe"], Commands.keyframe(3))
        assertSame(want["display"], Commands.display(3, 1))
        assertSame(want["audio"], Commands.audio(3, true))
        assertSame(want["clipboard"], Commands.clipboard(3, "hi"))
        assertSame(want["disconnect"], Commands.disconnect(3))
        // Android 不用的：画面确认走 Native.ctlAck，剪贴板由自己同步
        assertEquals(setOf("ack", "clipboardSync"), want.keys - setOf("input", "settings", "keyframe", "display", "audio", "clipboard", "disconnect"))
    }

    /** connect：控制码加上连接选项（和桌面版、网页版一样的 ConnectOptions） */
    @Test
    fun connectOptionsMatchRust() {
        val want = fixture.getJSONObject("connectOptions")
        val resume = Resume.fromJson(want.getJSONArray("resume"))
        val have = Commands.options(resume, Quality(60, 8000, 1080, true, Codec.H265), "ab".repeat(32), "482913", true)
        assertSame(want, have)
        val cmd = Commands.connect("123 456", resume, Quality(60, 8000, 1080, true, Codec.H265), "ab".repeat(32), "482913", true)
        assertEquals("connect", cmd.getString("cmd"))
        assertEquals("123 456", cmd.getString("code"))
        assertSame(want, JSONObject(cmd.toString()).apply { remove("cmd"); remove("code") })
    }

    @Test
    fun movesCoalesceWithoutCrossingClicks() {
        val pending = ArrayList<Any>()
        for (e in listOf(
            Input.move(0.1f, 0.1f), Input.move(0.2f, 0.3f),
            Input.delta(1, 2), Input.delta(3, -1), Input.button(Input.RIGHT, false), Input.delta(1, 1),
        )) {
            Input.append(pending, e)
        }
        assertSame(
            JSONArray().put(Input.move(0.2f, 0.3f)).put(Input.delta(4, 1)).put(Input.button(Input.RIGHT, false)).put(Input.delta(1, 1)),
            JSONArray(pending),
        )
    }

    @Test
    fun resumeTokenRoundtrip() {
        val hex = "000102030405060708090a0b0c0d0eff"
        val json = Resume.toJson(hex) as JSONArray
        assertEquals(16, json.length())
        assertEquals(255, json.getInt(15))
        assertEquals(hex, Resume.fromJson(json))
        assertEquals(JSONObject.NULL, Resume.toJson(null))
        assertEquals(JSONObject.NULL, Resume.toJson("12"))
        assertNull(Resume.fromJson(JSONArray().put(1)))
    }
}
