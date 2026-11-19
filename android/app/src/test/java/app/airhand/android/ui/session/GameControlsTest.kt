package app.airhand.android.ui.session

import app.airhand.android.data.GameBind
import app.airhand.android.data.GameKey
import app.airhand.android.data.GameLayout
import app.airhand.android.data.GameStick
import app.airhand.android.data.KeyMode
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import kotlin.math.cos
import kotlin.math.sin

/** 游戏按键：手指按在按钮、摇杆上变成什么按键 */
class GameControlsTest {
    private val log = ArrayList<String>()
    private val timers = GesturesTest.FakeTimers()
    private var haptics = 0

    private val out = object : GameControls.Out {
        override fun key(code: String, down: Boolean) {
            log += "$code${if (down) "↓" else "↑"}"
        }

        override fun button(button: String, down: Boolean) {
            log += "mouse-$button${if (down) "↓" else "↑"}"
        }

        override fun wheel(dy: Int) {
            log += "wheel $dy"
        }

        override fun look(dx: Float, dy: Float) {
            log += "look %.0f %.0f".format(dx, dy)
        }

        override fun haptic() {
            haptics++
        }

        override fun changed() {}
    }

    /** 区域 840×390，1dp = 1px */
    private val geo = GameGeometry(1f).apply { set(0f, 0f, 840f, 390f) }
    private val g = GameControls(out, timers, geo, slop = 8f)

    // 中心：摇杆 (126, 265.2) 半径 75；开火 (705.6, 280.8) 半径 42；W (420, 195)；蹲 (252, 117)；组合键 (588, 78)；滚轮 (756, 78)
    private val stick = GameStick(1, 0.15f, 0.68f, 150f, GameBind.WASD, sprint = "ShiftLeft", floating = false)
    private val fire = GameKey(2, 0.84f, 0.72f, 84f, listOf(GameBind.mouse("left")), look = true)
    private val w = GameKey(3, 0.5f, 0.5f, 60f, listOf("KeyW"))
    private val crouch = GameKey(4, 0.3f, 0.3f, 60f, listOf("KeyC"), mode = KeyMode.TOGGLE)
    private val combo = GameKey(5, 0.7f, 0.2f, 60f, listOf("ControlLeft", "Digit1"))
    private val wheel = GameKey(6, 0.9f, 0.2f, 50f, listOf(GameBind.WHEEL_UP))
    private val layout = GameLayout("t", "test", listOf(stick, fire, w, crouch, combo, wheel))

    private val sx = 126f
    private val sy = 265.2f

    @Before
    fun setUp() {
        g.layout = layout
        log.clear()
    }

    /** 摇杆上的手指：离中心 `dist`（像素），方向 `deg`（0 = 上，顺时针） */
    private fun stickAt(dist: Float, deg: Double, cx: Float = sx, cy: Float = sy): Pair<Float, Float> {
        val a = Math.toRadians(deg)
        return (cx + dist * sin(a)).toFloat() to (cy - dist * cos(a)).toFloat()
    }

    private fun moveStick(dist: Float, deg: Double) {
        val (x, y) = stickAt(dist, deg)
        g.move(0, x, y)
    }

    @Test
    fun holdingAButtonHoldsItsKey() {
        assertTrue(g.down(0, 420f, 195f))
        assertTrue(g.owns(0))
        assertTrue(g.isDown(w.id))
        assertEquals(listOf("KeyW↓"), log)
        assertTrue(g.up(0))
        assertEquals(listOf("KeyW↓", "KeyW↑"), log)
        assertFalse(g.isDown(w.id))
        assertEquals(1, haptics)
    }

    @Test
    fun fingersAwayFromButtonsAreLeftToGestures() {
        assertFalse(g.down(0, 420f, 330f))
        assertFalse(g.owns(0))
        assertFalse(g.up(0))
        assertTrue(log.isEmpty())
        // 没有显示时什么都不接
        g.layout = null
        assertFalse(g.down(1, 420f, 195f))
    }

    @Test
    fun slightlyOutsideAButtonStillCounts() {
        // 半径 30，外面 6dp 以内也算按到（手指比眼睛看到的粗）
        assertTrue(g.down(0, 420f + 34f, 195f))
        assertFalse(g.down(1, 420f + 40f, 195f))
    }

    @Test
    fun aFingerKeepsItsButtonWhenItSlidesOff() {
        g.down(0, 420f, 195f)
        g.move(0, 600f, 300f)
        assertEquals(listOf("KeyW↓"), log)
        g.up(0)
        assertEquals(listOf("KeyW↓", "KeyW↑"), log)
    }

    @Test
    fun comboPressesInOrderAndReleasesInReverse() {
        g.down(0, 588f, 78f)
        g.up(0)
        assertEquals(listOf("ControlLeft↓", "Digit1↓", "Digit1↑", "ControlLeft↑"), log)
    }

    @Test
    fun toggleStaysDownUntilTappedAgain() {
        g.down(0, 252f, 117f)
        g.up(0)
        assertEquals(listOf("KeyC↓"), log)
        assertTrue(g.isDown(crouch.id))
        g.down(0, 252f, 117f)
        g.up(0)
        assertEquals(listOf("KeyC↓", "KeyC↑"), log)
        assertFalse(g.isDown(crouch.id))
    }

    @Test
    fun stickHasEightDirections() {
        assertTrue(g.down(0, sx, sy))
        assertTrue("中心不按任何键: $log", log.isEmpty())
        moveStick(60f, 0.0)
        assertEquals(listOf("KeyW↓"), log)
        moveStick(60f, 45.0)
        assertEquals(listOf("KeyW↓", "KeyD↓"), log)
        moveStick(60f, 90.0)
        assertEquals(listOf("KeyW↓", "KeyD↓", "KeyW↑"), log)
        log.clear()
        // 一下子甩到另一边：先松开再按下
        moveStick(60f, 270.0)
        assertEquals(listOf("KeyD↑", "KeyA↓"), log)
        log.clear()
        moveStick(60f, 225.0)
        assertEquals(listOf("KeyS↓"), log)
        g.up(0)
        assertEquals(listOf("KeyS↓", "KeyS↑", "KeyA↑"), log)
    }

    @Test
    fun stickDirectionsHaveHysteresis() {
        g.down(0, sx, sy)
        moveStick(60f, 0.0)
        moveStick(60f, 24.0) // sin 24° = 0.41 > 0.383：斜着
        assertEquals(listOf("KeyW↓", "KeyD↓"), log)
        moveStick(60f, 19.0) // 0.33：还在回差里，不松开
        assertEquals(listOf("KeyW↓", "KeyD↓"), log)
        moveStick(60f, 15.0) // 0.26 < 0.30：松开
        assertEquals(listOf("KeyW↓", "KeyD↓", "KeyD↑"), log)
        moveStick(60f, 21.0) // 0.36 < 0.383：没松开的时候要过 0.383 才按
        assertEquals(listOf("KeyW↓", "KeyD↓", "KeyD↑"), log)
    }

    @Test
    fun stickDeadZoneHasHysteresis() {
        g.down(0, sx, sy)
        moveStick(15f, 0.0) // 0.2 r：死区里
        assertTrue(log.isEmpty())
        moveStick(20f, 0.0) // 0.27 r
        assertEquals(listOf("KeyW↓"), log)
        moveStick(15f, 0.0) // 0.2 r：按着时要回到 0.18 r 以内才松开
        assertEquals(listOf("KeyW↓"), log)
        moveStick(12f, 0.0)
        assertEquals(listOf("KeyW↓", "KeyW↑"), log)
    }

    @Test
    fun pushingPastTheEdgeAlsoPressesSprint() {
        g.down(0, sx, sy)
        moveStick(80f, 0.0) // 1.07 r：还没出去
        assertEquals(listOf("KeyW↓"), log)
        val before = haptics
        moveStick(90f, 0.0) // 1.2 r
        assertEquals(listOf("KeyW↓", "ShiftLeft↓"), log)
        assertEquals(before + 1, haptics)
        moveStick(80f, 0.0) // 回差：边缘以外不松开
        assertEquals(listOf("KeyW↓", "ShiftLeft↓"), log)
        moveStick(70f, 0.0)
        assertEquals(listOf("KeyW↓", "ShiftLeft↓", "ShiftLeft↑"), log)
        moveStick(90f, 0.0)
        g.up(0)
        assertEquals(listOf("ShiftLeft↓", "ShiftLeft↑", "KeyW↑"), log.takeLast(3))
    }

    @Test
    fun floatingStickCentersUnderTheThumb() {
        g.layout = layout.with(stick.copy(floating = true))
        log.clear()
        // 按在离中心 40px 的地方：中心挪到这里，不马上按方向
        val (x, y) = stickAt(40f, 90.0)
        assertTrue(g.down(0, x, y))
        assertTrue(log.isEmpty())
        val st = g.stickState(stick.id)!!
        assertEquals(x, st.cx, 0.01f)
        // 从这里往上推
        g.move(0, x, y - 30f)
        assertEquals(listOf("KeyW↓"), log)
        // 摇杆头不超出底盘
        g.move(0, x, y - 300f)
        val st2 = g.stickState(stick.id)!!
        assertEquals(y - 75f, st2.ky, 0.01f)
        g.up(0)
        assertNull(g.stickState(stick.id))
    }

    @Test
    fun aKeyHeldInTwoPlacesIsReleasedWhenBothLetGo() {
        g.down(0, sx, sy)
        moveStick(60f, 0.0) // 摇杆按着 W
        g.down(1, 420f, 195f) // W 按钮
        g.up(0)
        assertEquals("摇杆松开时 W 按钮还按着", listOf("KeyW↓"), log)
        g.up(1)
        assertEquals(listOf("KeyW↓", "KeyW↑"), log)
    }

    @Test
    fun theStickTakesOneFingerAtATime() {
        assertTrue(g.down(0, sx, sy))
        assertFalse(g.down(1, sx + 10f, sy))
    }

    @Test
    fun fireButtonTurnsTheViewWhenDragged() {
        g.down(0, 705.6f, 280.8f)
        assertEquals(listOf("mouse-left↓"), log)
        assertTrue(g.holdsMouse)
        // 手指的一点抖动不转视角
        g.move(0, 710f, 281f)
        assertEquals(1, log.size)
        // 过了一点距离开始转，从这里算起（不跳）
        g.move(0, 725f, 281f)
        assertEquals(1, log.size)
        g.move(0, 735f, 276f)
        assertEquals(listOf("mouse-left↓", "look 10 -5"), log)
        g.up(0)
        assertEquals("mouse-left↑", log.last())
        assertFalse(g.holdsMouse)
    }

    @Test
    fun wheelButtonKeepsTurningWhileHeld() {
        g.down(0, 756f, 78f)
        assertEquals(listOf("wheel 120"), log)
        timers.advance(GameControls.WHEEL_DELAY_MS)
        timers.advance(GameControls.WHEEL_REPEAT_MS)
        assertEquals(3, log.size)
        g.up(0)
        timers.advance(1000)
        assertEquals(listOf("wheel 120", "wheel 120", "wheel 120"), log)
    }

    @Test
    fun releaseAllLetsGoOfEverything() {
        g.down(0, sx, sy)
        moveStick(60f, 45.0)
        g.down(1, 705.6f, 280.8f)
        g.down(2, 252f, 117f)
        g.up(2) // 蹲：切换着
        log.clear()
        g.releaseAll()
        assertEquals(setOf("KeyW↑", "KeyD↑", "mouse-left↑", "KeyC↑"), log.toSet())
        assertEquals(4, log.size)
        assertFalse(g.isDown(crouch.id))
        assertFalse(g.holdsMouse)
        // 之后这些手指的移动、抬起都不算了
        g.move(0, sx, sy - 60f)
        assertFalse(g.up(0))
        assertEquals(4, log.size)
    }

    @Test
    fun liftKeepsToggles() {
        g.down(0, 252f, 117f)
        g.up(0)
        g.down(1, 420f, 195f)
        g.lift()
        assertEquals(listOf("KeyC↓", "KeyW↓", "KeyW↑"), log)
        assertTrue(g.isDown(crouch.id))
    }

    @Test
    fun changingTheLayoutReleasesHeldKeys() {
        g.down(0, 420f, 195f)
        g.layout = layout.copy(name = "other")
        assertEquals(listOf("KeyW↓", "KeyW↑"), log)
        // 一样的方案：不动
        g.down(0, 420f, 195f)
        g.layout = layout.copy(name = "other")
        assertEquals(listOf("KeyW↓", "KeyW↑", "KeyW↓"), log)
    }

    @Test
    fun noVibrationWhenTurnedOff() {
        g.layout = layout.copy(haptics = false)
        g.down(0, 420f, 195f)
        g.down(1, sx, sy)
        val (x, y) = stickAt(90f, 0.0)
        g.move(1, x, y)
        assertEquals(listOf("KeyW↓", "ShiftLeft↓"), log)
        assertEquals(0, haptics)
    }

    @Test
    fun overlappingButtonsPickTheNearestCenter() {
        val a = GameKey(1, 0.5f, 0.5f, 60f, listOf("KeyA"))
        val b = GameKey(2, 0.56f, 0.5f, 60f, listOf("KeyB")) // 中心相距 50.4px，两个按钮的范围重叠
        g.layout = GameLayout("o", "o", listOf(a, b))
        g.down(0, 440f, 195f) // 离 A 20px，离 B 30px
        assertEquals(listOf("KeyA↓"), log)
    }
}
