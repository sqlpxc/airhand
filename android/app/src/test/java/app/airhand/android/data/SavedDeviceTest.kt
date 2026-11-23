package app.airhand.android.data

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test

class SavedDeviceTest {
    /** 互换 Ctrl 和 ⌘：默认关闭（以前保存的设备没有这一项也是关闭），打开之后记得住 */
    @Test
    fun ctrlCommandSwapIsOffByDefaultAndRemembered() {
        val old = SavedDevice.parse(JSONObject().put("code", "482913").put("hostOs", "macOS"))
        assertFalse(old.swapCtrlCommand)
        val on = old.copy(swapCtrlCommand = true)
        assertEquals(on, SavedDevice.parse(on.toJson()))
    }
}
