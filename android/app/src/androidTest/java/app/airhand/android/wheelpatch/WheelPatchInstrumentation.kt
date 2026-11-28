package app.airhand.android.wheelpatch

import android.app.Activity
import android.app.Instrumentation
import android.os.Bundle
import android.os.SystemClock
import android.view.InputDevice
import android.view.MotionEvent
import android.view.Surface
import app.airhand.android.data.GameLayout
import app.airhand.android.ui.session.KeyboardController
import app.airhand.android.ui.session.RemoteView
import org.json.JSONObject

/** Android MotionEvent -> RemoteView -> protocol JSON, without a live remote connection. */
class WheelPatchInstrumentation : Instrumentation() {
    override fun onCreate(arguments: Bundle?) { super.onCreate(arguments); start() }

    override fun onStart() {
        val report = Bundle()
        try {
            runOnMainSync { verifyRouting() }
            report.putString("stream", "PASS: vendor/native wheel, reversal, mouse buttons, finger, disabled, editor and view-only routing\n")
            finish(Activity.RESULT_OK, report)
        } catch (error: Throwable) {
            report.putString("stream", "FAIL: ${error.stackTraceToString()}\n")
            finish(Activity.RESULT_CANCELED, report)
        }
    }

    private fun verifyRouting() {
        val output = ArrayList<JSONObject>()
        val prefs = TabletWheelCompat.preferences(targetContext)
        val oldPrefs = prefs.all
        try {
            prefs.edit().putBoolean("enabled", true).putInt("notchDp", 48).commit()
            val host = object : RemoteView.Host {
                override val keyboard = KeyboardController { }
                override fun onSurface(surface: Surface?) { }
                override fun onZoom(zoomed: Boolean, shownW: Float, shownH: Float) { }
                override fun onInput(events: List<Any>, now: Boolean) { output += events.filterIsInstance<JSONObject>() }
                override fun onToggleKeyboard() { }
            }
            val view = RemoteView(targetContext, host)
            view.layout(0, 0, 1000, 1000)
            view.setVideoSize(1000, 1000)
            fun configureWheel(reverse: Boolean) {
                view.setWheel(reverse, 100, app.airhand.android.data.WheelMode.LINEAR, false)
            }
            configureWheel(false)
            val span = 48 * targetContext.resources.displayMetrics.density
            var time = SystemClock.uptimeMillis()
            var down = time
            fun dispatch(action: Int, y: Float, tool: Int = MotionEvent.TOOL_TYPE_UNKNOWN,
                         source: Int = InputDevice.SOURCE_TOUCHSCREEN, wheel: Float = 0f,
                         generic: Boolean = false, buttons: Int = 0) {
                time += 10
                if (action == MotionEvent.ACTION_DOWN) down = time
                val pointer = MotionEvent.PointerProperties().apply { id = 0; toolType = tool }
                val coords = MotionEvent.PointerCoords().apply {
                    x = 100f; this.y = y; pressure = 1f
                    setAxisValue(MotionEvent.AXIS_VSCROLL, wheel)
                }
                val e = MotionEvent.obtain(down, time, action, 1, arrayOf(pointer), arrayOf(coords),
                    0, buttons, 1f, 1f, 7, 0, source, 0)
                try { if (generic) view.onGenericMotionEvent(e) else view.onTouchEvent(e) }
                finally { e.recycle() }
            }
            fun synthetic() {
                dispatch(MotionEvent.ACTION_DOWN, 100f)
                dispatch(MotionEvent.ACTION_MOVE, 100f + span)
                dispatch(MotionEvent.ACTION_UP, 100f + span)
            }
            synthetic()
            check(output.size == 1 && output[0].getString("type") == "wheel" && output[0].getInt("dy") == 120) { "Vendor wheel became pointer/click: $output" }
            output.clear()
            dispatch(MotionEvent.ACTION_SCROLL, 100f, MotionEvent.TOOL_TYPE_MOUSE, InputDevice.SOURCE_MOUSE, -1f, true)
            check(output.size == 1 && output[0].getInt("dy") == -120) { "Native wheel failed: $output" }
            output.clear()
            configureWheel(true)
            synthetic()
            check(output.size == 1 && output[0].getInt("dy") == -120) { "Reverse setting ignored: $output" }
            output.clear()
            configureWheel(false)
            dispatch(MotionEvent.ACTION_DOWN, 100f, MotionEvent.TOOL_TYPE_MOUSE, InputDevice.SOURCE_MOUSE, buttons = MotionEvent.BUTTON_PRIMARY)
            dispatch(MotionEvent.ACTION_UP, 100f, MotionEvent.TOOL_TYPE_MOUSE, InputDevice.SOURCE_MOUSE)
            check(output.count { it.optString("type") == "mouseButton" } == 2) { "Mouse buttons failed: $output" }
            check(output.none { it.optString("type") == "wheel" })
            output.clear()
            dispatch(MotionEvent.ACTION_DOWN, 100f, MotionEvent.TOOL_TYPE_FINGER)
            dispatch(MotionEvent.ACTION_MOVE, 150f, MotionEvent.TOOL_TYPE_FINGER)
            dispatch(MotionEvent.ACTION_CANCEL, 150f, MotionEvent.TOOL_TYPE_FINGER)
            check(output.none { it.optString("type") == "wheel" }) { "Finger became wheel: $output" }
            output.clear()
            prefs.edit().putBoolean("enabled", false).commit()
            synthetic()
            check(output.none { it.optString("type") == "wheel" }) { "Disabled compatibility still scrolls: $output" }
            dispatch(MotionEvent.ACTION_CANCEL, 100f)
            prefs.edit().putBoolean("enabled", true).commit()
            view.editor = view.newEditor(GameLayout("wheel-test", "wheel-test"))
            output.clear()
            synthetic()
            check(output.isEmpty()) { "Editor input leaked: $output" }
            view.editor = null
            view.interactive = false
            output.clear()
            synthetic()
            check(output.isEmpty()) { "View-only input leaked: $output" }
        } finally {
            val restore = prefs.edit().clear()
            for ((key, value) in oldPrefs) {
                when (value) {
                    is Boolean -> restore.putBoolean(key, value)
                    is Int -> restore.putInt(key, value)
                }
            }
            restore.commit()
        }
    }
}
