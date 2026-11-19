package app.airhand.android.ui.session

import androidx.annotation.StringRes
import app.airhand.android.R
import app.airhand.android.data.GameBind
import app.airhand.android.data.GameControl
import app.airhand.android.data.GameKey
import app.airhand.android.data.GameLayout
import app.airhand.android.data.GameStick
import app.airhand.android.data.KeyMode
import kotlin.math.hypot

/**
 * 按钮的位置换算（纯计算，方便测试）：方案里存的是比例和 dp，这里换成视图里的像素。
 * 区域是视图去掉刘海、留一点边的部分（[RemoteView] 设置）
 */
class GameGeometry(val density: Float) {
    var left = 0f
        private set
    var top = 0f
        private set
    var width = 0f
        private set
    var height = 0f
        private set

    fun set(left: Float, top: Float, right: Float, bottom: Float) {
        this.left = left
        this.top = top
        width = (right - left).coerceAtLeast(0f)
        height = (bottom - top).coerceAtLeast(0f)
    }

    val ready: Boolean get() = width > 0f && height > 0f

    fun cx(c: GameControl) = left + c.x * width
    fun cy(c: GameControl) = top + c.y * height
    fun radius(c: GameControl) = radius(c.size)
    fun radius(size: Float) = size * density / 2f

    /** 中心放在 (`px`, `py`)（像素）时存成什么位置（比例）：整个按钮留在区域里 */
    fun place(size: Float, px: Float, py: Float): Pair<Float, Float> {
        val r = radius(size)
        val x = if (width > 2 * r) (px - left).coerceIn(r, width - r) / width else 0.5f
        val y = if (height > 2 * r) (py - top).coerceIn(r, height - r) / height else 0.5f
        return x to y
    }

    /** 按钮被拉回区域里之后的样子（区域变了、按钮变大了） */
    fun inside(c: GameControl): GameControl {
        if (!ready) return c
        val (x, y) = place(c.size, cx(c), cy(c))
        return c.at(x, y)
    }

    /**
     * 手指落在哪个按钮上：按钮外面 [slack] 以内也算（手指比眼睛看到的粗）。
     * 落在几个按钮之间时取离中心最近的（按各自的大小算）
     */
    fun hit(controls: List<GameControl>, x: Float, y: Float, slack: (GameControl) -> Float): GameControl? {
        var best: GameControl? = null
        var bestK = Float.MAX_VALUE
        for (c in controls) {
            val reach = radius(c) + slack(c)
            val k = hypot(x - cx(c), y - cy(c)) / reach
            if (k <= 1f && k < bestK) {
                best = c
                bestK = k
            }
        }
        return best
    }
}

/**
 * 游戏按键（纯逻辑，方便测试；[RemoteView] 把落在按钮上的手指交给这里，其余的照常交给 [Gestures]）。
 *
 * * 按钮：按住就按着键（组合键按顺序按下、倒序松开），或者点一下切换；滚轮按着会一格一格地转下去；
 *   可以转视角的按钮（开火）按着拖动时发相对移动，一边开火一边瞄准；
 * * 摇杆：8 个方向，方向边界和死区都有回差，手指在边界上抖不会让键来回按；推出边缘再往外加按一个键（奔跑）；
 * * 同一个键被几处按着（摇杆的 W 和一个 W 按钮）时数着：都松开才松开，不会提前松掉；
 * * 一个手指从按下到抬起都归同一个按钮：开火时手指滑出按钮也不会松开。
 *
 * 按钮以外的东西都要 [releaseAll]：换方案、收起、窗口失去焦点（菜单、通知栏、切到后台）、断线
 */
class GameControls(
    private val out: Out,
    private val timers: Gestures.Timers,
    val geo: GameGeometry,
    private val slop: Float,
) {
    interface Out {
        fun key(code: String, down: Boolean)
        fun button(button: String, down: Boolean)
        fun wheel(dy: Int)

        /** 转视角：手指移动了多少（视图像素），由接收方乘上灵敏度、换算成被控端的移动量 */
        fun look(dx: Float, dy: Float)
        fun haptic()

        /** 要重画（按下、松开、摇杆动了）；按着的鼠标键也可能变了（[holdsMouse]） */
        fun changed()
    }

    /** 显示的方案；null 表示没有显示（不接手指）。换了就先把按着的都松开 */
    var layout: GameLayout? = null
        set(v) {
            if (field == v) return
            releaseAll()
            field = v
            out.changed()
        }

    private class KeyTouch(val key: GameKey, val x0: Float, val y0: Float) {
        var looking = false
        var lx = 0f
        var ly = 0f
        var repeat: Any? = null
    }

    /** 摇杆上的手指：(`cx`, `cy`) 是这一次的中心（浮动摇杆是按下的地方），(`x`, `y`) 是手指 */
    private class StickTouch(val stick: GameStick, val cx: Float, val cy: Float) {
        var x = cx
        var y = cy
        val dirs = BooleanArray(4)
        var sprint = false
    }

    /** 手指（MotionEvent 的 pointer id）→ 它按着的按钮 */
    private val touches = HashMap<Int, Any>()

    /** 切换着的按钮 → 它按下的键 */
    private val latched = HashMap<Int, List<String>>()

    /** 按着的键 → 几处按着 */
    private val held = HashMap<String, Int>()

    /** 这个手指归游戏按键 */
    fun owns(pointer: Int) = pointer in touches

    /** 按着鼠标键（被控端隐藏光标时要一直发相对移动，见 RemoteView.updateRelative） */
    val holdsMouse: Boolean get() = held.keys.any { GameBind.mouseButton(it) != null }

    /** 这个按钮现在是按下的样子（按着，或者切换着） */
    fun isDown(id: Int): Boolean =
        id in latched || touches.values.any { it is KeyTouch && it.key.id == id && it.key.mode == KeyMode.HOLD }

    /** 摇杆现在的样子（画出来用）；没人按着时为 null */
    class StickState(val cx: Float, val cy: Float, val kx: Float, val ky: Float, val dirs: BooleanArray, val sprint: Boolean)

    fun stickState(id: Int): StickState? {
        val t = touches.values.firstOrNull { it is StickTouch && it.stick.id == id } as? StickTouch ?: return null
        val r = geo.radius(t.stick)
        val dx = t.x - t.cx
        val dy = t.y - t.cy
        val d = hypot(dx, dy)
        val k = if (d > r) r / d else 1f
        return StickState(t.cx, t.cy, t.cx + dx * k, t.cy + dy * k, t.dirs.copyOf(), t.sprint)
    }

    /** 一个手指按下。落在按钮上就归这里（返回 true），否则交给手势 */
    fun down(pointer: Int, x: Float, y: Float): Boolean {
        val l = layout ?: return false
        if (!geo.ready || pointer in touches) return false
        val c = geo.hit(l.controls, x, y, ::slack) ?: return false
        // 摇杆同时只认一个手指：第二个手指照常操作画面
        if (c is GameStick && touches.values.any { it is StickTouch && it.stick.id == c.id }) return false
        when (c) {
            is GameKey -> {
                val t = KeyTouch(c, x, y)
                touches[pointer] = t
                if (c.mode == KeyMode.TOGGLE) {
                    val was = latched.remove(c.id)
                    if (was != null) {
                        releaseBinds(was)
                    } else {
                        latched[c.id] = c.binds
                        pressBinds(c.binds)
                    }
                } else {
                    pressBinds(c.binds)
                    if (c.binds.any { GameBind.wheel(it) != 0 }) t.repeat = timers.after(WHEEL_DELAY_MS) { repeatWheel(pointer, t) }
                }
            }
            is GameStick -> {
                val t = if (c.floating) StickTouch(c, x, y) else StickTouch(c, geo.cx(c), geo.cy(c))
                touches[pointer] = t
                steer(t, x, y)
            }
        }
        if (l.haptics) out.haptic()
        out.changed()
        return true
    }

    fun move(pointer: Int, x: Float, y: Float) {
        when (val t = touches[pointer]) {
            is StickTouch -> if (t.x != x || t.y != y) {
                steer(t, x, y)
                out.changed()
            }
            is KeyTouch -> if (t.key.look) {
                if (!t.looking) {
                    // 过了一点距离才算拖动（按下时手指的一点抖动不转视角），从这里开始算，不跳
                    if (hypot(x - t.x0, y - t.y0) < slop) return
                    t.looking = true
                } else if (x != t.lx || y != t.ly) {
                    out.look(x - t.lx, y - t.ly)
                }
                t.lx = x
                t.ly = y
            }
        }
    }

    /** 一个手指抬起。返回它是不是归这里的 */
    fun up(pointer: Int): Boolean {
        val t = touches.remove(pointer) ?: return false
        endTouch(t)
        out.changed()
        return true
    }

    /** 松开手指按着的（触摸被打断）；切换着的不动 */
    fun lift() {
        if (touches.isEmpty()) return
        touches.values.toList().forEach(::endTouch)
        touches.clear()
        out.changed()
    }

    /** 全部松开：手指按着的、切换着的，再加上万一漏掉的 */
    fun releaseAll() {
        touches.values.toList().forEach(::endTouch)
        touches.clear()
        latched.values.toList().forEach(::releaseBinds)
        latched.clear()
        for (token in held.keys.toList()) emit(token, false)
        held.clear()
        out.changed()
    }

    private fun endTouch(t: Any) {
        when (t) {
            is KeyTouch -> {
                timers.cancel(t.repeat)
                t.repeat = null
                if (t.key.mode == KeyMode.HOLD) releaseBinds(t.key.binds)
            }
            is StickTouch -> {
                if (t.sprint) t.stick.sprint?.let(::release)
                for (i in 3 downTo 0) if (t.dirs[i]) release(t.stick.keys[i])
                t.sprint = false
                t.dirs.fill(false)
            }
        }
    }

    private fun repeatWheel(pointer: Int, t: KeyTouch) {
        if (touches[pointer] !== t) return
        t.key.binds.forEach { GameBind.wheel(it).takeIf { w -> w != 0 }?.let(out::wheel) }
        t.repeat = timers.after(WHEEL_REPEAT_MS) { repeatWheel(pointer, t) }
    }

    /** 按钮外面多宽也算按到：摇杆的范围大一些（浮动摇杆不用正好按在圆里） */
    private fun slack(c: GameControl): Float {
        val r = geo.radius(c)
        return if (c is GameStick) r * 0.3f else maxOf(r * 0.15f, 6 * geo.density)
    }

    /** 摇杆：按手指相对中心的方向决定按着哪几个方向键 */
    private fun steer(t: StickTouch, x: Float, y: Float) {
        t.x = x
        t.y = y
        val r = geo.radius(t.stick)
        val vx = x - t.cx
        val vy = y - t.cy
        val d = hypot(vx, vy)
        val active = t.dirs.any { it }
        val want = BooleanArray(4)
        if (d > r * (if (active) DEAD_OUT else DEAD_IN)) {
            // 上、左、下、右方向上的分量（单位向量）
            val comp = floatArrayOf(-vy / d, -vx / d, vy / d, vx / d)
            for (i in 0..3) want[i] = comp[i] > if (t.dirs[i]) AXIS_OUT else AXIS_IN
        }
        val sprintKey = t.stick.sprint
        val sprint = sprintKey != null && want.any { it } && d > r * (if (t.sprint) SPRINT_OUT else SPRINT_IN)
        // 先松开再按下：手指一下子从上甩到下时先松 W 再按 S，不会两个一起按着
        if (t.sprint && !sprint) sprintKey?.let(::release)
        for (i in 0..3) if (t.dirs[i] && !want[i]) release(t.stick.keys[i])
        for (i in 0..3) if (!t.dirs[i] && want[i]) press(t.stick.keys[i])
        if (!t.sprint && sprint) {
            press(sprintKey)
            if (layout?.haptics != false) out.haptic()
        }
        want.copyInto(t.dirs)
        t.sprint = sprint
    }

    private fun pressBinds(binds: List<String>) {
        for (b in binds) {
            val w = GameBind.wheel(b)
            if (w != 0) out.wheel(w) else press(b)
        }
    }

    private fun releaseBinds(binds: List<String>) {
        for (b in binds.asReversed()) if (GameBind.wheel(b) == 0) release(b)
    }

    private fun press(token: String) {
        val n = held[token] ?: 0
        held[token] = n + 1
        if (n == 0) emit(token, true)
    }

    private fun release(token: String) {
        val n = held[token] ?: return
        if (n <= 1) {
            held.remove(token)
            emit(token, false)
        } else {
            held[token] = n - 1
        }
    }

    private fun emit(token: String, down: Boolean) {
        val b = GameBind.mouseButton(token)
        if (b != null) out.button(b, down) else out.key(token, down)
    }

    companion object {
        /** 死区（摇杆半径的比例）：推过 [DEAD_IN] 才算，回到 [DEAD_OUT] 以内才松开 */
        const val DEAD_IN = 0.25f
        const val DEAD_OUT = 0.18f

        /** 方向分量超过 sin 22.5° 按下（8 个方向各 45°），回到 [AXIS_OUT] 以下才松开 */
        const val AXIS_IN = 0.383f
        const val AXIS_OUT = 0.30f

        /** 推出边缘再往外（半径的倍数）加按，回到边缘以内松开 */
        const val SPRINT_IN = 1.15f
        const val SPRINT_OUT = 1.0f

        /** 按着滚轮按钮：多久之后开始一格一格地转，之后每格隔多久 */
        const val WHEEL_DELAY_MS = 350L
        const val WHEEL_REPEAT_MS = 110L
    }
}

/** 选键时列出的键，和按钮上显示的键名 */
object GameKeys {
    data class Group(@StringRes val title: Int, val tokens: List<String>)

    val groups: List<Group> = listOf(
        Group(R.string.game_group_mouse, GameBind.MOUSE_BUTTONS.map(GameBind::mouse) + listOf(GameBind.WHEEL_UP, GameBind.WHEEL_DOWN)),
        Group(
            R.string.game_group_common,
            listOf("Space", "ShiftLeft", "ControlLeft", "AltLeft", "Tab", "Escape", "Enter", "Backspace", "CapsLock", "Backquote", "ShiftRight", "ControlRight", "AltRight"),
        ),
        Group(R.string.game_group_letters, "QWERTYUIOPASDFGHJKLZXCVBNM".map { "Key$it" }),
        Group(R.string.game_group_digits, (1..9).map { "Digit$it" } + listOf("Digit0", "Minus", "Equal")),
        Group(R.string.game_group_function, (1..12).map { "F$it" }),
        Group(R.string.game_group_nav, listOf("ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Insert", "Delete", "Home", "End", "PageUp", "PageDown")),
        Group(R.string.game_group_symbols, listOf("BracketLeft", "BracketRight", "Backslash", "Semicolon", "Quote", "Comma", "Period", "Slash")),
        Group(
            R.string.game_group_numpad,
            (0..9).map { "Numpad$it" } + listOf("NumpadAdd", "NumpadSubtract", "NumpadMultiply", "NumpadDivide", "NumpadDecimal", "NumpadEnter"),
        ),
    )

    private val NAMES = mapOf(
        "Space" to "Space", "ShiftLeft" to "Shift", "ShiftRight" to "R-Shift", "ControlLeft" to "Ctrl", "ControlRight" to "R-Ctrl",
        "AltLeft" to "Alt", "AltRight" to "R-Alt", "MetaLeft" to "Win", "MetaRight" to "R-Win", "Tab" to "Tab", "Escape" to "Esc",
        "Enter" to "Enter", "Backspace" to "⌫", "CapsLock" to "Caps", "Backquote" to "`", "Minus" to "-", "Equal" to "=",
        "ArrowUp" to "↑", "ArrowDown" to "↓", "ArrowLeft" to "←", "ArrowRight" to "→", "Insert" to "Ins", "Delete" to "Del",
        "Home" to "Home", "End" to "End", "PageUp" to "PgUp", "PageDown" to "PgDn", "BracketLeft" to "[", "BracketRight" to "]",
        "Backslash" to "\\", "Semicolon" to ";", "Quote" to "'", "Comma" to ",", "Period" to ".", "Slash" to "/",
        "NumpadAdd" to "N+", "NumpadSubtract" to "N-", "NumpadMultiply" to "N*", "NumpadDivide" to "N/", "NumpadDecimal" to "N.",
        "NumpadEnter" to "N-Enter", "PrintScreen" to "PrtSc", "ScrollLock" to "ScrLk", "Pause" to "Pause", "NumLock" to "NumLk",
        "ContextMenu" to "Menu",
    )

    /** 一个键的名字（鼠标键、滚轮用 [text] 取本地化的文字） */
    fun label(token: String, text: (Int) -> String): String {
        GameBind.mouseButton(token)?.let {
            return text(
                when (it) {
                    "left" -> R.string.game_mouse_left
                    "right" -> R.string.game_mouse_right
                    "middle" -> R.string.game_mouse_middle
                    "back" -> R.string.game_mouse_back
                    else -> R.string.game_mouse_forward
                },
            )
        }
        when (token) {
            GameBind.WHEEL_UP -> return text(R.string.game_wheel_up)
            GameBind.WHEEL_DOWN -> return text(R.string.game_wheel_down)
        }
        NAMES[token]?.let { return it }
        return when {
            token.startsWith("Key") && token.length == 4 -> token.substring(3)
            token.startsWith("Digit") && token.length == 6 -> token.substring(5)
            token.startsWith("Numpad") && token.length == 7 -> "N" + token.substring(6)
            else -> token
        }
    }

    /** 组合键的名字：Ctrl+1 */
    fun label(binds: List<String>, text: (Int) -> String): String = binds.joinToString("+") { label(it, text) }

    /** 按钮上显示的字：自己写的，或者按键名 */
    fun caption(k: GameKey, text: (Int) -> String): String = k.label.ifBlank { label(k.binds, text) }

    /** 摇杆四个方向的名字（上、左、下、右）：W A S D，方向键用箭头 */
    fun stickLabels(s: GameStick, text: (Int) -> String): List<String> = s.keys.map { label(it, text) }

    /** 摇杆是哪一种：WASD / 方向键 / 别的 */
    fun stickName(s: GameStick, text: (Int) -> String): String = when (s.keys) {
        GameBind.WASD -> "WASD"
        GameBind.ARROWS -> "↑←↓→"
        else -> stickLabels(s, text).joinToString("")
    }

    /** 列表里一个方案的简介：前几个按钮的名字 */
    fun summary(l: GameLayout, text: (Int) -> String): String =
        l.controls.joinToString(" · ") {
            when (it) {
                is GameKey -> caption(it, text)
                is GameStick -> text(R.string.game_stick) + " " + stickName(it, text)
            }
        }

}
