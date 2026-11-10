package app.airhand.android.host

import org.junit.Assert.*
import org.junit.Test

class PacerTest {
    private val second = 1_000_000_000L

    /**
     * 渲染线程：等到时间线允许，再取屏幕的下一帧（等待期间出过画面就拿最新的一帧）。屏幕每 [source] 纳秒出一帧，
     * 处理一帧要 3 ms。返回每一帧 (时间戳, 取到它的时刻)
     */
    private fun render(source: Long, fps: Int = 60): List<Pair<Long, Long>> {
        val p = Pacer(fps)
        var now = second
        val got = ArrayList<Pair<Long, Long>>()
        while (now < 11 * second) {
            now += p.waitNanos(now)
            val latest = now / source * source
            val stamp = if (got.isEmpty() || latest > got.last().first) latest else latest + source
            now = maxOf(now, stamp)
            got.add(stamp to now)
            p.done(stamp)
            now += 3_000_000
        }
        return got
    }

    @Test
    fun aScreenAtTheFrameRateIsNeverDelayedOrSkipped() {
        for (hz in listOf(60.0, 59.94, 60.001)) {
            val source = Math.round(1e9 / hz)
            // 第一帧是开始之前就有的旧画面，处理它的时候下一帧到了
            val got = render(source).drop(1)
            assertTrue("$hz Hz: ${got.size}", got.size >= 10 * hz - 2)
            for ((a, b) in got.zipWithNext()) {
                assertEquals("$hz Hz: skipped at $a", source, b.first - a.first)
                assertEquals("$hz Hz: delayed at $b", b.first, b.second)
            }
        }
    }

    @Test
    fun aFasterScreenIsThinnedToTheFrameRate() {
        for (hz in listOf(90.0, 120.0, 144.0)) {
            val source = Math.round(1e9 / hz)
            val got = render(source)
            val fps = got.size / 10.0
            assertTrue("$hz Hz: $fps fps", fps in 58.0..62.0)
            for ((stamp, at) in got) assertTrue("$hz Hz: $stamp at $at", at - stamp < source)
        }
    }

    @Test
    fun afterAPauseTheTimelineRestarts() {
        val period = second / 30
        val p = Pacer(30)
        assertEquals(0L, p.waitNanos(0))
        p.done(0)
        // 可以提前 1/4 个周期
        assertEquals(period - period / 4, p.waitNanos(0))
        // 静止了一阵：新画面马上处理，之后照常按帧率，不连着补
        assertEquals(0L, p.waitNanos(5 * second))
        p.done(5 * second)
        assertEquals(period - period / 4, p.waitNanos(5 * second))
    }
}
