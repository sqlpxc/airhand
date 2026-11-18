package app.airhand.android.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class SettingsTest {
    /** 拖动窗口时显示本地光标：默认打开（保存的设置里没有这一项也是打开），关掉之后记得住 */
    @Test
    fun localCursorIsOnByDefaultAndRemembered() {
        assertTrue(Settings().localCursor)
        assertTrue(Settings.parse(JSONObject().put("navKeys", false)).localCursor)
        val off = Settings(localCursor = false)
        assertFalse(Settings.parse(off.toJson()).localCursor)
        assertEquals(off, Settings.parse(off.toJson()))
    }
}
