package app.airhand.android.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream

class TombstoneTest {
    /** 测试里拼 protobuf 用（字段号和类型照 AOSP 的 tombstone.proto） */
    private class Pb {
        private val out = ByteArrayOutputStream()
        fun varint(field: Int, v: Long) = apply { key(field, 0); raw(v) }
        fun fixed64(field: Int, v: Long) = apply { key(field, 1); for (i in 0 until 8) out.write(((v ushr (8 * i)) and 0xff).toInt()) }
        fun fixed32(field: Int, v: Int) = apply { key(field, 5); for (i in 0 until 4) out.write((v ushr (8 * i)) and 0xff) }
        fun string(field: Int, s: String) = bytes(field, s.toByteArray())
        fun message(field: Int, m: Pb) = bytes(field, m.bytes())
        fun bytes(field: Int, b: ByteArray) = apply { key(field, 2); raw(b.size.toLong()); out.write(b) }
        fun bytes(): ByteArray = out.toByteArray()
        private fun key(field: Int, wire: Int) = raw(((field shl 3) or wire).toLong())
        private fun raw(v: Long) {
            var x = v
            while (x and 0x7fL.inv() != 0L) {
                out.write(((x and 0x7f) or 0x80).toInt())
                x = x ushr 7
            }
            out.write(x.toInt())
        }
    }

    private fun frame(relPc: Long, file: String, function: String = "", offset: Long = 0, buildId: String = "", mapOffset: Long = 0) = Pb()
        .varint(1, relPc)
        .varint(2, relPc + 0x7000000000) // pc：不显示
        .varint(3, 0x7fff0000) // sp：不显示
        .apply { if (function.isNotEmpty()) string(4, function) }
        .apply { if (offset != 0L) varint(5, offset) }
        .string(6, file)
        .apply { if (mapOffset != 0L) varint(7, mapOffset) }
        .apply { if (buildId.isNotEmpty()) string(8, buildId) }

    private fun thread(id: Long, name: String, vararg frames: Pb) = Pb()
        .varint(1, id)
        .string(2, name)
        .message(3, Pb().string(1, "x0").varint(2, 0)) // 寄存器：不要
        .apply { frames.forEach { message(4, it) } }

    private fun entry(tid: Long, t: Pb) = Pb().varint(1, tid).message(2, t)

    private val so = "/data/app/~~abc/app.airhand.android-xyz/lib/arm64/libairhand_android.so"

    private fun segv(): Pb = Pb()
        .varint(1, 1) // ARM64
        .string(2, "HUAWEI/ALN-AL00/...:12/...") // 构建指纹：不显示
        .string(4, "2026-11-25 10:23:45.123456789+0800")
        .varint(5, 1234)
        .varint(6, 1250)
        .varint(7, 10234)
        .string(9, "app.airhand.android")
        .message(10, Pb().varint(1, 11).string(2, "SIGSEGV").varint(3, 1).string(4, "SEGV_MAPERR").varint(8, 1).varint(9, 0x10))
        .message(15, Pb().string(1, "null pointer dereference"))
        // 主线程排在前面，崩的是 tid 1250
        .message(16, entry(1234, thread(1234, "main", frame(0x1000, "/system/lib64/libmain_only.so"))))
        .message(16, entry(1250, thread(1250, "airhand-session",
            frame(0x4f2c8, so, buildId = "abc123"),
            frame(0x89c4c, "/apex/com.android.runtime/lib64/bionic/libc.so", function = "abort", offset = 164),
            frame(0x2000, "/data/app/x/base.apk", mapOffset = 0x1234000),
        )))
        // 认不得的字段（内存映射、fixed64、fixed32）跳过
        .message(17, Pb().varint(1, 0x7000000000).varint(2, 0x7000001000).string(7, "/system/lib64/libc.so"))
        .fixed64(26, 42)
        .fixed32(27, 7)
        .varint(22, 4096)

    @Test
    fun readsTheCrashedThread() {
        val s = Tombstone.summary(segv().bytes())
        assertNotNull(s)
        s!!
        assertTrue(s, s.contains("Timestamp: 2026-11-25 10:23:45.123456789+0800"))
        assertTrue(s, s.contains("pid: 1234, tid: 1250, name: airhand-session  >>> app.airhand.android <<<"))
        assertTrue(s, s.contains("signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0000000000000010"))
        assertTrue(s, s.contains("Cause: null pointer dereference"))
        assertTrue(s, s.contains("      #00 pc 000000000004f2c8  $so (BuildId: abc123)"))
        assertTrue(s, s.contains("      #01 pc 0000000000089c4c  /apex/com.android.runtime/lib64/bionic/libc.so (abort+164)"))
        assertTrue(s, s.contains("      #02 pc 0000000000002000  /data/app/x/base.apk (offset 0x1234000)"))
        // 只要崩的那个线程
        assertFalse(s, s.contains("libmain_only"))
        assertFalse(s, s.contains("cut short"))
        assertFalse(s, s.contains("HUAWEI"))
    }

    @Test
    fun abortWithoutFaultAddress() {
        val t = Pb()
            .varint(1, 1)
            .varint(5, 99)
            .varint(6, 99)
            .message(10, Pb().varint(1, 6).string(2, "SIGABRT").varint(3, -1).string(4, "SI_TKILL").varint(5, 1).varint(6, 10234).varint(7, 99))
            .string(14, "Rust cannot catch foreign exceptions")
            .message(16, entry(99, thread(99, "airhand-rt", frame(0x10, so))))
        val s = Tombstone.summary(t.bytes())!!
        // int32 的 -1 是 10 个字节的 varint
        assertTrue(s, s.contains("signal 6 (SIGABRT), code -1 (SI_TKILL from pid 99, uid 10234), fault addr --------"))
        assertTrue(s, s.contains("Abort message: 'Rust cannot catch foreign exceptions'"))
        assertTrue(s, s.contains("name: airhand-rt"))
    }

    @Test
    fun longBacktracesAreShortened() {
        val frames = (0 until Tombstone.MAX_FRAMES + 5).map { frame(it.toLong() * 4, so) }.toTypedArray()
        val t = Pb().varint(1, 1).varint(6, 7).message(16, entry(7, thread(7, "deep", *frames)))
        val s = Tombstone.summary(t.bytes())!!
        assertTrue(s, s.contains("#63 pc"))
        assertFalse(s, s.contains("#64 pc"))
        assertTrue(s, s.contains("… 5 more frames"))
    }

    @Test
    fun usesWhatWasReadWhenCutShort() {
        // 后面跟一大段内存映射，读的时候被截在中间
        val full = segv().message(17, Pb().string(7, "x".repeat(1000))).bytes()
        val s = Tombstone.summary(full.copyOf(full.size - 500))
        assertNotNull(s)
        assertTrue(s!!, s.contains("signal 11 (SIGSEGV)"))
        assertTrue(s, s.contains("#00 pc 000000000004f2c8"))
        assertTrue(s, s.contains("cut short"))
    }

    @Test
    fun thirtyTwoBitAddresses() {
        val t = Pb()
            // arch ARM32 = 0：proto3 不写默认值
            .varint(6, 5)
            .message(10, Pb().varint(1, 11).string(2, "SIGSEGV").varint(8, 1).varint(9, 0xdead))
            .message(16, entry(5, thread(5, "t", frame(0x4f2c8, "/lib/libx.so"))))
        val s = Tombstone.summary(t.bytes())!!
        assertTrue(s, s.contains("fault addr 0x0000dead"))
        assertTrue(s, s.contains("#00 pc 0004f2c8  /lib/libx.so"))
    }

    @Test
    fun notATombstone() {
        assertNull(Tombstone.summary(ByteArray(0)))
        // wire type 7 不存在
        assertNull(Tombstone.summary(byteArrayOf(0x0f, 0x01)))
        // 只有认不得的字段
        assertNull(Tombstone.summary(Pb().string(2, "fingerprint").varint(5, 1).bytes()))
        // 随便一段文本（比如 Android 11 上给的不是 protobuf）
        assertNull(Tombstone.summary("*** *** *** *** *** ***\nBuild fingerprint: 'x'".toByteArray()))
    }

    @Test
    fun readerSkipsTypesItDoesNotWant() {
        val r = ProtoReader(Pb().varint(1, 300).fixed64(2, -1).string(3, "hi").fixed32(4, 5).bytes())
        assertTrue(r.next()); assertEquals(1, r.field); assertEquals(300L, r.varint); assertEquals("", r.string())
        assertTrue(r.next()); assertEquals(2, r.field); assertEquals(-1L, r.varint)
        assertTrue(r.next()); assertEquals(3, r.field); assertEquals("hi", r.string())
        assertTrue(r.next()); assertEquals(4, r.field); assertEquals(5L, r.varint); assertFalse(r.message().next())
        assertFalse(r.next())
    }
}
