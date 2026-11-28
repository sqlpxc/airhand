package app.airhand.android.wheelpatch

import org.junit.Assert.*
import org.junit.Test

class TabletWheelDecoderTest {
    private fun sample(action: Int, x: Float = 100f, y: Float = 100f, time: Long = 100L) =
        TabletWheelDecoder.Sample(action, 7, 100L, time, x, y)

    @Test fun syntheticWheelDoesNotBecomePointerOrClick() {
        val d = TabletWheelDecoder()
        assertEquals(TabletWheelDecoder.Result(true), d.accept(sample(0), true, 48f))
        assertEquals(TabletWheelDecoder.Result(true, 0f, 1f), d.accept(sample(2, y = 148f, time = 110), true, 48f))
        assertEquals(TabletWheelDecoder.Result(true, 0f, -0.5f), d.accept(sample(2, y = 124f, time = 120), true, 48f))
        assertEquals(TabletWheelDecoder.Result(true), d.accept(sample(1, y = 124f, time = 130), true, 48f))
    }

    @Test fun standardMouseFingerStylusAndButtonsPassThrough() {
        for (s in listOf(sample(0).copy(unknownTool = false), sample(0).copy(touchscreen = false),
            sample(0).copy(pointers = 2), sample(0).copy(buttons = 1))) {
            assertFalse(TabletWheelDecoder().accept(s, true, 48f).consumed)
        }
    }

    @Test fun disablingResetsSequence() {
        val d = TabletWheelDecoder()
        d.accept(sample(0), true, 48f)
        assertFalse(d.accept(sample(2), false, 48f).consumed)
        assertFalse(d.accept(sample(2), true, 48f).consumed)
    }

    @Test fun differentDeviceOrGestureCannotScroll() {
        val d = TabletWheelDecoder()
        d.accept(sample(0), true, 48f)
        assertFalse(d.accept(sample(2).copy(device = 8), true, 48f).consumed)
        assertFalse(d.accept(sample(2).copy(downTime = 101), true, 48f).consumed)
        assertEquals(0f, d.accept(sample(2, y = 148f, time = 2000), true, 48f).vertical)
    }

    @Test fun cancelAndFocusResetPreventDanglingInput() {
        val d = TabletWheelDecoder()
        d.accept(sample(0), true, 48f)
        assertTrue(d.accept(sample(3), true, 48f).consumed)
        assertFalse(d.accept(sample(2), true, 48f).consumed)
        d.accept(sample(0), true, 48f)
        d.reset()
        assertFalse(d.accept(sample(1), true, 48f).consumed)
    }

    @Test fun tinyStepsAndHorizontalDirectionArePreserved() {
        val d = TabletWheelDecoder()
        d.accept(sample(0), true, 48f)
        val r = d.accept(sample(2, x = 99f, y = 101f, time = 110), true, 48f)
        assertEquals(1f / 48, r.horizontal, 0.00001f)
        assertEquals(1f / 48, r.vertical, 0.00001f)
    }

    @Test fun jumpsAndInvalidValuesDoNotGenerateScroll() {
        val d = TabletWheelDecoder()
        d.accept(sample(0), true, 48f)
        assertEquals(0f, d.accept(sample(2, y = 10000f), true, 48f).vertical)
        assertEquals(0f, d.accept(sample(2, y = Float.NaN), true, 48f).vertical)
        assertFalse(TabletWheelDecoder().accept(sample(0, y = Float.NaN), true, 48f).consumed)
    }
}
