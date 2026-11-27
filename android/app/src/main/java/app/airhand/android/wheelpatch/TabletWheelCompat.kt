package app.airhand.android.wheelpatch

import android.content.Context
import android.content.SharedPreferences
import android.os.Build
import android.util.Log
import android.view.InputDevice
import android.view.MotionEvent

/** Adapter kept outside upstream's input implementation. Emits through upstream's wheel path. */
class TabletWheelCompat(context: Context, private val scroll: (MotionEvent) -> Unit) {
    private val prefs = preferences(context)
    private val density = context.resources.displayMetrics.density
    private val decoder = TabletWheelDecoder()
    private var diagnosticEvents = 0

    fun reset() { decoder.reset() }

    fun onTouchEvent(event: MotionEvent): Boolean {
        if (prefs.getBoolean("diagnostics", false) && diagnosticEvents++ < 120) {
            Log.i("AirhandWheelPatch", "action=${event.actionMasked} source=${event.source} " +
                "tool=${event.getToolType(0)} pointers=${event.pointerCount} buttons=${event.buttonState} " +
                "device=${event.deviceId} x=${event.x} y=${event.y} " +
                "v=${event.getAxisValue(MotionEvent.AXIS_VSCROLL)} h=${event.getAxisValue(MotionEvent.AXIS_HSCROLL)}")
        }
        val result = decoder.accept(
            TabletWheelDecoder.Sample(
                event.actionMasked, event.deviceId, event.downTime, event.eventTime, event.x, event.y,
                event.pointerCount, event.getToolType(0) == MotionEvent.TOOL_TYPE_UNKNOWN,
                event.buttonState, event.isFromSource(InputDevice.SOURCE_TOUCHSCREEN),
            ),
            prefs.getBoolean("enabled", defaultEnabled()),
            prefs.getInt("notchDp", 48).coerceIn(12, 120) * density,
        )
        if (result.horizontal != 0f || result.vertical != 0f) {
            val pointer = MotionEvent.PointerProperties().apply {
                id = 0
                toolType = MotionEvent.TOOL_TYPE_MOUSE
            }
            val axes = MotionEvent.PointerCoords().apply {
                setAxisValue(MotionEvent.AXIS_HSCROLL, result.horizontal)
                setAxisValue(MotionEvent.AXIS_VSCROLL, result.vertical)
            }
            val wheelEvent = MotionEvent.obtain(
                event.downTime, event.eventTime, MotionEvent.ACTION_SCROLL, 1,
                arrayOf(pointer), arrayOf(axes), event.metaState, 0, 1f, 1f,
                event.deviceId, 0, InputDevice.SOURCE_MOUSE, 0,
            )
            try { scroll(wheelEvent) } finally { wheelEvent.recycle() }
        }
        return result.consumed
    }

    companion object {
        fun preferences(context: Context): SharedPreferences =
            context.getSharedPreferences("airhand-wheel-patch", Context.MODE_PRIVATE)

        fun defaultEnabled(): Boolean = Build.MANUFACTURER.equals("huawei", ignoreCase = true) ||
            Build.MANUFACTURER.equals("honor", ignoreCase = true)
    }
}
