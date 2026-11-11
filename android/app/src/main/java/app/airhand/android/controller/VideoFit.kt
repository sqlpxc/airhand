package app.airhand.android.controller

import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * 向对方要多大的画面（被控端只认"短边上限"）。
 *
 * * 让画面尺寸落在本机解码器的能力范围内：电视的硬件解码器常常只到 1080p（约 8160 个宏块），
 *   或者只到 4096×2304 这类横着的尺寸，竖屏手机原样的 1080×2400 放不下：解码器起不来或者解不出画面，
 *   一直黑屏（最低画质的 720×1600 就没事）；
 * * 双指放大之后要更清楚的画面（[zoomMaxShort]），不然放大的只是原来那点像素。
 */
object VideoFit {
    /** 缩到这么小还放不下，多半是解码器报的能力不对：不再缩，照原样 */
    private const val MIN_SHORT = 360
    private const val STEP = 8

    /** 放大时要的分辨率按这几档（和画质设置里的分辨率一样）往上取，再往上就是原始分辨率 */
    private val ZOOM_STEPS = intArrayOf(720, 1080, 1440, 2160)

    /** 放大不到这个比例就不再要更高的一档（和桌面控制台的 fit.ts 的 SNAP 一样） */
    private const val SNAP = 1.04f

    /**
     * 双指放大之后要的短边上限：对方的屏幕 `srcW`×`srcH`（像素）整个画面放大到 `shownW`×`shownH`（本机像素）时，
     * 画面不用再被放大的分辨率。按档位往上取，免得手指每动一点都让对方重建编码器；
     * 够到对方屏幕的短边就是原始分辨率（0）
     */
    fun zoomMaxShort(srcW: Int, srcH: Int, shownW: Float, shownH: Float): Int {
        val short = min(srcW, srcH)
        if (short <= 0 || !(shownW > 0f && shownH > 0f)) return 0
        val need = short * min(shownW / srcW, shownH / srcH) / SNAP
        val step = ZOOM_STEPS.firstOrNull { it >= need } ?: return 0
        return if (step >= short) 0 else step
    }

    /**
     * 对方屏幕 `srcW`×`srcH`、想要的短边上限 `maxShort`（0 = 不限）：放得下就原样返回，
     * 否则返回放得下的最大短边上限。`fits` 判断解码器支不支持某个尺寸。
     */
    fun maxShort(srcW: Int, srcH: Int, maxShort: Int, fits: (Int, Int) -> Boolean): Int {
        val short = min(srcW, srcH)
        if (short <= 0) return maxShort
        fun ok(s: Int) = sizes(srcW, srcH, s).all { (w, h) -> fits(w, h) }
        val start = if (maxShort in 1 until short) maxShort else short
        if (ok(start)) return maxShort
        var s = (start - 1) / STEP * STEP
        while (s >= MIN_SHORT) {
            if (ok(s)) return s
            s -= STEP
        }
        return maxShort
    }

    /**
     * 短边上限为 `s` 时被控端编出来的尺寸，两种被控端都算：
     * Android（`ScreenCapture.encodeSize`，16 对齐）和桌面（`encoder::target_size`，偶数）。
     */
    fun sizes(srcW: Int, srcH: Int, s: Int): List<Pair<Int, Int>> {
        val short = min(srcW, srcH)
        val scale = if (s in 1 until short) s.toDouble() / short else 1.0
        fun align16(v: Double) = max(16, (v / 16).roundToInt() * 16)
        fun even(v: Double) = max(64, v.roundToInt() and 1.inv())
        return listOf(
            align16(srcW * scale) to align16(srcH * scale),
            even(srcW * scale) to even(srcH * scale),
        )
    }

    /** 短边上限取较严的那个（0 = 不限） */
    fun stricter(a: Int, b: Int): Int = when {
        a == 0 -> b
        b == 0 -> a
        else -> min(a, b)
    }

    /** 短边上限取较宽的那个（0 = 不限） */
    fun looser(a: Int, b: Int): Int = if (a == 0 || b == 0) 0 else max(a, b)
}
