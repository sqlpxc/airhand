package app.airhand.android.core

import app.airhand.android.ui.phone.validServer
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class CodeTest {
    @Test
    fun parse() {
        // 与 airhand_proto::parse_code 相同的规则
        assertEquals("123456", Code.parse("123 456"))
        assertEquals("123456", Code.parse("123-456"))
        assertNull(Code.parse("012345"))
        assertNull(Code.parse("12345"))
        assertNull(Code.parse("12345a"))
        assertEquals("123 456", Code.format("123456"))
    }

    @Test
    fun findInPastedText() {
        assertEquals("123456", Code.find("控制码：123 456"))
        assertEquals("987654", Code.find("Connect with code 987-654 please"))
        assertEquals("555123", Code.find("555123"))
        assertNull(Code.find("1234567"))
        assertNull(Code.find("call 012 345"))
        assertNull(Code.find(null))
    }

    @Test
    fun presets() {
        assertEquals(Preset.BALANCED, Preset.of("nope"))
        assertEquals(Preset.HD, Preset.match(Preset.HD.quality(Codec.AV1)))
        assertEquals(1080, Preset.SMOOTH.maxShort)
    }

    @Test
    fun osKinds() {
        assertEquals(OsKind.ANDROID, OsKind.of("Android 14"))
        assertEquals(OsKind.WINDOWS, OsKind.of("Windows 11 Pro 24H2"))
        assertEquals(OsKind.MAC, OsKind.of("macOS 15.1"))
        assertEquals(OsKind.LINUX, OsKind.of("Ubuntu 24.04.1 LTS"))
        assertEquals(OsKind.UNKNOWN, OsKind.of(""))
    }

    @Test
    fun serverAddress() {
        assertTrue(validServer("relay.example.com:4433"))
        assertTrue(validServer(" 10.0.2.2:4433 "))
        assertTrue(validServer("[::1]:4433"))
        assertFalse(validServer("relay.example.com"))
        assertFalse(validServer("relay:99999"))
        assertFalse(validServer("::1:4433"))
        assertFalse(validServer(":4433"))
    }
}
