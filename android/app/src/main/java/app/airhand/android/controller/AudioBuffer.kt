package app.airhand.android.controller

import kotlin.math.floor
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * 单线程 PCM 抖动缓冲。先攒数据再播放，轻微变速吸收网络抖动和两端声卡时钟漂移（与控制台的 jitter.ts 同一套做法）。
 *
 * 每个包到达时量一下它比"最快的那些包"晚了多少（到达时刻 − 采集时刻，相对最小值），目标延迟马上加大到能盖住它，
 * 不等断粮了才一次加 20 ms；最近约一分钟里最大的晚到量也是目标延迟回落的下限，偶尔卡一下（Wi-Fi、网络切换）
 * 的不会一回落就又断粮。
 */
class AudioBuffer(private val rate: Int = 48_000) {
    data class Clock(val hostUs: Long, val appliedMs: Double)
    private data class Anchor(val pos: Long, val hostUs: Long)
    private val perMs = rate / 1000.0
    private val capacity = rate * 2
    private val samples = ShortArray(capacity * 2)
    private val anchors = ArrayDeque<Anchor>()
    private var written = 0L
    private var read = 0.0
    private var playing = false
    private var starved = false
    private var rebuffer = false
    private var target = 60 * perMs
    private var extra = 0.0
    private var average = 0.0
    private var low = Double.POSITIVE_INFINITY
    private var calm = 0.0
    private var windowLeft = rate * 2
    private var fade = 0
    private val fadeFrames = (rate * 0.003).roundToInt().coerceAtLeast(1)
    /** 晚到量的基准：最小的"到达时刻 − 采集时刻"（毫秒，两台设备的时钟差也在里面）；还没有包时为 NaN */
    private var lateBase = Double.NaN
    /** 上一个包的到达时刻（毫秒） */
    private var lateAt = 0.0
    /** 当前评估窗口里的最大晚到量（毫秒） */
    private var lateNow = 0.0
    /** 之前各评估窗口的最大晚到量（毫秒，环形） */
    private val lateWindows = DoubleArray(LATE_WINDOWS)
    private var lateIndex = 0

    val bufferedMs: Double get() = (written - read).coerceAtLeast(0.0) / perMs
    val targetMs: Double get() = target / perMs
    var underruns = 0
        private set
    var skips = 0
        private set
    private val goal: Double get() = target + extra

    fun setExtraDelay(ms: Int) {
        val next = ms.coerceIn(0, 300) * perMs
        if (playing && next - extra > 60 * perMs) rebuffer = true
        extra = next
    }

    /**
     * 写入一个解码后的包（交错立体声）。`start` = 静音后的第一包；`hostUs` = 第一个采样的采集时间（被控端时钟）；
     * `arrivedMs` = 包从网络上到达的时刻（本地单调时钟，毫秒），给了才量晚到量。
     */
    fun push(pcm: ShortArray, count: Int, start: Boolean, hostUs: Long, arrivedMs: Double? = null) {
        if (count < 2) return
        if (starved) {
            starved = false
            if (!start) {
                target = min(MAX_TARGET_MS * perMs, target + 20 * perMs)
                underruns++
                calm = 0.0
            }
        }
        if (arrivedMs != null) measure(arrivedMs, hostUs / 1000.0)
        val offset = (count / 2 - (capacity - 2)).coerceAtLeast(0)
        val frames = count / 2 - offset
        if (written + frames - read > capacity - 2) {
            read = (written + frames - (capacity - 2)).toDouble()
            fade = 0
        }
        anchors.addLast(Anchor(written, hostUs + (offset * 1_000_000L / rate)))
        for (i in 0 until frames) {
            val dst = ((written + i) % capacity).toInt() * 2
            samples[dst] = pcm[(offset + i) * 2]
            samples[dst + 1] = pcm[(offset + i) * 2 + 1]
        }
        written += frames
        pruneAnchors()
    }

    /** 填充一块交错立体声，并返回该块第一个采样的采集时钟。静音 / 重新缓冲时不报告时钟。 */
    fun pull(out: ShortArray, finishBurst: Boolean = false): Clock? {
        out.fill(0)
        if (!playing) {
            // 很短的提示音也要播完，不能永远等不到目标缓冲量。
            if (written <= read || (written - read < goal && !finishBurst)) return null
            playing = true
            fade = 0
            average = written - read
        }
        if (written - read > goal + 150 * perMs) {
            read = written - goal
            average = goal
            fade = 0
            skips++
        }
        pruneAnchors()
        val anchor = anchors.firstOrNull()?.takeIf { it.pos <= read }
        val clock = if (anchor != null && !rebuffer) Clock(
            anchor.hostUs + ((read - anchor.pos) * 1_000_000 / rate).toLong(),
            min(extra, (average - target).coerceAtLeast(0.0)) / perMs,
        ) else null
        low = min(low, written - read)
        average += (written - read - average) * 0.02
        val errorMs = (average - goal) / perMs
        val adjustment = when {
            errorMs > 10 -> min(0.02, (errorMs - 10) / 2000)
            errorMs < -10 -> -min(0.01, (-errorMs - 10) / 2000)
            else -> 0.0
        }
        val step = if (finishBurst) 1.0 else 1 + adjustment
        val frames = out.size / 2
        var i = 0
        while (i < frames) {
            val pos = floor(read).toLong()
            if (pos >= written || (pos + 1 >= written && !finishBurst)) break
            val fraction = read - pos
            val a = (pos % capacity).toInt() * 2
            val b = (min(pos + 1, written - 1) % capacity).toInt() * 2
            var gain = if (fade < fadeFrames) fade++.toDouble() / fadeFrames else 1.0
            if (rebuffer) gain *= 1 - i.toDouble() / frames
            for (channel in 0..1) {
                val value = samples[a + channel] + (samples[b + channel] - samples[a + channel]) * fraction
                out[i * 2 + channel] = (value * gain).roundToInt().coerceIn(-32768, 32767).toShort()
            }
            read = min(written.toDouble(), read + step)
            i++
        }
        if (i < frames) {
            playing = false
            starved = !rebuffer
        }
        if (rebuffer) {
            rebuffer = false
            playing = false
        }
        calm += i * step
        windowLeft -= frames
        if (windowLeft <= 0) {
            windowLeft = rate * 2
            // 回落的下限：最近约一分钟里最大的晚到量也要盖得住
            val floor = (recentLate() + LATE_MARGIN_MS).coerceIn(MIN_TARGET_MS, MAX_TARGET_MS) * perMs
            lateWindows[lateIndex] = lateNow
            lateIndex = (lateIndex + 1) % LATE_WINDOWS
            lateNow = 0.0
            if (playing && calm >= rate * 20 && low > 25 * perMs + extra && target > floor) target = (target - 5 * perMs).coerceAtLeast(floor)
            low = Double.POSITIVE_INFINITY
        }
        return clock.takeIf { i > 0 }
    }

    /** 最近约一分钟里最大的晚到量（毫秒） */
    private fun recentLate(): Double = lateWindows.fold(lateNow) { a, b -> maxOf(a, b) }

    /** 一个包在 `nowMs` 到达，第一个采样在 `hostMs` 采集（被控端时钟）：量晚到量，需要时马上加大目标延迟 */
    private fun measure(nowMs: Double, hostMs: Double) {
        val delay = nowMs - hostMs
        if (!lateBase.isNaN()) lateBase += (nowMs - lateAt).coerceAtLeast(0.0) * BASE_LEAK_MS_PER_S / 1000
        lateAt = nowMs
        if (lateBase.isNaN() || delay < lateBase) {
            lateBase = delay
            return
        }
        val late = delay - lateBase
        // 断线 / 卡死：从这一包重新取基准，跟在它后面涌来的积压也就不算晚到
        if (late > MAX_LATE_MS) {
            lateBase = delay
            return
        }
        lateNow = maxOf(lateNow, late)
        target = maxOf(target, min(MAX_TARGET_MS, late + LATE_MARGIN_MS) * perMs)
    }

    private companion object {
        const val MIN_TARGET_MS = 40.0
        const val MAX_TARGET_MS = 400.0
        /**
         * 目标延迟至少比晚到量多这么多：包到达前缓冲里本来就少半个包（10 ms 的锯齿）、变速的死区（10 ms）、
         * 一个输出块，再留一点余量
         */
        const val LATE_MARGIN_MS = 30.0
        /** 晚到超过这么多算断线 / 卡死（缓冲盖不住），不按它加大目标延迟，只当一次断粮 */
        const val MAX_LATE_MS = 250.0
        /** 晚到量的基准（最小的"到达 − 采集"）每秒最多上浮这么多：跟上两端时钟的偏差、换了一条更慢的路 */
        const val BASE_LEAK_MS_PER_S = 10.0
        /** 记最近这么多个评估窗口（每个 2 秒）里的最大晚到量，约一分钟 */
        const val LATE_WINDOWS = 30
    }

    private fun pruneAnchors() {
        while (anchors.size > 1 && (anchors[1].pos <= read || anchors.size > 256)) anchors.removeFirst()
    }
}

/** AudioTrack 可以短写；保留余下采样重试，负返回值交给调用者报告播放故障。 */
internal fun writeAudioFully(samples: ShortArray, active: () -> Boolean, write: (Int, Int) -> Int): Int {
    var offset = 0
    while (offset < samples.size && active()) {
        val n = write(offset, samples.size - offset)
        check(n >= 0) { "AudioTrack.write: $n" }
        check(n <= samples.size - offset && n % 2 == 0) { "invalid AudioTrack write size: $n" }
        if (n == 0) {
            Thread.sleep(1)
        } else {
            offset += n
        }
    }
    return offset / 2
}
