package app.airhand.android.ui.session

import app.airhand.android.controller.Input
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min

/**
 * 把手指的动作变成远端的鼠标操作（纯逻辑，方便测试；[RemoteView] 把 MotionEvent 转成这里的调用）。
 *
 * 三种方式（[Mode]）：
 *
 * **触控板**（默认用于电脑）：屏幕当触控板用，本机画出指针
 * * 单指滑动 = 移动指针（慢速精细、快速加速）
 * * 单击 = 左键单击；双击 = 双击
 * * 长按 = 右键
 * * 点一下再按住拖动（双击不松开）= 按住左键拖动（选文字、拖窗口）
 * * 双指点击 = 右键；双指拖动 = 滚动（松手后惯性滚动）
 * * 双指捏合 = 放大画面（放大后画面跟着指针走）
 *
 * **触屏**（电脑也可以选）：点哪里就是哪里
 * * 单击 = 在那里单击；长按 = 在那里右键
 * * 单指拖动 = 按住左键拖动
 * * 双指拖动 = 滚动；双指捏合 = 放大 / 移动画面；双指点击 = 右键
 *
 * **多点触摸**（被控端是手机，能接收触摸）：每个手指原样传过去，双指缩放、多指手势都作用在对方手机上
 *
 * **直通**（被控端是手机，但不能接收多点触摸，例如 Android 7）：一个手指的点按、长按、滑动传过去，
 * 双指捏合 / 移动只缩放本机的画面
 *
 * 三指点击 = 打开 / 收起键盘（多点触摸方式下三指手势也传给对方，不用来开键盘）。
 *
 * **相对移动**（[relative]：被控端的光标隐藏了，例如游戏里拖动转视角；或者打开了锁定鼠标）：
 * 触控板的移动、触屏的拖动发"移动了多少"（[Out.delta]），不再发位置；点击也不先把光标挪到哪里。
 * 游戏这时自己把光标拉回原位、只看移动量，发位置会和它打架（视角乱跳、拖到画面边上就转不动了）。
 */
class Gestures(private val out: Out, private val timers: Timers, private val slop: Float) {

    interface Out {
        /** 视图坐标 → 远端归一化坐标（0..1）；还没有画面时返回 null */
        fun toRemote(x: Float, y: Float): Pair<Float, Float>?

        /** 画面在视图里的宽高（像素，放大后变大） */
        fun contentSize(): Pair<Float, Float>
        fun move(nx: Float, ny: Float)

        /** 相对移动：移动了画面宽 / 高的多少（不是位置），由接收方换算成被控端的像素 */
        fun delta(nx: Float, ny: Float)
        fun button(button: String, down: Boolean)
        fun wheel(dx: Int, dy: Int)
        fun zoom(factor: Float, fx: Float, fy: Float)
        fun pan(dx: Float, dy: Float)

        /** 触控板模式下指针动了：画面放大时跟着它走 */
        fun follow()
        fun haptic()

        /** 在视图的这个位置显示一下点击反馈 */
        fun ripple(x: Float, y: Float)
        fun toggleKeyboard()

        /** 触摸点（多点触摸方式）。`phase` 是 [DOWN] / [MOVE] / [UP] / [CANCEL]，`id` 是 0..9 */
        fun touch(id: Int, phase: String, nx: Float, ny: Float)
    }

    interface Timers {
        fun after(ms: Long, r: () -> Unit): Any
        fun cancel(token: Any?)
    }

    enum class Mode { TRACKPAD, TOUCH, PASSTHROUGH, MULTITOUCH }

    data class P(val id: Int, val x: Float, val y: Float)

    var mode = Mode.TRACKPAD

    /** 发相对移动（见类的说明）。只对触控板和触屏方式有用：直通、多点触摸是控制手机，没有光标 */
    var relative = false

    /** 远端指针（归一化） */
    var cursorX = 0.5f
        private set
    var cursorY = 0.5f
        private set

    fun setCursor(x: Float, y: Float) {
        cursorX = x.coerceIn(0f, 1f)
        cursorY = y.coerceIn(0f, 1f)
    }

    // ───────────── 状态 ─────────────

    private enum class Kind { NONE, ONE, TWO, MANY, DONE }

    private var kind = Kind.NONE
    private var startX = 0f
    private var startY = 0f
    private var startT = 0L
    private var lastX = 0f
    private var lastY = 0f
    private var lastT = 0L
    private var moved = false
    private var longFired = false
    private var dragging = false
    private var doubleTapDrag = false
    private var longTimer: Any? = null

    // 直通模式：按下稍等一下再发，第二个手指很快落下就当作缩放，不点到对方
    private var pendingDown = false
    private var downTimer: Any? = null

    // 双指
    private var twoStartT = 0L
    private var twoStartSpan = 0f
    private var twoStartCx = 0f
    private var twoStartCy = 0f
    private var twoLastSpan = 0f
    private var twoLastCx = 0f
    private var twoLastCy = 0f
    private var twoKind = 0 // 0 未定 1 滚动 2 缩放
    private var twoMoved = false
    private var maxFingers = 0
    private var wheelAccX = 0f
    private var wheelAccY = 0f
    private var velY = 0f
    private var velX = 0f
    private var flingTimer: Any? = null

    // 多点触摸：本机的手指编号 → 发给对方的触摸点编号（0..9）
    private val touchIds = HashMap<Int, Int>()
    private val touchPos = HashMap<Int, Pair<Float, Float>>()

    // 上一次单击（触控板的"点一下再按住拖动"）
    private var lastTapT = -10_000L
    private var lastTapX = 0f
    private var lastTapY = 0f

    // ───────────── 事件 ─────────────

    fun down(p: P, t: Long) {
        stopFling()
        if (mode == Mode.MULTITOUCH) {
            kind = Kind.MANY
            touchIds.clear()
            touchPos.clear()
            touchDown(p)
            out.ripple(p.x, p.y)
            return
        }
        kind = Kind.ONE
        maxFingers = 1
        startX = p.x; startY = p.y; startT = t
        lastX = p.x; lastY = p.y; lastT = t
        moved = false
        longFired = false
        dragging = false
        doubleTapDrag = false
        when (mode) {
            // 多点触摸方式在函数开头已经处理完了
            Mode.MULTITOUCH -> {}
            Mode.TRACKPAD -> {
                doubleTapDrag = t - lastTapT < DOUBLE_TAP_MS && hypot(p.x - lastTapX, p.y - lastTapY) < slop * 4
                if (!doubleTapDrag) longTimer = timers.after(LONG_PRESS_MS) { onLongPress() }
            }
            Mode.TOUCH -> {
                // 相对移动时（游戏）按下不挪光标，不然视角会跳一下
                if (!relative) {
                    val r = out.toRemote(p.x, p.y) ?: return
                    setCursor(r.first, r.second)
                    out.move(cursorX, cursorY)
                }
                longTimer = timers.after(LONG_PRESS_MS) { onLongPress() }
            }
            Mode.PASSTHROUGH -> {
                val r = out.toRemote(p.x, p.y) ?: return
                setCursor(r.first, r.second)
                pendingDown = true
                downTimer = timers.after(PASSTHROUGH_DELAY_MS) { commitDown() }
            }
        }
    }

    private fun commitDown() {
        if (!pendingDown || kind != Kind.ONE) return
        pendingDown = false
        val r = out.toRemote(startX, startY) ?: return
        out.move(r.first, r.second)
        out.button(LEFT, true)
        dragging = true
        if (lastX != startX || lastY != startY) {
            out.toRemote(lastX, lastY)?.let { out.move(it.first, it.second) }
        }
    }

    private fun onLongPress() {
        longTimer = null
        if (kind != Kind.ONE || moved) return
        longFired = true
        out.haptic()
        when (mode) {
            // 多点触摸方式在函数开头已经处理完了
            Mode.MULTITOUCH -> {}
            Mode.TRACKPAD -> click(RIGHT)
            Mode.TOUCH -> {
                out.ripple(startX, startY)
                click(RIGHT)
            }
            Mode.PASSTHROUGH -> {}
        }
    }

    private fun click(button: String) {
        if (!relative) out.move(cursorX, cursorY)
        out.button(button, true)
        out.button(button, false)
    }

    /** 又一个手指落下（`p`），`ps` 是现在所有的手指 */
    fun pointerDown(p: P, ps: List<P>, t: Long) {
        if (mode == Mode.MULTITOUCH) {
            touchDown(p)
            return
        }
        timers.cancel(longTimer)
        longTimer = null
        maxFingers = max(maxFingers, ps.size)
        if (ps.size >= 3) {
            endDrag()
            kind = Kind.MANY
            return
        }
        if (kind == Kind.ONE) {
            if (pendingDown) {
                // 第二个手指很快落下：是缩放，不是点按
                pendingDown = false
                timers.cancel(downTimer)
            }
            endDrag()
            kind = Kind.TWO
            twoStartT = t
            val (cx, cy, span) = centroid(ps)
            twoStartCx = cx; twoStartCy = cy; twoStartSpan = span
            twoLastCx = cx; twoLastCy = cy; twoLastSpan = span
            twoKind = if (mode == Mode.PASSTHROUGH) 2 else 0
            twoMoved = moved
            wheelAccX = 0f; wheelAccY = 0f
            velX = 0f; velY = 0f
        }
    }

    fun move(ps: List<P>, t: Long) {
        if (mode == Mode.MULTITOUCH) {
            for (p in ps) {
                val id = touchIds[p.id] ?: continue
                if (touchPos[p.id] == (p.x to p.y)) continue
                touchPos[p.id] = p.x to p.y
                out.toRemote(p.x, p.y)?.let { out.touch(id, MOVE, it.first, it.second) }
            }
            return
        }
        when (kind) {
            Kind.ONE -> ps.firstOrNull()?.let { moveOne(it, t) }
            Kind.TWO -> if (ps.size >= 2) moveTwo(ps, t)
            else -> {}
        }
    }

    private fun moveOne(p: P, t: Long) {
        val dx = p.x - lastX
        val dy = p.y - lastY
        val dt = max(1L, t - lastT)
        if (!moved && hypot(p.x - startX, p.y - startY) > slop) {
            moved = true
            timers.cancel(longTimer)
            longTimer = null
            when (mode) {
                // 多点触摸方式在函数开头已经处理完了
                Mode.MULTITOUCH -> {}
                Mode.TRACKPAD -> if (doubleTapDrag) {
                    out.haptic()
                    if (!relative) out.move(cursorX, cursorY)
                    out.button(LEFT, true)
                    dragging = true
                }
                Mode.TOUCH -> if (!longFired) {
                    // 从按下的位置开始拖
                    if (!relative) out.toRemote(startX, startY)?.let { out.move(it.first, it.second) }
                    out.button(LEFT, true)
                    dragging = true
                }
                Mode.PASSTHROUGH -> if (pendingDown) {
                    // 单指已经滑开了，不会是双指缩放：马上按下
                    timers.cancel(downTimer)
                    commitDown()
                }
            }
        }
        lastX = p.x; lastY = p.y; lastT = t
        if (!moved && mode != Mode.PASSTHROUGH) return
        when (mode) {
            // 多点触摸方式在函数开头已经处理完了
            Mode.MULTITOUCH -> {}
            Mode.TRACKPAD -> {
                val (w, h) = out.contentSize()
                if (w <= 0 || h <= 0) return
                val speed = hypot(dx, dy) / dt // 像素 / 毫秒
                val gain = accel(speed)
                if (relative) {
                    // 被控端的光标隐藏着：只发移动了多少，本机画的指针不动
                    out.delta(dx / w * gain, dy / h * gain)
                    return
                }
                setCursor(cursorX + dx / w * gain, cursorY + dy / h * gain)
                out.move(cursorX, cursorY)
                out.follow()
            }
            Mode.TOUCH -> if (dragging) {
                if (relative) {
                    // 游戏里拖动转视角：手指在画面上移动了多少，就发多少
                    val (w, h) = out.contentSize()
                    if (w > 0 && h > 0) out.delta(dx / w, dy / h)
                } else {
                    out.toRemote(p.x, p.y)?.let {
                        setCursor(it.first, it.second)
                        out.move(cursorX, cursorY)
                    }
                }
            }
            Mode.PASSTHROUGH -> {
                val r = out.toRemote(p.x, p.y) ?: return
                setCursor(r.first, r.second)
                if (dragging) out.move(cursorX, cursorY)
            }
        }
    }

    private fun moveTwo(ps: List<P>, t: Long) {
        val (cx, cy, span) = centroid(ps)
        if (twoKind == 0) {
            if (abs(span - twoStartSpan) > slop * 2.5f) {
                twoKind = 2
                twoMoved = true
            } else if (hypot(cx - twoStartCx, cy - twoStartCy) > slop) {
                twoKind = 1
                twoMoved = true
            }
        }
        val dx = cx - twoLastCx
        val dy = cy - twoLastCy
        val dt = max(1L, t - lastT)
        when (twoKind) {
            1 -> {
                // 自然滚动：内容跟着手指走。手指往下 = 往上滚（滚轮为正）
                wheelAccY += dy * WHEEL_PER_PX
                wheelAccX -= dx * WHEEL_PER_PX
                flushWheel()
                velX = 0.6f * velX + 0.4f * (dx / dt)
                velY = 0.6f * velY + 0.4f * (dy / dt)
            }
            2 -> {
                if (twoLastSpan > 0f && span > 0f) out.zoom(span / twoLastSpan, cx, cy)
                out.pan(dx, dy)
            }
        }
        twoLastCx = cx; twoLastCy = cy; twoLastSpan = span
        lastT = t
    }

    private fun flushWheel() {
        val ix = wheelAccX.toInt()
        val iy = wheelAccY.toInt()
        if (ix != 0 || iy != 0) {
            out.wheel(ix, iy)
            wheelAccX -= ix
            wheelAccY -= iy
        }
    }

    /** 一个手指（`p`）抬起，还剩 `remaining` 个 */
    fun pointerUp(p: P, remaining: Int, t: Long) {
        if (mode == Mode.MULTITOUCH) {
            touchUp(p, UP)
            return
        }
        if (kind == Kind.TWO && remaining == 1) {
            val tap = !twoMoved && maxFingers == 2 && t - twoStartT < TWO_TAP_MS
            if (tap) {
                out.haptic()
                if (mode == Mode.TOUCH) {
                    out.toRemote(twoStartCx, twoStartCy)?.let { setCursor(it.first, it.second) }
                }
                if (mode != Mode.PASSTHROUGH) click(RIGHT)
            } else if (twoKind == 1) {
                startFling()
            }
            // 剩下的那个手指不再算数，等全部抬起
            kind = Kind.DONE
        } else if (kind == Kind.MANY && remaining == 2) {
            if (maxFingers == 3 && t - startT < THREE_TAP_MS) out.toggleKeyboard()
            kind = Kind.DONE
        }
    }

    fun up(p: P, t: Long) {
        if (mode == Mode.MULTITOUCH) {
            touchUp(p, UP)
            // 漏掉的抬起（不应该发生）：全部松开
            for (id in touchIds.keys.toList()) touchUp(P(id, touchPos[id]?.first ?: p.x, touchPos[id]?.second ?: p.y), CANCEL)
            kind = Kind.NONE
            return
        }
        timers.cancel(longTimer)
        longTimer = null
        if (kind == Kind.ONE) {
            if (p.x != lastX || p.y != lastY) moveOne(p, t)
            when (mode) {
                // 多点触摸方式在函数开头已经处理完了
                Mode.MULTITOUCH -> {}
                Mode.TRACKPAD -> {
                    if (dragging) {
                        out.button(LEFT, false)
                    } else if (!moved && !longFired && t - startT < TAP_MS) {
                        click(LEFT)
                        lastTapT = t
                        lastTapX = p.x
                        lastTapY = p.y
                    }
                }
                Mode.TOUCH -> {
                    if (dragging) {
                        out.button(LEFT, false)
                    } else if (!longFired) {
                        out.ripple(startX, startY)
                        click(LEFT)
                    }
                }
                Mode.PASSTHROUGH -> {
                    if (pendingDown) {
                        // 很快的点按：按下、松开一起发
                        pendingDown = false
                        timers.cancel(downTimer)
                        out.ripple(startX, startY)
                        out.toRemote(startX, startY)?.let { out.move(it.first, it.second) }
                        out.button(LEFT, true)
                        out.toRemote(p.x, p.y)?.let { out.move(it.first, it.second) }
                        out.button(LEFT, false)
                    } else if (dragging) {
                        out.toRemote(p.x, p.y)?.let { out.move(it.first, it.second) }
                        out.button(LEFT, false)
                    }
                }
            }
        }
        dragging = false
        kind = Kind.NONE
    }

    fun cancel() {
        for (id in touchIds.keys.toList()) touchUp(P(id, touchPos[id]?.first ?: 0f, touchPos[id]?.second ?: 0f), CANCEL)
        timers.cancel(longTimer)
        timers.cancel(downTimer)
        pendingDown = false
        endDrag()
        kind = Kind.NONE
    }

    private fun touchDown(p: P) {
        if (p.id in touchIds) return
        val slot = (0 until MAX_TOUCH).firstOrNull { it !in touchIds.values } ?: return
        val r = out.toRemote(p.x, p.y) ?: return
        touchIds[p.id] = slot
        touchPos[p.id] = p.x to p.y
        out.touch(slot, DOWN, r.first, r.second)
    }

    private fun touchUp(p: P, phase: String) {
        val slot = touchIds.remove(p.id) ?: return
        touchPos.remove(p.id)
        val r = out.toRemote(p.x, p.y) ?: (0f to 0f)
        out.touch(slot, phase, r.first, r.second)
    }

    private fun endDrag() {
        if (dragging) {
            out.button(LEFT, false)
            dragging = false
        }
    }

    // ───────────── 惯性滚动 ─────────────

    private fun startFling() {
        if (hypot(velX, velY) < FLING_MIN) return
        val tick = object : () -> Unit {
            override fun invoke() {
                velX *= FLING_DECAY
                velY *= FLING_DECAY
                if (hypot(velX, velY) < FLING_STOP) {
                    flingTimer = null
                    return
                }
                wheelAccY += velY * FRAME_MS * WHEEL_PER_PX
                wheelAccX -= velX * FRAME_MS * WHEEL_PER_PX
                flushWheel()
                flingTimer = timers.after(FRAME_MS.toLong(), this)
            }
        }
        flingTimer = timers.after(FRAME_MS.toLong(), tick)
    }

    private fun stopFling() {
        timers.cancel(flingTimer)
        flingTimer = null
    }

    private fun centroid(ps: List<P>): Triple<Float, Float, Float> {
        val a = ps[0]
        val b = ps[1]
        return Triple((a.x + b.x) / 2, (a.y + b.y) / 2, hypot(a.x - b.x, a.y - b.y))
    }

    companion object {
        // 和输入事件里的名字一样（Input，JSON 里是 camelCase）
        const val LEFT = Input.LEFT
        const val RIGHT = Input.RIGHT
        const val DOWN = Input.DOWN
        const val MOVE = Input.MOVE
        const val UP = Input.UP
        const val CANCEL = Input.CANCEL

        /** 同时传给对方的触摸点上限（协议的 MAX_TOUCH_POINTS） */
        const val MAX_TOUCH = 10
        const val LONG_PRESS_MS = 450L
        const val TAP_MS = 350L
        const val DOUBLE_TAP_MS = 300L
        const val TWO_TAP_MS = 300L
        const val THREE_TAP_MS = 400L
        const val PASSTHROUGH_DELAY_MS = 60L

        /** 双指滚动：每像素多少滚轮单位（120 = 一格，约 40 像素一格） */
        const val WHEEL_PER_PX = 3f
        private const val FRAME_MS = 16f
        private const val FLING_MIN = 0.6f
        private const val FLING_STOP = 0.05f
        private const val FLING_DECAY = 0.93f

        /** 指针加速：慢慢移动时精细（0.7×），快速划过时更远（最多 2.4×） */
        fun accel(pxPerMs: Float): Float = min(2.4f, max(0.7f, 0.7f + (pxPerMs - 0.2f) * 0.9f))
    }
}
