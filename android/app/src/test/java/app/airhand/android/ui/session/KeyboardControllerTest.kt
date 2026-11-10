package app.airhand.android.ui.session

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test

/** 软键盘的文字、粘滞修饰键变成什么输入事件 */
class KeyboardControllerTest {
    private val sent = ArrayList<String>()
    private val kb = KeyboardController { events -> events.forEach { sent += describe(it) } }

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
}
