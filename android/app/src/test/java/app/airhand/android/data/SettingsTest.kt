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

    /** 外接鼠标滚轮：默认不反转、100%，改了记得住；不在可选范围里的速度当 100% */
    @Test
    fun wheelPreferences() {
        val old = Settings.parse(JSONObject())
        assertFalse(old.reverseWheel)
        assertEquals(100, old.wheelSpeed)
        val custom = Settings(reverseWheel = true, wheelSpeed = 150)
        assertEquals(custom, Settings.parse(custom.toJson()))
        assertEquals(100, Settings.parse(JSONObject().put("wheelSpeed", 0)).wheelSpeed)
    }
}
