package app.airhand.android.controller

import android.os.Handler
import android.os.HandlerThread
import android.view.KeyEvent
import app.airhand.android.core.SystemAction
import org.json.JSONArray
import org.json.JSONObject

/**
 * 输入事件（与 Rust 的 `airhand_proto::session::InputEvent` 的 JSON 形状相同，样例见 ProtocolTest）：
 * `{"type":"mouseMove","x":0.5,"y":0.5}`、`{"type":"releaseAll"}`，名字都是 camelCase
 */
object Input {
    private fun event(type: String): JSONObject = JSONObject().put("type", type)

    /** 移到这个位置（0..1，相对画面） */
    fun move(x: Float, y: Float): JSONObject =
        event("mouseMove").put("x", x.coerceIn(0f, 1f).toDouble()).put("y", y.coerceIn(0f, 1f).toDouble())

    /**
     * 相对移动（被控端的光标隐藏了，例如游戏里转视角）：鼠标的原始计数，约等于被控端的像素。
     * 被控端按真实鼠标的移动注入，游戏自己把光标拉回原位、只看移动了多少
     */
    fun delta(dx: Int, dy: Int): JSONObject = event("mouseDelta").put("dx", dx).put("dy", dy)

    fun button(button: String, down: Boolean): JSONObject = event("mouseButton").put("button", button).put("down", down)

    /** 滚轮：120 = 一格，向上 / 向右为正（Windows 的约定） */
    fun wheel(dx: Int, dy: Int): JSONObject =
        event("wheel").put("dx", dx.coerceIn(-32000, 32000)).put("dy", dy.coerceIn(-32000, 32000))

    /** 物理按键，取值为 W3C `KeyboardEvent.code` */
    fun key(code: String, down: Boolean): JSONObject = event("key").put("code", code).put("down", down)

    fun text(text: String): JSONObject = event("text").put("text", text)

    /** 触摸点：`phase` 是 [DOWN] / [MOVE] / [UP] / [CANCEL]，`id` 小于 10 */
    fun touch(id: Int, phase: String, x: Float, y: Float): JSONObject =
        event("touch").put("id", id).put("phase", phase)
            .put("x", x.coerceIn(0f, 1f).toDouble()).put("y", y.coerceIn(0f, 1f).toDouble())

    /** 系统操作（被控端在 Hello 里列出的） */
    fun system(action: SystemAction): JSONObject = event("system").put("action", action.wire)

    /** 松开所有按着的键和鼠标按钮 */
    fun releaseAll(): JSONObject = event("releaseAll")

    const val DOWN = "down"
    const val MOVE = "move"
    const val UP = "up"
    const val CANCEL = "cancel"

    const val LEFT = "left"
    const val RIGHT = "right"
    const val MIDDLE = "middle"
    const val BACK = "back"
    const val FORWARD = "forward"

    /**
     * 把一个事件加到待发的一批里：紧挨着的绝对移动只留最后一个，相对移动加起来（不越过点击、按键）。
     * 和 Rust 的 `InputEvent::coalesce` 一样
     */
    fun append(pending: MutableList<Any>, e: Any) {
        val last = pending.lastOrNull()
        val type = (e as? JSONObject)?.optString("type")
        val lastType = (last as? JSONObject)?.optString("type")
        if (type == "mouseMove" && lastType == "mouseMove") {
            pending[pending.size - 1] = e
        } else if (type == "mouseDelta" && lastType == "mouseDelta") {
            val a = last as JSONObject
            val b = e as JSONObject
            pending[pending.size - 1] = delta(a.getInt("dx") + b.getInt("dx"), a.getInt("dy") + b.getInt("dy"))
        } else {
            pending.add(e)
        }
    }

    /** 按下再松开一组键（组合键：先按下修饰键，再按下主键，倒序松开） */
    fun chord(codes: List<String>): List<JSONObject> =
        codes.map { key(it, true) } + codes.reversed().map { key(it, false) }
}

/**
 * 把输入事件合并后发出去：连续的绝对移动只保留最后一个、相对移动加起来，最多每 8 ms 发一批。
 * 发送在单独的线程上，触摸处理不会被 JNI 调用拖慢。
 */
class InputSender(private val deliver: (JSONArray) -> Unit) {
    private val thread = HandlerThread("airhand-input").apply { start() }
    private val handler = Handler(thread.looper)
    private val lock = Any()
    private var pending = ArrayList<Any>()
    private var scheduled = false
    private val flushTask = Runnable { flush() }

    fun send(events: List<Any>, now: Boolean = true) {
        if (events.isEmpty()) return
        synchronized(lock) {
            for (e in events) Input.append(pending, e)
            if (now) {
                handler.removeCallbacks(flushTask)
                handler.post(flushTask)
                scheduled = true
            } else if (!scheduled) {
                scheduled = true
                handler.postDelayed(flushTask, 8)
            }
        }
    }

    fun send(vararg events: Any) = send(events.toList())

    private fun flush() {
        val batch = synchronized(lock) {
            scheduled = false
            val b = pending
            pending = ArrayList()
            b
        }
        if (batch.isEmpty()) return
        val a = JSONArray()
        batch.forEach { a.put(it) }
        deliver(a)
    }

    fun release() {
        handler.post { flush() }
        thread.quitSafely()
    }
}

/** Android 按键码 → W3C `KeyboardEvent.code`（外接键盘、软键盘的特殊键） */
object KeyMap {
    private val map: Map<Int, String> = buildMap {
        for (i in 0..25) put(KeyEvent.KEYCODE_A + i, "Key" + ('A' + i))
        for (i in 0..9) put(KeyEvent.KEYCODE_0 + i, "Digit$i")
        for (i in 0..9) put(KeyEvent.KEYCODE_NUMPAD_0 + i, "Numpad$i")
        for (i in 1..12) put(KeyEvent.KEYCODE_F1 + i - 1, "F$i")
        put(KeyEvent.KEYCODE_ENTER, "Enter")
        put(KeyEvent.KEYCODE_NUMPAD_ENTER, "NumpadEnter")
        put(KeyEvent.KEYCODE_DEL, "Backspace")
        put(KeyEvent.KEYCODE_FORWARD_DEL, "Delete")
        put(KeyEvent.KEYCODE_TAB, "Tab")
        put(KeyEvent.KEYCODE_ESCAPE, "Escape")
        put(KeyEvent.KEYCODE_SPACE, "Space")
        put(KeyEvent.KEYCODE_DPAD_LEFT, "ArrowLeft")
        put(KeyEvent.KEYCODE_DPAD_RIGHT, "ArrowRight")
        put(KeyEvent.KEYCODE_DPAD_UP, "ArrowUp")
        put(KeyEvent.KEYCODE_DPAD_DOWN, "ArrowDown")
        put(KeyEvent.KEYCODE_MOVE_HOME, "Home")
        put(KeyEvent.KEYCODE_MOVE_END, "End")
        put(KeyEvent.KEYCODE_PAGE_UP, "PageUp")
        put(KeyEvent.KEYCODE_PAGE_DOWN, "PageDown")
        put(KeyEvent.KEYCODE_INSERT, "Insert")
        put(KeyEvent.KEYCODE_SHIFT_LEFT, "ShiftLeft")
        put(KeyEvent.KEYCODE_SHIFT_RIGHT, "ShiftRight")
        put(KeyEvent.KEYCODE_CTRL_LEFT, "ControlLeft")
        put(KeyEvent.KEYCODE_CTRL_RIGHT, "ControlRight")
        put(KeyEvent.KEYCODE_ALT_LEFT, "AltLeft")
        put(KeyEvent.KEYCODE_ALT_RIGHT, "AltRight")
        put(KeyEvent.KEYCODE_META_LEFT, "MetaLeft")
        put(KeyEvent.KEYCODE_META_RIGHT, "MetaRight")
        put(KeyEvent.KEYCODE_CAPS_LOCK, "CapsLock")
        put(KeyEvent.KEYCODE_NUM_LOCK, "NumLock")
        put(KeyEvent.KEYCODE_SCROLL_LOCK, "ScrollLock")
        put(KeyEvent.KEYCODE_SYSRQ, "PrintScreen")
        put(KeyEvent.KEYCODE_BREAK, "Pause")
        put(KeyEvent.KEYCODE_MENU, "ContextMenu")
        put(KeyEvent.KEYCODE_GRAVE, "Backquote")
        put(KeyEvent.KEYCODE_MINUS, "Minus")
        put(KeyEvent.KEYCODE_EQUALS, "Equal")
        put(KeyEvent.KEYCODE_LEFT_BRACKET, "BracketLeft")
        put(KeyEvent.KEYCODE_RIGHT_BRACKET, "BracketRight")
        put(KeyEvent.KEYCODE_BACKSLASH, "Backslash")
        put(KeyEvent.KEYCODE_SEMICOLON, "Semicolon")
        put(KeyEvent.KEYCODE_APOSTROPHE, "Quote")
        put(KeyEvent.KEYCODE_COMMA, "Comma")
        put(KeyEvent.KEYCODE_PERIOD, "Period")
        put(KeyEvent.KEYCODE_SLASH, "Slash")
        put(KeyEvent.KEYCODE_NUMPAD_ADD, "NumpadAdd")
        put(KeyEvent.KEYCODE_NUMPAD_SUBTRACT, "NumpadSubtract")
        put(KeyEvent.KEYCODE_NUMPAD_MULTIPLY, "NumpadMultiply")
        put(KeyEvent.KEYCODE_NUMPAD_DIVIDE, "NumpadDivide")
        put(KeyEvent.KEYCODE_NUMPAD_DOT, "NumpadDecimal")
    }

    fun code(keyCode: Int): String? = map[keyCode]

    /** 美式键盘上打出这个字符的键：(code, 是否要按 Shift)。用于"Ctrl + 字母"这类组合键 */
    fun forChar(c: Char): Pair<String, Boolean>? = when (c) {
        in 'a'..'z' -> "Key" + c.uppercaseChar() to false
        in 'A'..'Z' -> "Key$c" to true
        in '0'..'9' -> "Digit$c" to false
        ' ' -> "Space" to false
        '\n' -> "Enter" to false
        '\t' -> "Tab" to false
        else -> SYMBOLS[c]
    }

    private val SYMBOLS: Map<Char, Pair<String, Boolean>> = mapOf(
        '`' to ("Backquote" to false), '~' to ("Backquote" to true),
        '-' to ("Minus" to false), '_' to ("Minus" to true),
        '=' to ("Equal" to false), '+' to ("Equal" to true),
        '[' to ("BracketLeft" to false), '{' to ("BracketLeft" to true),
        ']' to ("BracketRight" to false), '}' to ("BracketRight" to true),
        '\\' to ("Backslash" to false), '|' to ("Backslash" to true),
        ';' to ("Semicolon" to false), ':' to ("Semicolon" to true),
        '\'' to ("Quote" to false), '"' to ("Quote" to true),
        ',' to ("Comma" to false), '<' to ("Comma" to true),
        '.' to ("Period" to false), '>' to ("Period" to true),
        '/' to ("Slash" to false), '?' to ("Slash" to true),
        '!' to ("Digit1" to true), '@' to ("Digit2" to true), '#' to ("Digit3" to true),
        '$' to ("Digit4" to true), '%' to ("Digit5" to true), '^' to ("Digit6" to true),
        '&' to ("Digit7" to true), '*' to ("Digit8" to true), '(' to ("Digit9" to true), ')' to ("Digit0" to true),
    )
}
