package app.airhand.android.host

/**
 * 帧率节流：按画面的时间戳（纳秒，`System.nanoTime` 的时钟）排一条时间线，每处理一帧往后推一个周期；
 * 可以提前 1/4 个周期，按时到的画面不用等。和桌面版的 `client/src-tauri/src/host/pace.rs` 是同一个做法。
 *
 * 不按"画完这一帧的时刻 + 1/帧率"排：屏幕本来就按这个帧率出画面时，每帧多出一点处理和睡过头的时间，
 * 下一帧要等的一帧比一帧久，等满一帧就跳掉一帧再从头来（平均多出半帧的延迟，还每隔一阵顿一下）。
 * 时间线落后了（画面静止了一阵）从这一帧重新排，不会连着补；画面比时间线早时，时间线每帧最多往前挪 1/64 个周期，
 * 跟得上比设定的帧率快一点点的屏幕。
 */
internal class Pacer(fps: Int) {
    private var period = periodOf(fps)

    /** 时间线上下一帧的位置；还没处理过画面时为 null */
    private var next: Long? = null

    fun setFps(fps: Int) {
        period = periodOf(fps)
    }

    /** 现在（[now]）还要等多少纳秒才能处理下一帧 */
    fun waitNanos(now: Long): Long = next?.let { maxOf(0L, it - period / 4 - now) } ?: 0L

    /** 处理了时间戳为 [stamp] 的一帧 */
    fun done(stamp: Long) {
        val base = next?.let { maxOf(stamp, it - period / 64) } ?: stamp
        next = base + period
    }

    private fun periodOf(fps: Int) = 1_000_000_000L / fps.coerceAtLeast(1)
}
