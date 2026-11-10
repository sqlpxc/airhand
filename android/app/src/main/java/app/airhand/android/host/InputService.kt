package app.airhand.android.host

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.ComponentName
import android.content.Context
import android.graphics.Path
import android.media.AudioManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.provider.Settings
import android.text.TextUtils
import android.util.Log
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject

/**
 * 远程操作：用无障碍服务把控制端的鼠标 / 键盘变成手机上的手势和操作。
 *
 * Android 不允许普通应用直接注入触摸事件，无障碍服务是唯一的办法（需要用户在系统设置里手动打开）。
 * 这个服务不读取任何界面内容，只在有人远程操作时执行手势、全局操作（返回、主页……）和输入文字。
 */
class InputService : AccessibilityService() {
    private var injector: Injector? = null

    override fun onServiceConnected() {
        super.onServiceConnected()
        injector = Injector(this)
        instance = this
        _connected.value = true
        Log.i(TAG, "accessibility service connected")
    }

    override fun onDestroy() {
        disconnect()
        super.onDestroy()
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        disconnect()
        return super.onUnbind(intent)
    }

    private fun disconnect() {
        if (instance === this) instance = null
        injector?.release()
        injector = null
        _connected.value = false
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}

    companion object {
        private const val TAG = "airhand"

        @Volatile
        private var instance: InputService? = null
        private val _connected = MutableStateFlow(false)

        /** 无障碍服务已经开着（可以远程操作） */
        val connected: StateFlow<Boolean> = _connected.asStateFlow()

        fun inject(events: JSONArray) {
            instance?.injector?.handle(events)
        }

        /** 系统设置里有没有打开（服务可能还没连上） */
        fun isEnabled(context: Context): Boolean {
            if (_connected.value) return true
            val me = ComponentName(context, InputService::class.java)
            val list = Settings.Secure.getString(context.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: return false
            val split = TextUtils.SimpleStringSplitter(':')
            split.setString(list)
            return split.any { ComponentName.unflattenFromString(it) == me }
        }
    }
}

/**
 * 把输入事件变成手势 / 操作（在单独的线程上按顺序执行）。
 *
 * * **触摸点**（手机控制手机）和**鼠标左键**（电脑控制手机）都是"手指"：按下、移动、抬起对应连续的手势
 *   （Android 8+ 的 `continueStroke`），几个手指同时按着就是多点触控，可以拖动、长按、双指缩放；
 * * **系统操作**（返回、主页、最近任务、通知栏……）：控制端的导航键和菜单发来的；
 * * 方便电脑上的人：右键 / 侧键"后退" / Esc = 返回，中键 / 单按 Win = 主页，Alt+Tab = 最近任务；
 * * 滚轮 = 在指针处滑动；
 * * 打字：文字直接写进当前的输入框（Android 13+ 用无障碍输入法接口，更早的版本改输入框的内容）；
 *   桌面控制端按的是物理键，可打印的键按美式键盘换算成字符。
 */
class Injector(private val svc: AccessibilityService) {
    private val thread = HandlerThread("airhand-inject").apply { start() }
    private val handler = Handler(thread.looper)

    // 鼠标指针（像素）
    private var x = 0f
    private var y = 0f
    private var w = 1
    private var h = 1

    /** 一个按着的手指 */
    private class Finger(px: Float, py: Float, val downAt: Long) {
        /** 上一段手势里它那一笔（之后的段落接着它画） */
        var stroke: GestureDescription.StrokeDescription? = null

        /** 上一段之后走过的路径 */
        var path = Path().apply { moveTo(px, py) }

        /** Android 7：没有连续手势，整笔记下来，全部抬起时一次发出 */
        val whole = Path().apply { moveTo(px, py) }
        var lastX = px
        var lastY = py
        var lifted = false
        var upAt = 0L

        fun moveTo(px: Float, py: Float) {
            if (px == lastX && py == lastY) return
            path.lineTo(px, py)
            whole.lineTo(px, py)
            lastX = px
            lastY = py
        }
    }

    /** 按着的手指：触摸点 0..9，鼠标左键是 [MOUSE] */
    private val fingers = LinkedHashMap<Int, Finger>()
    private var dirty = false
    private var lastDispatch = 0L
    private val flushTask = Runnable { flush() }

    // 滚轮：一次只执行一个滑动，执行期间的滚动累积起来
    private var wheelBusy = false
    private var wheelDx = 0
    private var wheelDy = 0

    private val pressed = HashSet<String>()
    private var metaAlone = false
    private var capsLock = false

    fun handle(events: JSONArray) {
        handler.post {
            screen()
            for (i in 0 until events.length()) {
                try {
                    (events.get(i) as? JSONObject)?.let { event(it) }
                } catch (ex: Exception) {
                    Log.w(TAG, "can't inject an event: $ex")
                }
            }
            if (dirty) schedule()
        }
    }

    fun release() {
        handler.post { releaseAll() }
        thread.quitSafely()
    }

    private fun screen() {
        val p = ScreenCapture.screen(svc)
        w = p.x.coerceAtLeast(1)
        h = p.y.coerceAtLeast(1)
    }

    private fun px(nx: Double) = (nx.toFloat() * w).coerceIn(0f, (w - 1).toFloat())
    private fun py(ny: Double) = (ny.toFloat() * h).coerceIn(0f, (h - 1).toFloat())

    /**
     * 一个输入事件（Rust 的 `airhand_proto::session::InputEvent`，JSON 以 `type` 区分，名字都是 camelCase）。
     * 不认识的事件、按钮、系统操作不执行
     */
    private fun event(e: JSONObject) {
        when (e.optString("type")) {
            "mouseMove" -> {
                x = px(e.optDouble("x"))
                y = py(e.optDouble("y"))
                fingers[MOUSE]?.let { f -> move(f, x, y) }
            }
            // 相对移动（控制端只在被控端的光标隐藏时发，手机没有光标；收到了就当作按像素移动）
            "mouseDelta" -> {
                x = (x + e.optInt("dx")).coerceIn(0f, (w - 1).toFloat())
                y = (y + e.optInt("dy")).coerceIn(0f, (h - 1).toFloat())
                fingers[MOUSE]?.let { f -> move(f, x, y) }
            }
            "touch" -> {
                val id = e.optInt("id")
                if (id !in 0 until MAX_FINGERS) return
                val tx = px(e.optDouble("x"))
                val ty = py(e.optDouble("y"))
                when (e.optString("phase")) {
                    "down" -> fingerDown(id, tx, ty)
                    "move" -> fingers[id]?.let { f -> move(f, tx, ty) }
                    "up", "cancel" -> fingers[id]?.let { f -> lift(f, tx, ty) }
                }
            }
            "mouseButton" -> {
                val isDown = e.optBoolean("down")
                when (e.optString("button")) {
                    "left" -> if (isDown) fingerDown(MOUSE, x, y) else fingers[MOUSE]?.let { f -> lift(f, x, y) }
                    "right", "back" -> if (!isDown) global(AccessibilityService.GLOBAL_ACTION_BACK)
                    "middle" -> if (!isDown) global(AccessibilityService.GLOBAL_ACTION_HOME)
                }
            }
            "system" -> system(e.optString("action"))
            "wheel" -> {
                wheelDx += e.optInt("dx")
                wheelDy += e.optInt("dy")
                if (!wheelBusy && fingers.isEmpty()) scroll()
            }
            "key" -> key(e.optString("code"), e.optBoolean("down"))
            "text" -> insertText(e.optString("text"))
            "releaseAll" -> releaseAll()
        }
    }

    // ───────────────────────────── 手指 ─────────────────────────────

    private fun fingerDown(id: Int, px: Float, py: Float) {
        fingers[id]?.let { lift(it, it.lastX, it.lastY) }
        // 同时最多 10 个手指（系统手势的上限）
        if (fingers.count { !it.value.lifted } >= GestureDescription.getMaxStrokeCount()) return
        val now = SystemClock.uptimeMillis()
        if (fingers.isEmpty()) lastDispatch = now
        fingers[id] = Finger(px, py, now)
        dirty = true
    }

    private fun move(f: Finger, px: Float, py: Float) {
        if (f.lifted) return
        f.moveTo(px, py)
        dirty = true
    }

    private fun lift(f: Finger, px: Float, py: Float) {
        if (f.lifted) return
        f.moveTo(px, py)
        f.lifted = true
        f.upAt = SystemClock.uptimeMillis()
        dirty = true
    }

    /** 太密的变化合并起来：两段手势之间至少隔 16 ms；有手指按下、抬起时马上发 */
    private fun schedule() {
        handler.removeCallbacks(flushTask)
        val wait = lastDispatch + SEGMENT_MS - SystemClock.uptimeMillis()
        val urgent = fingers.values.any { it.lifted || it.stroke == null }
        if (urgent || wait <= 0) flush() else handler.postDelayed(flushTask, wait)
    }

    /** 把所有手指这一段的动作作为一个手势发出去 */
    private fun flush() {
        handler.removeCallbacks(flushTask)
        if (!dirty || fingers.isEmpty()) return
        dirty = false
        val now = SystemClock.uptimeMillis()
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
            flushWhole(now)
            return
        }
        // 这一段的时长：距上一段的时间，但不超过 SEGMENT_MAX_MS。手指停着不动的那段时间里系统一直按着它，
        // 不需要再"重放"一遍（否则长按之后抬起会被推迟同样长的时间）
        val duration = (now - lastDispatch).coerceIn(1, SEGMENT_MAX_MS)
        val b = GestureDescription.Builder()
        val strokes = ArrayList<Pair<Finger, GestureDescription.StrokeDescription>>()
        for (f in fingers.values) {
            val willContinue = !f.lifted
            val s = try {
                f.stroke?.continueStroke(f.path, 0, duration, willContinue)
                    ?: GestureDescription.StrokeDescription(f.path, 0, duration, willContinue)
            } catch (e: Exception) {
                Log.w(TAG, "bad stroke: $e")
                continue
            }
            b.addStroke(s)
            strokes += f to s
        }
        if (strokes.isNotEmpty()) svc.dispatchGesture(b.build(), null, null)
        lastDispatch = now
        for ((f, s) in strokes) {
            f.stroke = if (f.lifted) null else s
            f.path = Path().apply { moveTo(f.lastX, f.lastY) }
        }
        fingers.values.removeAll { it.lifted }
        if (fingers.isEmpty() && (wheelDx != 0 || wheelDy != 0)) scroll()
    }

    /** Android 7：所有手指都抬起时，把整个手势（每笔从按下到抬起）一次发出去 */
    private fun flushWhole(now: Long) {
        if (fingers.values.any { !it.lifted }) return
        val start = fingers.values.minOf { it.downAt }
        val max = GestureDescription.getMaxGestureDuration() - 1
        val b = GestureDescription.Builder()
        for (f in fingers.values) {
            val offset = (f.downAt - start).coerceIn(0, max - 1)
            val duration = (f.upAt - f.downAt).coerceIn(1, max - offset)
            runCatching { b.addStroke(GestureDescription.StrokeDescription(f.whole, offset, duration)) }
        }
        runCatching { svc.dispatchGesture(b.build(), null, null) }
        lastDispatch = now
        fingers.clear()
    }

    private fun scroll() {
        val dy = wheelDy
        val dx = wheelDx
        wheelDx = 0
        wheelDy = 0
        if (dx == 0 && dy == 0) return
        // 一格（120）滑过屏幕高度的 1/8；向上滚 = 手指往下滑
        val step = h / 8f
        val tx = (x + dx / 120f * step).coerceIn(1f, w - 2f)
        val ty = (y + dy / 120f * step).coerceIn(1f, h - 2f)
        val sx = x.coerceIn(1f, w - 2f)
        val sy = y.coerceIn(1f, h - 2f)
        if (tx == sx && ty == sy) return
        val p = Path().apply { moveTo(sx, sy); lineTo(tx, ty) }
        wheelBusy = true
        val ok = svc.dispatchGesture(
            GestureDescription.Builder().addStroke(GestureDescription.StrokeDescription(p, 0, WHEEL_MS)).build(),
            object : AccessibilityService.GestureResultCallback() {
                override fun onCompleted(d: GestureDescription?) = next()
                override fun onCancelled(d: GestureDescription?) = next()
                private fun next() {
                    handler.post {
                        wheelBusy = false
                        if (fingers.isEmpty() && (wheelDx != 0 || wheelDy != 0)) scroll()
                    }
                }
            },
            handler,
        )
        if (!ok) wheelBusy = false
    }

    private fun releaseAll() {
        for (f in fingers.values) lift(f, f.lastX, f.lastY)
        if (dirty) flush()
        fingers.clear()
        pressed.clear()
        metaAlone = false
    }

    private fun global(action: Int) {
        svc.performGlobalAction(action)
    }

    /** 控制端发来的系统操作（只会是 [supportedActions] 里列出的） */
    private fun system(action: String) {
        when (action) {
            "back" -> global(AccessibilityService.GLOBAL_ACTION_BACK)
            "home" -> global(AccessibilityService.GLOBAL_ACTION_HOME)
            "recents" -> global(AccessibilityService.GLOBAL_ACTION_RECENTS)
            "notifications" -> global(AccessibilityService.GLOBAL_ACTION_NOTIFICATIONS)
            "quickSettings" -> global(AccessibilityService.GLOBAL_ACTION_QUICK_SETTINGS)
            "powerMenu" -> global(AccessibilityService.GLOBAL_ACTION_POWER_DIALOG)
            "screenshot" -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                global(AccessibilityService.GLOBAL_ACTION_TAKE_SCREENSHOT)
            }
            "lockScreen" -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                global(AccessibilityService.GLOBAL_ACTION_LOCK_SCREEN)
            }
            "volumeUp" -> volume(AudioManager.ADJUST_RAISE)
            "volumeDown" -> volume(AudioManager.ADJUST_LOWER)
        }
    }

    // ───────────────────────────── 键盘 ─────────────────────────────

    private fun has(vararg codes: String) = codes.any { it in pressed }
    private val ctrl get() = has("ControlLeft", "ControlRight")
    private val alt get() = has("AltLeft", "AltRight")
    private val shift get() = has("ShiftLeft", "ShiftRight")
    private val meta get() = has("MetaLeft", "MetaRight", "OSLeft", "OSRight")

    private fun key(code: String, isDown: Boolean) {
        val isMeta = code == "MetaLeft" || code == "MetaRight" || code == "OSLeft" || code == "OSRight"
        if (!isDown) {
            pressed.remove(code)
            // 单按一下 Win（中间没按别的键）= 主页
            if (isMeta && metaAlone) global(AccessibilityService.GLOBAL_ACTION_HOME)
            if (isMeta) metaAlone = false
            return
        }
        if (code in pressed && isMeta) return
        pressed.add(code)
        if (isMeta) {
            metaAlone = true
            return
        }
        if (code in MODIFIERS) return
        if (meta) metaAlone = false
        if (code == "CapsLock") {
            capsLock = !capsLock
            return
        }

        // 电脑上的人顺手按的键（手机的导航键、菜单发的是系统操作，不靠这些）
        when {
            alt && code == "Tab" -> return global(AccessibilityService.GLOBAL_ACTION_RECENTS)
            code == "Escape" || code == "BrowserBack" -> return global(AccessibilityService.GLOBAL_ACTION_BACK)
            code == "BrowserHome" -> return global(AccessibilityService.GLOBAL_ACTION_HOME)
            code == "AudioVolumeUp" -> return volume(AudioManager.ADJUST_RAISE)
            code == "AudioVolumeDown" -> return volume(AudioManager.ADJUST_LOWER)
            code == "AudioVolumeMute" -> return volume(AudioManager.ADJUST_TOGGLE_MUTE)
        }
        if (ctrl && !alt) {
            when (code) {
                "KeyC" -> return nodeAction(AccessibilityNodeInfo.ACTION_COPY)
                "KeyV" -> return nodeAction(AccessibilityNodeInfo.ACTION_PASTE)
                "KeyX" -> return nodeAction(AccessibilityNodeInfo.ACTION_CUT)
                "KeyA" -> return selectAll()
            }
        }
        if (!ctrl && !alt && !meta) {
            charFor(code, shift)?.let { return insertText(it.toString()) }
        }
        EDIT_KEYS[code]?.let { editKey(it) }
    }

    private fun volume(dir: Int) {
        val am = svc.getSystemService(Context.AUDIO_SERVICE) as AudioManager
        am.adjustStreamVolume(AudioManager.STREAM_MUSIC, dir, AudioManager.FLAG_SHOW_UI)
    }

    /** 美式键盘：物理键 → 字符 */
    private fun charFor(code: String, shift: Boolean): Char? {
        if (code.startsWith("Key") && code.length == 4) {
            val c = code[3]
            return if (shift xor capsLock) c.uppercaseChar() else c.lowercaseChar()
        }
        if (code.startsWith("Digit") && code.length == 6) {
            return if (shift) ")!@#$%^&*("[code[5] - '0'] else code[5]
        }
        if (code.startsWith("Numpad") && code.length == 7 && code[6].isDigit()) return code[6]
        val pair = SYMBOLS[code] ?: return null
        return if (shift) pair.second else pair.first
    }

    // ───────────────────────────── 文字 ─────────────────────────────

    private fun focused(): AccessibilityNodeInfo? =
        svc.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { it.isEditable }

    private fun insertText(text: String) {
        if (text.isEmpty()) return
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            svc.inputMethod?.currentInputConnection?.let {
                it.commitText(text, 1, null)
                return
            }
        }
        val node = focused() ?: return
        val (cur, s, e) = textState(node)
        setText(node, cur.substring(0, s) + text + cur.substring(e), s + text.length)
    }

    /** 输入框当前的文字和选区 */
    private fun textState(node: AccessibilityNodeInfo): Triple<String, Int, Int> {
        val hint = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && node.isShowingHintText
        val cur = if (hint) "" else node.text?.toString() ?: ""
        var s = node.textSelectionStart
        var e = node.textSelectionEnd
        if (s < 0 || e < 0 || s > cur.length || e > cur.length) {
            s = cur.length
            e = cur.length
        }
        return Triple(cur, minOf(s, e), maxOf(s, e))
    }

    private fun setText(node: AccessibilityNodeInfo, text: String, cursor: Int) {
        node.performAction(
            AccessibilityNodeInfo.ACTION_SET_TEXT,
            Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text) },
        )
        setSelection(node, cursor, cursor)
    }

    private fun setSelection(node: AccessibilityNodeInfo, start: Int, end: Int) {
        node.performAction(
            AccessibilityNodeInfo.ACTION_SET_SELECTION,
            Bundle().apply {
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, start)
                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, end)
            },
        )
    }

    private fun nodeAction(action: Int) {
        focused()?.performAction(action)
    }

    private fun selectAll() {
        val node = focused() ?: return
        setSelection(node, 0, textState(node).first.length)
    }

    /** 编辑键（退格、删除、回车、方向键……） */
    private fun editKey(keyCode: Int) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            svc.inputMethod?.currentInputConnection?.let { ic ->
                val now = SystemClock.uptimeMillis()
                val meta = if (shift) KeyEvent.META_SHIFT_ON else 0
                ic.sendKeyEvent(KeyEvent(now, now, KeyEvent.ACTION_DOWN, keyCode, 0, meta))
                ic.sendKeyEvent(KeyEvent(now, now, KeyEvent.ACTION_UP, keyCode, 0, meta))
                return
            }
        }
        // 更早的系统：直接改输入框
        val node = focused() ?: return
        val (cur, s, e) = textState(node)
        when (keyCode) {
            KeyEvent.KEYCODE_DEL -> when {
                s != e -> setText(node, cur.removeRange(s, e), s)
                s > 0 -> {
                    val from = if (s >= 2 && Character.isSurrogatePair(cur[s - 2], cur[s - 1])) s - 2 else s - 1
                    setText(node, cur.removeRange(from, s), from)
                }
            }
            KeyEvent.KEYCODE_FORWARD_DEL -> when {
                s != e -> setText(node, cur.removeRange(s, e), s)
                s < cur.length -> {
                    val to = if (s + 1 < cur.length && Character.isSurrogatePair(cur[s], cur[s + 1])) s + 2 else s + 1
                    setText(node, cur.removeRange(s, to), s)
                }
            }
            KeyEvent.KEYCODE_ENTER -> if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                node.performAction(AccessibilityNodeInfo.AccessibilityAction.ACTION_IME_ENTER.id)
            }
            KeyEvent.KEYCODE_DPAD_LEFT -> (s - 1).coerceAtLeast(0).let { setSelection(node, it, it) }
            KeyEvent.KEYCODE_DPAD_RIGHT -> (e + 1).coerceAtMost(cur.length).let { setSelection(node, it, it) }
            KeyEvent.KEYCODE_MOVE_HOME -> setSelection(node, 0, 0)
            KeyEvent.KEYCODE_MOVE_END -> setSelection(node, cur.length, cur.length)
        }
    }

    companion object {
        private const val TAG = "airhand"
        private const val SEGMENT_MS = 16L
        private const val SEGMENT_MAX_MS = 40L
        private const val WHEEL_MS = 60L

        /** 鼠标左键当作的手指（触摸点是 0..9） */
        private const val MOUSE = 100
        private const val MAX_FINGERS = 10

        /** 这台手机能执行的系统操作（告诉控制端，它只显示这些），JSON 里的名字（和 `SystemAction.wire` 一样） */
        fun supportedActions(): List<String> = buildList {
            addAll(listOf("back", "home", "recents", "notifications", "quickSettings", "powerMenu"))
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) addAll(listOf("screenshot", "lockScreen"))
            addAll(listOf("volumeUp", "volumeDown"))
        }

        /** 能不能多点触控（Android 8 起才有连续手势） */
        val touchSupported: Boolean get() = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O

        private val MODIFIERS = setOf("ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight")

        private val EDIT_KEYS = mapOf(
            "Backspace" to KeyEvent.KEYCODE_DEL,
            "Delete" to KeyEvent.KEYCODE_FORWARD_DEL,
            "Enter" to KeyEvent.KEYCODE_ENTER,
            "NumpadEnter" to KeyEvent.KEYCODE_ENTER,
            "Tab" to KeyEvent.KEYCODE_TAB,
            "ArrowLeft" to KeyEvent.KEYCODE_DPAD_LEFT,
            "ArrowRight" to KeyEvent.KEYCODE_DPAD_RIGHT,
            "ArrowUp" to KeyEvent.KEYCODE_DPAD_UP,
            "ArrowDown" to KeyEvent.KEYCODE_DPAD_DOWN,
            "Home" to KeyEvent.KEYCODE_MOVE_HOME,
            "End" to KeyEvent.KEYCODE_MOVE_END,
            "PageUp" to KeyEvent.KEYCODE_PAGE_UP,
            "PageDown" to KeyEvent.KEYCODE_PAGE_DOWN,
        )

        private val SYMBOLS = mapOf(
            "Space" to (' ' to ' '),
            "Backquote" to ('`' to '~'),
            "Minus" to ('-' to '_'),
            "Equal" to ('=' to '+'),
            "BracketLeft" to ('[' to '{'),
            "BracketRight" to (']' to '}'),
            "Backslash" to ('\\' to '|'),
            "Semicolon" to (';' to ':'),
            "Quote" to ('\'' to '"'),
            "Comma" to (',' to '<'),
            "Period" to ('.' to '>'),
            "Slash" to ('/' to '?'),
            "NumpadAdd" to ('+' to '+'),
            "NumpadSubtract" to ('-' to '-'),
            "NumpadMultiply" to ('*' to '*'),
            "NumpadDivide" to ('/' to '/'),
            "NumpadDecimal" to ('.' to '.'),
        )
    }
}
