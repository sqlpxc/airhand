package app.airhand.android.ui.session

import android.view.KeyEvent
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

/** 软键盘的文字、粘滞修饰键、外接键盘的按键变成什么输入事件 */
class KeyboardControllerTest {
    private val sent = ArrayList<String>()
    private var swap = false
    private val kb = KeyboardController({ swap }) { events -> events.forEach { sent += describe(it) } }

    private fun describe(e: Any): String {
        if (e !is JSONObject) return e.toString()
        return when (e.optString("type")) {
            "key" -> "${e.getString("code")}${if (e.getBoolean("down")) "↓" else "↑"}"
            "text" -> "text:${e.getString("text")}"
            else -> e.toString()
        }
    }

    @Test
    fun plainTextIsSentAsText() {
        kb.text("你好 world")
        assertEquals(listOf("text:你好 world"), sent)
    }

    @Test
    fun newlinesBecomeEnter() {
        kb.text("ls\n")
        assertEquals(listOf("text:ls", "Enter↓", "Enter↑"), sent)
    }

    @Test
    fun stickyCtrlAppliesToTheNextCharacterOnly() {
        kb.toggle(KeyboardController.Mod.CTRL)
        kb.text("c")
        kb.text("c")
        assertEquals(listOf("ControlLeft↓", "KeyC↓", "KeyC↑", "ControlLeft↑", "text:c"), sent)
        assertEquals(KeyboardController.Sticky.OFF, kb.state(KeyboardController.Mod.CTRL))
    }

    @Test
    fun lockedModifierStays() {
        kb.toggle(KeyboardController.Mod.ALT)
        kb.toggle(KeyboardController.Mod.ALT)
        assertEquals(KeyboardController.Sticky.LOCK, kb.state(KeyboardController.Mod.ALT))
        kb.press("Tab")
        kb.press("Tab")
        assertEquals(listOf("AltLeft↓", "Tab↓", "Tab↑", "AltLeft↑", "AltLeft↓", "Tab↓", "Tab↑", "AltLeft↑"), sent)
        kb.toggle(KeyboardController.Mod.ALT)
        assertEquals(KeyboardController.Sticky.OFF, kb.state(KeyboardController.Mod.ALT))
    }

    @Test
    fun shiftedSymbolsUseShift() {
        kb.toggle(KeyboardController.Mod.CTRL)
        kb.text("?")
        assertEquals(listOf("ControlLeft↓", "ShiftLeft↓", "Slash↓", "Slash↑", "ShiftLeft↑", "ControlLeft↑"), sent)
    }

    // ───────────── 外接键盘 ─────────────

    @Test
    fun physicalKeysAreSentAsPressed() {
        kb.keyDown("ShiftLeft", KeyEvent.META_SHIFT_ON)
        kb.keyDown("KeyA", KeyEvent.META_SHIFT_ON)
        kb.keyDown("KeyA", KeyEvent.META_SHIFT_ON)
        kb.keyUp("KeyA")
        kb.keyUp("ShiftLeft")
        assertEquals(listOf("ShiftLeft↓", "KeyA↓", "KeyA↓", "KeyA↑", "ShiftLeft↑"), sent)
    }

    /** Ctrl 的按下被输入法或者系统拿走了，只能从 C 的 metaState 看出来：和 C 一起按一下，C 的抬起不再发 */
    @Test
    fun modifierMissedByTheAppIsAddedFromMetaState() {
        kb.keyDown("KeyC", KeyEvent.META_CTRL_ON)
        kb.keyUp("KeyC")
        assertEquals(listOf("ControlLeft↓", "KeyC↓", "KeyC↑", "ControlLeft↑"), sent)
    }

    @Test
    fun rightModifierCountsAsHeld() {
        kb.keyDown("ControlRight", KeyEvent.META_CTRL_ON)
        kb.keyDown("KeyC", KeyEvent.META_CTRL_ON)
        assertEquals(listOf("ControlRight↓", "KeyC↓"), sent)
    }

    @Test
    fun swapTurnsCtrlIntoCommandAndBack() {
        swap = true
        kb.keyDown("ControlLeft", KeyEvent.META_CTRL_ON)
        kb.keyDown("KeyC", KeyEvent.META_CTRL_ON)
        kb.keyUp("KeyC")
        kb.keyUp("ControlLeft")
        kb.keyDown("MetaRight", KeyEvent.META_META_ON)
        kb.keyUp("MetaRight")
        assertEquals(listOf("MetaLeft↓", "KeyC↓", "KeyC↑", "MetaLeft↑", "ControlRight↓", "ControlRight↑"), sent)
    }

    @Test
    fun swapAppliesToModifierMissedByTheApp() {
        swap = true
        kb.keyDown("KeyC", KeyEvent.META_CTRL_ON)
        kb.keyUp("KeyC")
        assertEquals(listOf("MetaLeft↓", "KeyC↓", "KeyC↑", "MetaLeft↑"), sent)
    }

    /** 按着时改了开关：松开的还是按下时发的那个，对方那边不会卡着 ⌘ */
    @Test
    fun releaseMatchesWhatWasPressed() {
        swap = true
        kb.keyDown("ControlLeft", KeyEvent.META_CTRL_ON)
        swap = false
        kb.keyUp("ControlLeft")
        assertEquals(listOf("MetaLeft↓", "MetaLeft↑"), sent)
    }

    /** 快捷键栏上的修饰键写的是什么就发什么，不跟着互换 */
    @Test
    fun stickyKeysAreNotSwapped() {
        swap = true
        kb.toggle(KeyboardController.Mod.META)
        kb.keyDown("KeyC")
        kb.keyUp("KeyC")
        kb.toggle(KeyboardController.Mod.CTRL)
        kb.text("c")
        assertEquals(
            listOf("MetaLeft↓", "KeyC↓", "KeyC↑", "MetaLeft↑", "ControlLeft↓", "KeyC↓", "KeyC↑", "ControlLeft↑"),
            sent,
        )
    }

    @Test
    fun stickyModifierAlreadyHeldIsNotPressedTwice() {
        kb.keyDown("ControlLeft", KeyEvent.META_CTRL_ON)
        kb.toggle(KeyboardController.Mod.CTRL)
        kb.keyDown("KeyC", KeyEvent.META_CTRL_ON)
        kb.keyUp("KeyC")
        assertEquals(listOf("ControlLeft↓", "KeyC↓", "KeyC↑"), sent)
        assertEquals(KeyboardController.Sticky.OFF, kb.state(KeyboardController.Mod.CTRL))
    }

    /** 窗口失去焦点：按着的都松开（后按的先松），之后才到的抬起不再发 */
    @Test
    fun releaseKeysLetsGoOfEverything() {
        swap = true
        kb.keyDown("ControlLeft", KeyEvent.META_CTRL_ON)
        kb.keyDown("KeyW", KeyEvent.META_CTRL_ON)
        kb.releaseKeys()
        kb.keyUp("KeyW")
        kb.keyUp("ControlLeft")
        assertEquals(listOf("MetaLeft↓", "KeyW↓", "KeyW↑", "MetaLeft↑"), sent)
    }
}
