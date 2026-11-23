package app.airhand.android.ui.session

import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import app.airhand.android.controller.Input
import app.airhand.android.controller.KeyMap

/**
 * 键盘：软键盘打的字、快捷键栏的特殊键、外接键盘，都从这里发给远端。
 *
 * 修饰键（Ctrl / Alt / Shift / Win）在快捷键栏上是"粘滞"的：点一下只对下一个键有效（[Sticky.ONCE]），
 * 再点一下锁定（[Sticky.LOCK]），再点取消。比如点 Ctrl 再在软键盘上打 c，发出去的就是 Ctrl+C。
 *
 * 外接键盘（和输入法发来的按键事件）按物理键发（[key]）：
 * * 修饰键的按下可能被输入法或者系统拿走了，只能从后面那个键的 `metaState` 看出按着：这时把它和那个键一起按一下；
 * * 控制 Mac、这台电脑打开了互换 Ctrl 和 ⌘ 时（[swapCtrlCommand]），键盘上的 Ctrl 发成 ⌘，⌘（Win 键）发成 Ctrl。
 *   快捷键栏和快捷键菜单不换，按钮上写的是什么就发什么（和桌面控制台一样）；
 * * 按着的键都记着（[held]）：松开时发按下时发的那个键（中途改了开关也不会卡住），窗口失去焦点时一起松开（[releaseKeys]）。
 */
class KeyboardController(
    /** 外接键盘的 Ctrl 和 ⌘ 互换（控制 Mac、这台电脑打开了这个开关时） */
    private val swapCtrlCommand: () -> Boolean = { false },
    private val send: (List<Any>) -> Unit,
) {
    /** `flag`：按键事件的 `metaState` 里表示这个修饰键按着的位 */
    enum class Mod(val code: String, val flag: Int) {
        CTRL("ControlLeft", KeyEvent.META_CTRL_ON),
        ALT("AltLeft", KeyEvent.META_ALT_ON),
        SHIFT("ShiftLeft", KeyEvent.META_SHIFT_ON),
        META("MetaLeft", KeyEvent.META_META_ON),
    }

    enum class Sticky { OFF, ONCE, LOCK }

    val mods = mutableStateMapOf<Mod, Sticky>()

    /** 外接键盘按着的键 → 按下时发出去的键（Ctrl / ⌘ 互换时不一样），按按下的顺序 */
    private val held = LinkedHashMap<String, String>()

    /** 按下时和补上的修饰键一起按完了的键（见 [keyDown]）：它的抬起不用再发 */
    private val tapped = HashSet<String>()

    /** 输入法正在组字（例如拼音）的内容，还没有发出去，界面上显示出来 */
    var composing by mutableStateOf("")

    fun state(m: Mod): Sticky = mods[m] ?: Sticky.OFF

    fun toggle(m: Mod) {
        mods[m] = when (state(m)) {
            Sticky.OFF -> Sticky.ONCE
            Sticky.ONCE -> Sticky.LOCK
            Sticky.LOCK -> Sticky.OFF
        }
    }

    private fun active(): List<Mod> = Mod.entries.filter { state(it) != Sticky.OFF }

    private fun consumeOnce() {
        Mod.entries.forEach { if (state(it) == Sticky.ONCE) mods[it] = Sticky.OFF }
    }

    /** 按一下某个键（带上当前的修饰键） */
    fun press(code: String, extraShift: Boolean = false) {
        val codes = active().map { it.code }.toMutableList()
        if (extraShift && Mod.SHIFT.code !in codes) codes += Mod.SHIFT.code
        send(Input.chord(codes + code))
        consumeOnce()
    }

    /** 同时按下一组键（快捷键菜单里的组合键） */
    fun chord(codes: List<String>) = send(Input.chord(codes))

    /** 输入法提交的文字 */
    fun text(s: String) {
        if (s.isEmpty()) return
        val withMods = active().any { it != Mod.SHIFT }
        if (withMods || (state(Mod.SHIFT) != Sticky.OFF && s.length == 1)) {
            // 有修饰键：按键的方式发（Ctrl+C 之类）
            for (c in s) {
                val k = KeyMap.forChar(c)
                if (k != null) press(k.first, k.second) else send(listOf(Input.text(c.toString())))
            }
            return
        }
        // 普通文字：换行变成回车键，其余整段发过去
        val parts = s.split('\n')
        parts.forEachIndexed { i, part ->
            if (part.isNotEmpty()) send(listOf(Input.text(part)))
            if (i < parts.size - 1) press("Enter")
        }
    }

    /** 外接键盘 / 输入法发来的按键事件。返回是否处理了 */
    fun key(e: KeyEvent): Boolean {
        val code = KeyMap.code(e.keyCode) ?: return false
        when (e.action) {
            KeyEvent.ACTION_DOWN -> keyDown(code, e.metaState)
            KeyEvent.ACTION_UP -> keyUp(code)
            else -> return false
        }
        return true
    }

    /** 外接键盘按下一个键。`metaState`：按键事件说这时按着哪些修饰键 */
    internal fun keyDown(code: String, metaState: Int = 0) {
        if (code in MODIFIER_CODES) {
            // 按住时的自动重复照样发，用第一次按下时的映射
            send(listOf(Input.key(held.getOrPut(code) { mapped(code) }, true)))
            return
        }
        // 要和这个键一起按的修饰键：快捷键栏上点了的（写的是什么就发什么），按键事件说按着、却没见到它按下的。
        // 对方那边已经按着的（不分左右）不用再按
        val physical = heldModifiers(held.keys)
        val extra = LinkedHashSet<String>()
        for (m in Mod.entries) {
            if (state(m) != Sticky.OFF) extra += m.code
            if (metaState and m.flag != 0 && side(m.code) !in physical) extra += mapped(m.code)
        }
        val remote = heldModifiers(held.values)
        extra.removeAll { side(it) in remote }
        if (extra.isEmpty()) {
            held[code] = code
            tapped -= code
            send(listOf(Input.key(code, true)))
        } else {
            // 一下按完：修饰键按下，这个键按下、松开，修饰键松开。它自己的抬起之后不再发
            held -= code
            tapped += code
            send(Input.chord(extra.toList() + code))
        }
        consumeOnce()
    }

    /** 外接键盘松开一个键：发按下时发的那个。没见到按下的（那时窗口没有焦点、或者已经一起松开了）不发 */
    internal fun keyUp(code: String) {
        if (tapped.remove(code)) return
        val sent = held.remove(code) ?: return
        send(listOf(Input.key(sent, false)))
    }

    /** 松开外接键盘按着的所有键，后按下的先松开。窗口失去焦点时：之后的抬起不一定送得到这里，对方那边会一直按着 */
    fun releaseKeys() {
        tapped.clear()
        if (held.isEmpty()) return
        val ups = held.values.toList().asReversed().map { Input.key(it, false) }
        held.clear()
        send(ups)
    }

    /** 外接键盘上的这个键发给对方时是哪个键：只有 Ctrl 和 ⌘ 可能互换 */
    private fun mapped(code: String): String = if (swapCtrlCommand()) CTRL_COMMAND_SWAP[code] ?: code else code

    /** 不分左右的修饰键（ControlLeft、ControlRight → Control） */
    private fun side(code: String): String = code.removeSuffix("Left").removeSuffix("Right")

    private fun heldModifiers(codes: Collection<String>): Set<String> = codes.filter { it in MODIFIER_CODES }.mapTo(HashSet(), ::side)

    companion object {
        private val MODIFIER_CODES = setOf(
            "ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight",
        )
        private val CTRL_COMMAND_SWAP = mapOf(
            "ControlLeft" to "MetaLeft", "MetaLeft" to "ControlLeft",
            "ControlRight" to "MetaRight", "MetaRight" to "ControlRight",
        )
    }
}

/**
 * 外接键盘的哪些按键在输入法之前发（见 RemoteView.onKeyPreIme）。
 *
 * 按下：Ctrl / ⌘ 本身和带着它们的组合键，不然输入法会自己处理掉（Ctrl+C 之类，发不到对方）；别的键照样先给输入法（打拼音）。
 * 抬起跟着按下走：按下是在输入法之前发的，抬起也在之前发（Ctrl+C 先松开 Ctrl 时，C 的抬起已经不带 Ctrl 了）；
 * 按下给了输入法的，抬起也给它。输入法只看到按下、看不到抬起会乱，例如单按 Shift 切换中英文是按下时放过去、抬起时才切换
 */
internal class ImeBypass {
    /** 按下在输入法之前发了、还没抬起的键（Android 的按键码） */
    private val keys = HashSet<Int>()

    /** 这个按键要不要在输入法之前发。`shortcut`：按着 Ctrl / ⌘，或者它就是 Ctrl / ⌘ */
    fun early(keyCode: Int, down: Boolean, shortcut: Boolean): Boolean = if (down) shortcut else keys.remove(keyCode)

    /** 在输入法之前发了一个按下。按住时的自动重复不算：第一次按下给了输入法的，抬起也给它 */
    fun sent(keyCode: Int, repeatCount: Int) {
        if (repeatCount == 0) keys += keyCode
    }

    /** 窗口失去焦点：按着的键都已经松开了（KeyboardController.releaseKeys） */
    fun clear() = keys.clear()
}

/**
 * 软键盘的输入连接：本地的编辑框始终是空的，提交的文字立刻发给远端。
 * 拼音这类组字过程留在输入法里（同时显示在界面上），提交时才发；删除键变成退格键。
 */
class RemoteInputConnection(view: View, private val kb: KeyboardController) : BaseInputConnection(view, true) {
    private fun clear() {
        editable?.let {
            removeComposingSpans(it)
            it.clear()
        }
        kb.composing = ""
    }

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val s = text?.toString() ?: ""
        clear()
        kb.text(s)
        return true
    }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
        super.setComposingText(text, newCursorPosition)
        kb.composing = text?.toString() ?: ""
        return true
    }

    override fun finishComposingText(): Boolean {
        val pending = editable?.toString() ?: ""
        clear()
        super.finishComposingText()
        if (pending.isNotEmpty()) kb.text(pending)
        return true
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        if (kb.composing.isNotEmpty()) return super.deleteSurroundingText(beforeLength, afterLength)
        repeat(beforeLength.coerceAtMost(64)) { kb.press("Backspace") }
        repeat(afterLength.coerceAtMost(64)) { kb.press("Delete") }
        return true
    }

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        if (event.keyCode == KeyEvent.KEYCODE_DEL || event.keyCode == KeyEvent.KEYCODE_ENTER || event.keyCode == KeyEvent.KEYCODE_FORWARD_DEL) {
            if (event.action == KeyEvent.ACTION_DOWN) kb.press(if (event.keyCode == KeyEvent.KEYCODE_DEL) "Backspace" else if (event.keyCode == KeyEvent.KEYCODE_ENTER) "Enter" else "Delete")
            return true
        }
        return kb.key(event) || super.sendKeyEvent(event)
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        kb.press("Enter")
        return true
    }

    companion object {
        fun configure(outAttrs: EditorInfo) {
            outAttrs.inputType = android.text.InputType.TYPE_CLASS_TEXT or android.text.InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
            outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_ACTION_NONE
            outAttrs.initialSelStart = 0
            outAttrs.initialSelEnd = 0
        }
    }
}
