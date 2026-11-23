package app.airhand.android.ui.session

import android.view.KeyEvent
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** 外接键盘的哪些按键在输入法之前发：Ctrl / ⌘ 的组合键，抬起跟着按下走 */
class ImeBypassTest {
    private val b = ImeBypass()

    /** 模拟 RemoteView.onKeyPreIme：要提前的就当作已经发了 */
    private fun down(keyCode: Int, shortcut: Boolean, repeatCount: Int = 0): Boolean =
        b.early(keyCode, true, shortcut).also { if (it) b.sent(keyCode, repeatCount) }

    private fun up(keyCode: Int): Boolean = b.early(keyCode, false, false)

    /** 先松开 Ctrl：C 的抬起已经不带 Ctrl，也要在输入法之前发 */
    @Test
    fun ctrlCGoesFirstEvenWhenCtrlIsReleasedFirst() {
        assertTrue(down(KeyEvent.KEYCODE_CTRL_LEFT, true))
        assertTrue(down(KeyEvent.KEYCODE_C, true))
        assertTrue(up(KeyEvent.KEYCODE_CTRL_LEFT))
        assertTrue(up(KeyEvent.KEYCODE_C))
    }

    /** 单按 Shift（输入法用来切换中英文）：按下、抬起都给输入法 */
    @Test
    fun shiftStaysWithTheIme() {
        assertFalse(down(KeyEvent.KEYCODE_SHIFT_LEFT, false))
        assertFalse(up(KeyEvent.KEYCODE_SHIFT_LEFT))
    }

    /** 按着 A 的时候才按下 Ctrl：A 的第一次按下给了输入法，抬起也给它 */
    @Test
    fun repeatsDontMoveTheRelease() {
        assertFalse(down(KeyEvent.KEYCODE_A, false))
        assertTrue(down(KeyEvent.KEYCODE_CTRL_LEFT, true))
        assertTrue(down(KeyEvent.KEYCODE_A, true, repeatCount = 3))
        assertFalse(up(KeyEvent.KEYCODE_A))
        assertTrue(up(KeyEvent.KEYCODE_CTRL_LEFT))
    }

    @Test
    fun focusLossForgetsKeys() {
        assertTrue(down(KeyEvent.KEYCODE_META_LEFT, true))
        b.clear()
        assertFalse(up(KeyEvent.KEYCODE_META_LEFT))
    }
}
