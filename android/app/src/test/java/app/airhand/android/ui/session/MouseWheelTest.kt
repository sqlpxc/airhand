package app.airhand.android.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** 外接鼠标的滚轮换算成发给对方的量（120 = 一格） */
class MouseWheelTest {
    @Test
    fun oneNotchIs120() {
        val w = MouseWheel()
        assertEquals(0 to 120, w.steps(0f, 1f))
        assertEquals(-120 to 0, w.steps(-1f, 0f))
    }

    /** 高精度滚轮一次只滚一点点：不够 1 的攒着，不会丢 */
    @Test
    fun fractionsAddUp() {
        val w = MouseWheel()
        assertNull(w.steps(0f, 0.005f))
        assertEquals(0 to 1, w.steps(0f, 0.005f))
    }

    /** 换方向时丢掉反方向的余量 */
    @Test
    fun changingDirectionDropsTheRest() {
        val w = MouseWheel()
        assertNull(w.steps(0f, 0.005f))
        assertEquals(0 to -1, w.steps(0f, -0.01f))
    }

    @Test
    fun reverseAndSpeed() {
        val w = MouseWheel()
        w.reverse = true
        w.percent = 50
        assertEquals(-60 to -60, w.steps(1f, 1f))
        w.reverse = false
        w.percent = 200
        assertEquals(0 to 240, w.steps(0f, 1f))
    }
}
