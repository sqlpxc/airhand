package app.airhand.android.controller

import org.junit.Assert.*
import org.junit.Test

class AudioBufferTest {
    private fun packet(value: Int = 12_000) = ShortArray(960 * 2) { if (it % 2 == 0) value.toShort() else (-value).toShort() }
    private val block = ShortArray(480 * 2)

    @Test
    fun buffersRealAudioBeforePlayingAndPreservesStereo() {
        val buffer = AudioBuffer()
        buffer.push(packet(), 1920, true, 1_000_000)
        assertNull(buffer.pull(block))
        assertTrue(block.all { it == 0.toShort() })
        buffer.push(packet(), 1920, false, 1_020_000)
        assertNull(buffer.pull(block))
        buffer.push(packet(), 1920, false, 1_040_000)
        assertEquals(1_000_000L, buffer.pull(block)!!.hostUs)
        assertEquals(0, block[0].toInt()) // 渐入
        assertEquals(12_000, block[400].toInt())
        assertEquals(-12_000, block[401].toInt())
    }

    @Test
    fun shortNotificationStillPlaysAndDrainsCompletely() {
        val buffer = AudioBuffer()
        buffer.push(packet(), 1920, true, 50_000)
        assertNull(buffer.pull(block))
        assertEquals(50_000L, buffer.pull(block, finishBurst = true)!!.hostUs)
        assertNotNull(buffer.pull(block, finishBurst = true))
        assertEquals(0.0, buffer.bufferedMs, 0.001)
        assertNull(buffer.pull(block, finishBurst = true))
    }

    @Test
    fun networkStarvationRaisesTargetButSilenceDoesNot() {
        fun afterGap(start: Boolean): AudioBuffer {
            val buffer = AudioBuffer()
            repeat(3) { buffer.push(packet(), 1920, it == 0, it * 20_000L) }
            repeat(10) { buffer.pull(block) }
            buffer.push(packet(), 1920, start, 1_000_000)
            return buffer
        }
        assertEquals(80.0, afterGap(false).targetMs, 0.001)
        assertEquals(1, afterGap(false).underruns)
        assertEquals(60.0, afterGap(true).targetMs, 0.001)
        assertEquals(0, afterGap(true).underruns)
    }

    @Test
    fun burstDropsOldAudioAndClockFollowsRetainedSamples() {
        val buffer = AudioBuffer()
        repeat(25) { buffer.push(packet(), 1920, it == 0, 2_000_000L + it * 20_000) }
        val clock = buffer.pull(block)!!
        assertEquals(1, buffer.skips)
        assertEquals(2_440_000L, clock.hostUs)
        assertTrue(buffer.bufferedMs <= 60)
    }

    @Test
    fun ringWrapRetainsLatestClockAndNeverExceedsCapacity() {
        val buffer = AudioBuffer()
        repeat(200) { buffer.push(packet(), 1920, it == 0, 3_000_000L + it * 20_000) }
        assertTrue(buffer.bufferedMs < 2000)
        assertEquals(6_940_000L, buffer.pull(block)!!.hostUs)
        assertEquals(1, buffer.skips)
    }

    @Test
    fun extraDelayRebuffersAndReportsOnlyAppliedDelay() {
        val buffer = AudioBuffer()
        repeat(3) { buffer.push(packet(), 1920, it == 0, it * 20_000L) }
        buffer.pull(block)
        buffer.setExtraDelay(100)
        assertNull(buffer.pull(block)) // 淡出后补齐
        assertTrue(block[0] > block[block.size - 2])
        assertNull(buffer.pull(block))
        repeat(6) { buffer.push(packet(), 1920, false, 60_000L + it * 20_000) }
        assertEquals(100.0, buffer.pull(block)!!.appliedMs, 0.01)
        buffer.setExtraDelay(0)
        assertEquals(0.0, buffer.pull(block)!!.appliedMs, 0.01)
    }

    @Test
    fun steadyNetworkAndClockDriftStayContinuousForOneMinute() {
        for (intervalMs in listOf(20.0, 19.8, 20.1)) {
            val buffer = AudioBuffer()
            var nextPacket = 0.0
            var seq = 0L
            for (ms in 0 until 60_000 step 10) {
                while (nextPacket <= ms) {
                    buffer.push(packet(), 1920, seq == 0L, seq++ * 20_000)
                    nextPacket += intervalMs
                }
                val clock = buffer.pull(block)
                if (ms >= 100) assertNotNull("clock at $ms ms, packet interval $intervalMs", clock)
                assertTrue(buffer.bufferedMs < 200)
            }
            assertEquals("underruns at $intervalMs", 0, buffer.underruns)
            assertEquals("skips at $intervalMs", 0, buffer.skips)
            assertTrue("target at $intervalMs: ${buffer.targetMs}", buffer.targetMs in 40.0..45.0)
        }
    }

    /**
     * 按 AudioPlayer 的循环仿真：每 10 ms 先放进已经到达的包（带网络到达时刻），再取 10 ms；100 ms 没有新包就把剩下的播完。
     * 包基础延迟 5 ms，`late(发送时刻 ms)` 是额外晚到的毫秒数；按顺序到达，前面的晚了后面的跟着排队。
     * `drift`：被控端按自己的时钟每 20 ms 一包，但在这边看间隔是 20 / (1 + drift) ms
     */
    private fun simulate(seconds: Int, drift: Double = 0.0, late: (Double) -> Double): AudioBuffer {
        val buffer = AudioBuffer()
        val period = 20.0 / (1 + drift)
        var last = 0.0
        val arrivals = (0 until (seconds * 1000 / period).toInt()).map { i ->
            val sent = i * period
            last = maxOf(last, sent + 5 + late(sent))
            last
        }
        var next = 0
        var lastPush = 0.0
        for (ms in 0 until seconds * 1000 step 10) {
            while (next < arrivals.size && arrivals[next] <= ms) {
                buffer.push(packet(), 1920, next == 0, 7_000_000_000L + next * 20_000L, arrivals[next])
                next++
                lastPush = ms.toDouble()
            }
            buffer.pull(block, finishBurst = ms - lastPush > 100)
        }
        return buffer
    }

    /** `times`（秒）开始各卡 `ms` 毫秒：期间发出的包都等到卡完才到 */
    private fun stalls(times: List<Int>, ms: Int): (Double) -> Double = { sent ->
        times.firstOrNull { sent >= it * 1000 && sent < it * 1000 + ms }?.let { it * 1000.0 + ms - sent } ?: 0.0
    }

    @Test
    fun occasionalStallsOnlyStarveTheFirstTime() {
        // 每隔 15~45 秒卡 100 ms：只看断粮学的话，两次之间目标延迟已经回落，每次都会断
        val buffer = simulate(300, late = stalls(listOf(5, 37, 67, 109, 144, 181, 216, 246, 265, 281), 100))
        assertTrue("underruns ${buffer.underruns}", buffer.underruns <= 1)
        assertEquals(0, buffer.skips)
        assertTrue("target ${buffer.targetMs}", buffer.targetMs in 120.0..150.0)
    }

    @Test
    fun latenessRaisesTargetBeforeStarving() {
        // 先稳定到目标延迟回落到 40 ms，然后每 3 秒有一包晚到，每 10 秒多晚 5 ms：20、25……、45 ms
        val buffer = simulate(120) { sent ->
            if (sent < 60_000 || sent % 3000 >= 20) 0.0 else 20.0 + ((sent - 60_000) / 10_000).toInt() * 5
        }
        assertEquals(0, buffer.underruns)
        assertTrue("target ${buffer.targetMs}", buffer.targetMs >= 70)
    }

    @Test
    fun targetStaysUpForAMinuteAfterTheLastStallThenDecays() {
        assertTrue(simulate(70, late = stalls(listOf(5, 20), 100)).targetMs >= 120)
        assertEquals(40.0, simulate(200, late = stalls(listOf(5, 20), 100)).targetMs, 0.5)
        val clean = simulate(60) { 0.0 }
        assertEquals(0, clean.underruns)
        assertEquals(40.0, clean.targetMs, 0.5)
    }

    @Test
    fun outageIsNotLearnedAsJitter() {
        val buffer = simulate(20, late = stalls(listOf(5), 1000))
        assertTrue("target ${buffer.targetMs}", buffer.targetMs <= 80)
    }

    @Test
    fun clockDriftIsNotLateness() {
        for (drift in listOf(0.005, -0.005)) {
            val buffer = simulate(60, drift) { 0.0 }
            assertTrue("underruns at $drift: ${buffer.underruns}", buffer.underruns <= 1)
            assertTrue("target at $drift: ${buffer.targetMs}", buffer.targetMs <= 50)
        }
    }

    @Test
    fun partialAndZeroWritesRetryAllSamplesInOrder() {
        val pcm = ShortArray(20) { it.toShort() }
        val received = mutableListOf<Short>()
        val sizes = ArrayDeque(listOf(4, 0, 2, 14))
        val frames = writeAudioFully(pcm, { true }) { offset, remaining ->
            val n = sizes.removeFirst()
            assertTrue(n <= remaining)
            received.addAll(pcm.slice(offset until offset + n))
            n
        }
        assertEquals(10, frames)
        assertEquals(pcm.toList(), received)
    }

    @Test
    fun failedWriteIsReportedAndReleaseCancelsPendingWrite() {
        assertThrows(IllegalStateException::class.java) { writeAudioFully(block, { true }) { _, _ -> -6 } }
        var active = true
        assertEquals(2, writeAudioFully(block, { active }) { _, _ -> active = false; 4 })
    }
}
