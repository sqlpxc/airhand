package app.airhand.android.wheelpatch

/** No Android dependency: the vendor-event classifier can be tested without a tablet. */
internal class TabletWheelDecoder {
    data class Sample(
        val action: Int,
        val device: Int,
        val downTime: Long,
        val time: Long,
        val x: Float,
        val y: Float,
        val pointers: Int = 1,
        val unknownTool: Boolean = true,
        val buttons: Int = 0,
        val touchscreen: Boolean = true,
    )

    data class Result(val consumed: Boolean, val horizontal: Float = 0f, val vertical: Float = 0f)

    private var active: Sample? = null

    fun reset() { active = null }

    fun accept(sample: Sample, enabled: Boolean, pixelsPerNotch: Float): Result {
        if (!enabled) {
            reset()
            return Result(false)
        }
        // A real finger/stylus, mouse button or multi-touch must keep its original meaning.
        val candidate = sample.unknownTool && sample.touchscreen && sample.pointers == 1 && sample.buttons == 0
        val previous = active
        if (!candidate) {
            reset()
            // Finish an intercepted vendor sequence without exposing a dangling UP to gestures.
            return Result(previous != null && previous.device == sample.device && previous.downTime == sample.downTime)
        }
        if (sample.action == DOWN) {
            active = sample.takeIf { it.x.isFinite() && it.y.isFinite() }
            return Result(active != null)
        }
        if (previous == null || sample.device != previous.device || sample.downTime != previous.downTime) {
            return Result(false)
        }
        if (sample.action == CANCEL || sample.action == UP) {
            reset()
            return Result(true)
        }
        if (sample.action != MOVE) return Result(true)
        active = sample
        val elapsed = sample.time - previous.time
        if (elapsed !in 0..1000 || !sample.x.isFinite() || !sample.y.isFinite() ||
            !pixelsPerNotch.isFinite() || pixelsPerNotch <= 0f) return Result(true)
        val dx = sample.x - previous.x
        val dy = sample.y - previous.y
        if (!dx.isFinite() || !dy.isFinite()) return Result(true)
        // Do not turn a coordinate reset into a huge scroll. Keep small increments fractional.
        if (kotlin.math.abs(dx) > pixelsPerNotch * 8 || kotlin.math.abs(dy) > pixelsPerNotch * 8) return Result(true)
        // Android synthetic swipes move with the content; protocol +Y is up, +X is right.
        return Result(true, if (dx == 0f) 0f else -dx / pixelsPerNotch, if (dy == 0f) 0f else dy / pixelsPerNotch)
    }

    companion object {
        const val DOWN = 0
        const val UP = 1
        const val MOVE = 2
        const val CANCEL = 3
    }
}
