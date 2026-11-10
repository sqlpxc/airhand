package app.airhand.android.ui.session

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** 手势：触控板 / 触屏 / 直通三种方式下，手指动作变成什么鼠标操作 */
class GesturesTest {
    private val log = ArrayList<String>()
    private val timers = FakeTimers()
    private lateinit var g: Gestures

    /** 视图 1000×500，画面占满（没有黑边） */
    private val out = object : Gestures.Out {
        override fun toRemote(x: Float, y: Float) = (x / 1000f) to (y / 500f)
        override fun contentSize() = 1000f to 500f
        override fun move(nx: Float, ny: Float) {
            log += "move %.2f %.2f".format(nx, ny)
        }

        override fun delta(nx: Float, ny: Float) {
            log += "delta %.3f %.3f".format(nx, ny)
        }

        override fun button(button: String, down: Boolean) {
            log += "$button ${if (down) "down" else "up"}"
        }

        override fun wheel(dx: Int, dy: Int) {
            log += "wheel $dx $dy"
        }

        override fun zoom(factor: Float, fx: Float, fy: Float) {
            log += "zoom"
        }

        override fun pan(dx: Float, dy: Float) {}
        override fun follow() {}
        override fun haptic() {}
        override fun ripple(x: Float, y: Float) {}
        override fun toggleKeyboard() {
            log += "keyboard"
        }

        override fun touch(id: Int, phase: String, nx: Float, ny: Float) {
            log += "touch %d %s %.2f %.2f".format(id, phase, nx, ny)
        }
    }

    class FakeTimers : Gestures.Timers {
        var now = 0L
        private val pending = ArrayList<Pair<Long, () -> Unit>>()
        override fun after(ms: Long, r: () -> Unit): Any {
            val p = (now + ms) to r
            pending += p
            return p
        }

        override fun cancel(token: Any?) {
            pending.remove(token)
        }

        fun advance(ms: Long) {
            val target = now + ms
            while (true) {
                val next = pending.filter { it.first <= target }.minByOrNull { it.first } ?: break
                pending.remove(next)
                now = next.first
                next.second()
            }
            now = target
        }
    }

    private fun p(x: Float, y: Float, id: Int = 0) = Gestures.P(id, x, y)
    private fun buttons() = log.filter { it.startsWith("left") || it.startsWith("right") }

    @Before
    fun setUp() {
        g = Gestures(out, timers, slop = 10f)
    }

    @Test
    fun trackpadTapClicksAtThePointer() {
        g.mode = Gestures.Mode.TRACKPAD
        g.setCursor(0.25f, 0.5f)
        g.down(p(800f, 400f), 0)
        g.up(p(800f, 400f), 80)
        assertEquals(listOf("move 0.25 0.50", "left down", "left up"), log)
    }

    @Test
    fun trackpadSlideMovesThePointerRelatively() {
        g.mode = Gestures.Mode.TRACKPAD
        g.down(p(100f, 100f), 0)
        g.move(listOf(p(150f, 100f)), 100)
        g.move(listOf(p(200f, 100f)), 200)
        g.up(p(200f, 100f), 250)
        assertTrue(g.cursorX > 0.55f)
        assertEquals(0.5f, g.cursorY, 0.001f)
        assertTrue("no clicks: $log", buttons().isEmpty())
    }

    @Test
    fun hiddenRemoteCursorMakesTheTrackpadRelative() {
        g.mode = Gestures.Mode.TRACKPAD
        g.setCursor(0.25f, 0.5f)
        g.relative = true
        // 点一下：不先把光标挪到本机指针的位置（游戏里视角会跳）
        g.down(p(800f, 400f), 0)
        g.up(p(800f, 400f), 80)
        assertEquals(listOf("left down", "left up"), log)
        log.clear()
        // 滑动：发移动量（画面的几分之几），本机画的指针不动
        g.down(p(100f, 100f), 1000)
        g.move(listOf(p(150f, 100f)), 1100)
        g.move(listOf(p(200f, 120f)), 1200)
        g.up(p(200f, 120f), 1250)
        assertTrue("$log", log.isNotEmpty() && log.all { it.startsWith("delta ") })
        assertTrue("$log", log.any { it.startsWith("delta 0.") && !it.startsWith("delta 0.000") })
        assertEquals(0.25f, g.cursorX, 0.001f)
        assertEquals(0.5f, g.cursorY, 0.001f)
    }

    @Test
    fun touchDragBecomesRelativeWhenTheGameHidesTheCursor() {
        g.mode = Gestures.Mode.TOUCH
        g.down(p(500f, 250f), 0)
        g.move(listOf(p(540f, 250f)), 50)
        // 拖动开始时还是绝对坐标：移到按下的位置、按下左键
        assertEquals(listOf("move 0.50 0.50", "move 0.50 0.50", "left down", "move 0.54 0.50"), log)
        log.clear()
        // 游戏开始转视角，光标隐藏了：之后发手指移动了多少
        g.relative = true
        g.move(listOf(p(640f, 300f)), 100)
        g.up(p(640f, 300f), 150)
        assertEquals(listOf("delta 0.100 0.100", "left up"), log)
    }

    @Test
    fun trackpadLongPressIsARightClick() {
        g.mode = Gestures.Mode.TRACKPAD
        g.down(p(100f, 100f), 0)
        timers.advance(Gestures.LONG_PRESS_MS + 10)
        g.up(p(100f, 100f), 600)
        assertEquals(listOf("right down", "right up"), buttons())
    }

    @Test
    fun trackpadTapThenHoldDrags() {
        g.mode = Gestures.Mode.TRACKPAD
        g.down(p(100f, 100f), 0)
        g.up(p(100f, 100f), 60)
        g.down(p(102f, 101f), 150)
        g.move(listOf(p(160f, 101f)), 200)
        g.move(listOf(p(220f, 101f)), 250)
        g.up(p(220f, 101f), 300)
        assertEquals(listOf("left down", "left up", "left down", "left up"), buttons())
        // 拖动期间指针在动
        val i = log.indexOfLast { it == "left down" }
        assertTrue(log.subList(i, log.size).any { it.startsWith("move") })
    }

    @Test
    fun twoFingerTapIsARightClick() {
        g.mode = Gestures.Mode.TRACKPAD
        g.down(p(100f, 100f), 0)
        g.pointerDown(p(200f, 100f, 1), listOf(p(100f, 100f), p(200f, 100f, 1)), 20)
        g.pointerUp(p(0f, 0f, 1), 1, 120)
        g.up(p(100f, 100f), 130)
        assertEquals(listOf("right down", "right up"), buttons())
    }

    @Test
    fun twoFingerDragScrollsNaturally() {
        g.mode = Gestures.Mode.TRACKPAD
        g.down(p(100f, 100f), 0)
        g.pointerDown(p(200f, 100f, 1), listOf(p(100f, 100f), p(200f, 100f, 1)), 10)
        // 手指往下 = 内容往下 = 往上滚（滚轮为正）
        g.move(listOf(p(100f, 130f), p(200f, 130f, 1)), 40)
        g.move(listOf(p(100f, 160f), p(200f, 160f, 1)), 70)
        g.pointerUp(p(0f, 0f, 1), 1, 80)
        g.up(p(100f, 160f), 90)
        val wheel = log.filter { it.startsWith("wheel") }.map { it.split(" ")[2].toInt() }
        assertTrue("wheel: $log", wheel.isNotEmpty() && wheel.all { it > 0 })
        assertTrue(buttons().isEmpty())
    }

    @Test
    fun pinchZoomsTheView() {
        g.mode = Gestures.Mode.TOUCH
        g.down(p(400f, 200f), 0)
        g.pointerDown(p(500f, 200f, 1), listOf(p(400f, 200f), p(500f, 200f, 1)), 10)
        g.move(listOf(p(350f, 200f), p(550f, 200f, 1)), 40)
        g.pointerUp(p(0f, 0f, 1), 1, 60)
        g.up(p(350f, 200f), 70)
        assertTrue(log.contains("zoom"))
        assertTrue(buttons().isEmpty())
    }

    @Test
    fun touchTapClicksWhereTheFingerIs() {
        g.mode = Gestures.Mode.TOUCH
        g.down(p(500f, 250f), 0)
        g.up(p(500f, 250f), 60)
        assertEquals(listOf("move 0.50 0.50", "move 0.50 0.50", "left down", "left up"), log)
    }

    @Test
    fun touchLongPressIsARightClickThere() {
        g.mode = Gestures.Mode.TOUCH
        g.down(p(250f, 125f), 0)
        timers.advance(Gestures.LONG_PRESS_MS + 10)
        g.up(p(250f, 125f), 600)
        assertEquals(listOf("right down", "right up"), buttons())
        assertEquals(0.25f, g.cursorX, 0.001f)
    }

    @Test
    fun touchDragHoldsTheLeftButtonFromTheStart() {
        g.mode = Gestures.Mode.TOUCH
        g.down(p(100f, 100f), 0)
        g.move(listOf(p(300f, 100f)), 50)
        g.up(p(400f, 100f), 100)
        val down = log.indexOf("left down")
        assertEquals("move 0.10 0.20", log[down - 1])
        assertEquals("left up", log.last())
        assertEquals(0.4f, g.cursorX, 0.001f)
    }

    @Test
    fun passthroughQuickTap() {
        g.mode = Gestures.Mode.PASSTHROUGH
        g.down(p(100f, 100f), 0)
        g.up(p(100f, 100f), 30)
        assertEquals(listOf("left down", "left up"), buttons())
    }

    @Test
    fun passthroughHoldIsALongPressOnTheOtherSide() {
        g.mode = Gestures.Mode.PASSTHROUGH
        g.down(p(100f, 100f), 0)
        timers.advance(Gestures.PASSTHROUGH_DELAY_MS + 5)
        assertEquals(listOf("left down"), buttons())
        timers.advance(800)
        g.up(p(100f, 100f), 900)
        assertEquals(listOf("left down", "left up"), buttons())
    }

    @Test
    fun passthroughPinchDoesNotTouchTheOtherPhone() {
        g.mode = Gestures.Mode.PASSTHROUGH
        g.down(p(400f, 200f), 0)
        g.pointerDown(p(500f, 200f, 1), listOf(p(400f, 200f), p(500f, 200f, 1)), 20)
        timers.advance(200)
        g.move(listOf(p(380f, 200f), p(520f, 200f, 1)), 60)
        g.pointerUp(p(0f, 0f, 1), 1, 100)
        g.up(p(380f, 200f), 110)
        assertTrue(buttons().isEmpty())
        assertTrue(log.contains("zoom"))
    }

    @Test
    fun passthroughSwipeStartsRightAway() {
        g.mode = Gestures.Mode.PASSTHROUGH
        g.down(p(500f, 400f), 0)
        g.move(listOf(p(500f, 300f)), 16)
        g.up(p(500f, 100f), 40)
        assertEquals(listOf("left down", "left up"), buttons())
        val down = log.indexOf("left down")
        assertEquals("move 0.50 0.80", log[down - 1])
    }

    @Test
    fun threeFingerTapTogglesTheKeyboard() {
        g.mode = Gestures.Mode.TRACKPAD
        g.down(p(100f, 100f), 0)
        g.pointerDown(p(200f, 100f, 1), listOf(p(100f, 100f), p(200f, 100f, 1)), 10)
        g.pointerDown(p(300f, 100f, 2), listOf(p(100f, 100f), p(200f, 100f, 1), p(300f, 100f, 2)), 20)
        g.pointerUp(p(0f, 0f, 2), 2, 100)
        g.pointerUp(p(0f, 0f, 1), 1, 110)
        g.up(p(100f, 100f), 120)
        assertEquals(listOf("keyboard"), log.filter { it == "keyboard" })
        assertTrue(buttons().isEmpty())
    }

    @Test
    fun multitouchPassesEveryFingerThrough() {
        g.mode = Gestures.Mode.MULTITOUCH
        g.down(p(400f, 250f, id = 5), 0)
        g.pointerDown(p(600f, 250f, id = 9), listOf(p(400f, 250f, 5), p(600f, 250f, 9)), 10)
        g.move(listOf(p(300f, 250f, 5), p(700f, 250f, 9)), 30)
        // 没动的手指不重复发
        g.move(listOf(p(300f, 250f, 5), p(750f, 250f, 9)), 40)
        g.pointerUp(p(750f, 250f, 9), 1, 60)
        g.up(p(300f, 250f, 5), 70)
        assertEquals(
            listOf(
                "touch 0 down 0.40 0.50",
                "touch 1 down 0.60 0.50",
                "touch 0 move 0.30 0.50",
                "touch 1 move 0.70 0.50",
                "touch 1 move 0.75 0.50",
                "touch 1 up 0.75 0.50",
                "touch 0 up 0.30 0.50",
            ),
            log,
        )
    }

    @Test
    fun multitouchReusesFreeSlotsAndCancels() {
        g.mode = Gestures.Mode.MULTITOUCH
        g.down(p(100f, 100f, id = 0), 0)
        g.pointerDown(p(200f, 100f, id = 1), listOf(p(100f, 100f, 0), p(200f, 100f, 1)), 10)
        g.pointerUp(p(100f, 100f, 0), 1, 20)
        // 新手指用空出来的 0 号
        g.pointerDown(p(300f, 100f, id = 2), listOf(p(200f, 100f, 1), p(300f, 100f, 2)), 30)
        // 三指手势也传给对方，不开键盘
        g.pointerDown(p(400f, 100f, id = 3), listOf(p(200f, 100f, 1), p(300f, 100f, 2), p(400f, 100f, 3)), 35)
        g.cancel()
        val events = log.map { it.split(" ").take(3).joinToString(" ") }
        assertEquals(listOf("touch 0 down", "touch 1 down", "touch 0 up", "touch 0 down", "touch 2 down"), events.take(5))
        // 打断时还按着的三个点都松开（顺序不重要）
        assertEquals(setOf("touch 0 cancel", "touch 1 cancel", "touch 2 cancel"), events.drop(5).toSet())
        assertEquals(8, events.size)
        assertTrue(log.none { it == "keyboard" })
        assertTrue(buttons().isEmpty())
    }

    @Test
    fun accelerationIsBounded() {
        assertEquals(0.7f, Gestures.accel(0f), 0.001f)
        assertEquals(2.4f, Gestures.accel(100f), 0.001f)
        assertTrue(Gestures.accel(1f) > 1f)
    }
}
