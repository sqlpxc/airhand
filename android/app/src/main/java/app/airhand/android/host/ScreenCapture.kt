package app.airhand.android.host

import android.content.Context
import android.graphics.Point
import android.graphics.SurfaceTexture
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.opengl.EGLSurface
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.util.DisplayMetrics
import android.util.Log
import android.view.Display
import android.view.Surface
import app.airhand.android.core.Codec
import app.airhand.android.core.DisplayInfo
import app.airhand.android.core.Native
import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * 屏幕采集 + 硬件编码。
 *
 * ```
 * MediaProjection ─▶ VirtualDisplay ─▶ SurfaceTexture ──(GL)──▶ MediaCodec 编码器 ─▶ Rust（发给控制端）
 * ```
 *
 * 中间多一道 GL 是为了像桌面版一样控制节奏：只有画面变了、到了帧率间隔、并且网络跟得上
 * （Rust 的在途帧控制，`hostWaitWindow`）时才把**最新**的画面送进编码器；画面静止时控制端要关键帧，
 * 也能把上一帧重新画一遍编出来。没人看的时候虚拟显示器不输出画面、编码器也释放掉，不费电。
 *
 * Android 14 起一个 MediaProjection 只能建一次虚拟显示器，所以它一直保留，只换 Surface / 改尺寸。
 */
class ScreenCapture(
    private val context: Context,
    private val projection: MediaProjection,
    private val handle: () -> Long,
) {
    @Volatile
    private var running = true

    @Volatile
    private var active = false

    @Volatile
    private var displayChanged = true
    private val lock = Object()
    private var frameAvailable = false

    private val callbacks = HandlerThread("airhand-capture-cb").apply { start() }
    private val callbackHandler = Handler(callbacks.looper)
    private val displayManager = context.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) {}
        override fun onDisplayRemoved(displayId: Int) {}
        override fun onDisplayChanged(displayId: Int) {
            if (displayId == Display.DEFAULT_DISPLAY) {
                displayChanged = true
                wake()
            }
        }
    }

    private val thread = Thread({ loop() }, "airhand-capture")

    init {
        displayManager.registerDisplayListener(displayListener, Handler(Looper.getMainLooper()))
        thread.start()
    }

    /** 有没有人在看（没人看时不采集、不编码） */
    fun setActive(a: Boolean) {
        active = a
        wake()
    }

    fun release() {
        running = false
        wake()
        displayManager.unregisterDisplayListener(displayListener)
        thread.join(2000)
        callbacks.quitSafely()
    }

    private fun wake() = synchronized(lock) { lock.notifyAll() }

    // ───────────────────────────── 渲染线程 ─────────────────────────────

    private lateinit var egl: Egl
    private lateinit var blit: OesBlit
    private lateinit var texture: SurfaceTexture
    private lateinit var textureSurface: Surface
    private var virtualDisplay: VirtualDisplay? = null
    private var vdSize = Point(0, 0)
    private var real = Point(0, 0)
    private var dpi = 320

    private var encoder: Encoder? = null
    private var settings = Tick(fps = 30, maxShort = 1080, codec = Codec.H264, kbps = 4000)

    private data class Tick(val fps: Int, val maxShort: Int, val codec: Codec, val kbps: Int)

    private fun loop() {
        try {
            egl = Egl()
            blit = OesBlit()
            texture = SurfaceTexture(blit.texture)
            texture.setOnFrameAvailableListener({
                synchronized(lock) {
                    frameAvailable = true
                    lock.notifyAll()
                }
            }, callbackHandler)
            textureSurface = Surface(texture)
            render()
        } catch (e: Throwable) {
            Log.e(TAG, "screen capture failed", e)
            call(JSONObject().put("op", "capture").put("failure", e.toString()))
        } finally {
            encoder?.release()
            encoder = null
            runCatching { virtualDisplay?.release() }
            virtualDisplay = null
            runCatching { textureSurface.release() }
            runCatching { texture.release() }
            runCatching { blit.release() }
            runCatching { egl.release() }
        }
    }

    private fun call(o: JSONObject) {
        val h = handle()
        if (h != 0L) runCatching { Native.hostCall(h, o.toString()) }
    }

    private fun render() {
        var dirty = false
        var force = false
        var nextDue = 0L
        val texMatrix = FloatArray(16)
        var lastPts = 0L
        while (running) {
            if (displayChanged) {
                displayChanged = false
                val r = realSize()
                if (r != real) {
                    real = r
                    Log.i(TAG, "screen is ${r.x}x${r.y}")
                    call(JSONObject().put("op", "displays").put("displays", JSONArray().put(displayInfo(r).toJson())))
                    // 尺寸变了（旋转）：按新尺寸重建编码器
                    encoder?.release()
                    encoder = null
                }
            }
            if (!active) {
                if (encoder != null) {
                    encoder?.release()
                    encoder = null
                    virtualDisplay?.surface = null
                    Log.i(TAG, "nobody is watching, capture paused")
                }
                synchronized(lock) { lock.wait(300) }
                continue
            }
            val h = handle()
            if (h == 0L) {
                synchronized(lock) { lock.wait(100) }
                continue
            }

            // Rust 那边的设置 / 码率 / 关键帧请求
            Native.hostTick(h)?.let { t ->
                val o = JSONObject(t)
                val next = Tick(
                    fps = o.optInt("fps", 30).coerceIn(1, 120),
                    maxShort = o.optInt("maxShort", 1080),
                    codec = Codec.fromWire(o.optString("codec")) ?: Codec.H264,
                    kbps = o.optInt("kbps", 4000),
                )
                if (next.maxShort != settings.maxShort || next.codec != settings.codec || next.fps != settings.fps) {
                    encoder?.release()
                    encoder = null
                }
                if (next.kbps != settings.kbps) encoder?.setBitrate(next.kbps)
                settings = next
                if (o.optBoolean("key")) {
                    encoder?.requestKeyframe()
                    force = true
                }
            }

            if (encoder == null) {
                encoder = createEncoder() ?: run {
                    synchronized(lock) { lock.wait(1000) }
                    null
                } ?: continue
                force = true
            }
            val enc = encoder!!

            synchronized(lock) {
                if (!frameAvailable && !force) lock.wait(40)
            }
            if (consumeFrame()) dirty = true
            if (!dirty && !force) continue

            // 帧率节流
            val now = System.nanoTime()
            if (now < nextDue) Thread.sleep((nextDue - now) / 1_000_000, ((nextDue - now) % 1_000_000).toInt())
            // 在途帧控制：网络跟不上时在这里等，期间新的画面覆盖旧的
            while (running && active && !Native.hostWaitWindow(h, 50)) {
                if (consumeFrame()) dirty = true
            }
            if (!running || !active) continue
            if (consumeFrame()) dirty = true

            texture.getTransformMatrix(texMatrix)
            val pts = max(texture.timestamp, lastPts + 1000).let { if (force && !dirty) max(System.nanoTime(), lastPts + 1000) else it }
            lastPts = pts
            try {
                egl.makeCurrent(enc.eglSurface)
                blit.draw(enc.width, enc.height, texMatrix)
                egl.swap(enc.eglSurface, pts)
                egl.makeIdle()
            } catch (e: Exception) {
                Log.w(TAG, "can't draw a frame: $e")
                encoder?.release()
                encoder = null
                continue
            }
            dirty = false
            force = false
            nextDue = System.nanoTime() + 1_000_000_000L / settings.fps
        }
    }

    /** 取走虚拟显示器的新画面（不管送不送编码），返回有没有新画面 */
    private fun consumeFrame(): Boolean {
        val has = synchronized(lock) {
            val f = frameAvailable
            frameAvailable = false
            f
        }
        if (has) texture.updateTexImage()
        return has
    }

    private fun realSize(): Point {
        val d = displayManager.getDisplay(Display.DEFAULT_DISPLAY)
        val m = DisplayMetrics()
        @Suppress("DEPRECATION")
        d.getRealMetrics(m)
        dpi = m.densityDpi
        return Point(m.widthPixels, m.heightPixels)
    }

    private fun createEncoder(): Encoder? {
        if (real.x == 0) real = realSize()
        val codec = settings.codec.takeIf { it in encodable } ?: Codec.H264
        val info = pickEncoder(codec) ?: pickEncoder(Codec.H264) ?: run {
            call(JSONObject().put("op", "capture").put("failure", "no video encoder"))
            return null
        }
        val c = if (info.supportedTypes.any { it.equals(codec.mime, true) }) codec else Codec.H264
        val caps = info.getCapabilitiesForType(c.mime).videoCapabilities
        val (w, h) = encodeSize(real.x, real.y, settings.maxShort, caps)
        if (vdSize.x != w || vdSize.y != h) {
            texture.setDefaultBufferSize(w, h)
            virtualDisplay?.resize(w, h, dpi)
            vdSize = Point(w, h)
        }
        if (virtualDisplay == null) {
            virtualDisplay = projection.createVirtualDisplay(
                "airhand", w, h, dpi, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                textureSurface, null, callbackHandler,
            )
        } else {
            virtualDisplay?.surface = textureSurface
        }
        return try {
            val e = Encoder(info, c, w, h, settings.fps, settings.kbps)
            call(
                JSONObject().put("op", "encoder").put(
                    "info",
                    JSONObject().put("codec", c.wire).put("name", info.name).put("hardware", isHardware(info))
                        .put("width", w).put("height", h),
                ),
            )
            call(JSONObject().put("op", "capture").put("failure", JSONObject.NULL))
            e
        } catch (ex: Exception) {
            Log.w(TAG, "can't start ${info.name} at ${w}x$h: $ex")
            call(JSONObject().put("op", "capture").put("failure", "can't start the encoder ${info.name}: $ex"))
            null
        }
    }

    /** 一个编码器及其输出线程 */
    private inner class Encoder(info: MediaCodecInfo, val codec: Codec, val width: Int, val height: Int, fps: Int, kbps: Int) {
        private val mc: MediaCodec = MediaCodec.createByCodecName(info.name)
        private val inputSurface: Surface
        val eglSurface: EGLSurface

        @Volatile
        private var draining = true
        private val drain: Thread

        init {
            fun basic() = MediaFormat.createVideoFormat(codec.mime, width, height).apply {
                setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
                setInteger(MediaFormat.KEY_BIT_RATE, kbps * 1000)
                setInteger(MediaFormat.KEY_FRAME_RATE, fps)
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 60)
            }
            val full = basic().apply {
                // 超长 GOP：只在需要时（新的控制端、解码出错）要关键帧
                setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 3600)
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
                    setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) setInteger(MediaFormat.KEY_LATENCY, 1)
            }
            try {
                mc.configure(full, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            } catch (e: Exception) {
                // 有的编码器不接受某个可选参数：只留必需的再试一次
                Log.w(TAG, "${info.name} rejected the format, retrying with the basics: $e")
                mc.reset()
                mc.configure(basic(), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            }
            inputSurface = mc.createInputSurface()
            mc.start()
            eglSurface = egl.windowSurface(inputSurface)
            drain = Thread({ drainLoop() }, "airhand-encode-out").apply { start() }
            Log.i(TAG, "encoder ${info.name} started: ${codec.label} ${width}x$height ${fps}fps ${kbps}kbps")
        }

        fun setBitrate(kbps: Int) = runCatching {
            mc.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, kbps * 1000) })
        }

        fun requestKeyframe() = runCatching {
            mc.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
        }

        private fun drainLoop() {
            val info = MediaCodec.BufferInfo()
            while (draining) {
                val idx = try {
                    mc.dequeueOutputBuffer(info, 100_000)
                } catch (e: IllegalStateException) {
                    break
                }
                when {
                    idx >= 0 -> {
                        val buf = runCatching { mc.getOutputBuffer(idx) }.getOrNull()
                        val h = handle()
                        if (buf != null && info.size > 0 && h != 0L) {
                            var flags = 0
                            if (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0) flags = flags or 1
                            if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) flags = flags or 2
                            Native.hostVideo(h, buf, info.offset, info.size, flags, info.presentationTimeUs, codec.id, width, height)
                        }
                        runCatching { mc.releaseOutputBuffer(idx, false) }
                    }
                    idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                        // 有的编码器只在输出格式里给参数集
                        val f = mc.outputFormat
                        val parts = listOfNotNull(f.getByteBufferCompat("csd-0"), f.getByteBufferCompat("csd-1"))
                        val h = handle()
                        if (parts.isNotEmpty() && h != 0L && codec != Codec.AV1) {
                            val total = parts.sumOf { it.remaining() }
                            val direct = ByteBuffer.allocateDirect(total)
                            parts.forEach { direct.put(it.duplicate()) }
                            Native.hostVideo(h, direct, 0, total, 2, 0, codec.id, width, height)
                        }
                    }
                }
            }
        }

        fun release() {
            draining = false
            drain.join(500)
            runCatching { egl.makeIdle() }
            runCatching { egl.destroy(eglSurface) }
            runCatching { mc.stop() }
            runCatching { mc.release() }
            runCatching { inputSurface.release() }
        }
    }

    companion object {
        private const val TAG = "airhand"

        fun displayInfo(r: Point) = DisplayInfo(0, "Screen", r.x, r.y, true)

        /** 本机的屏幕（旋转之后的实际尺寸） */
        fun screen(context: Context): Point {
            val dm = context.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
            val m = DisplayMetrics()
            @Suppress("DEPRECATION")
            dm.getDisplay(Display.DEFAULT_DISPLAY).getRealMetrics(m)
            return Point(m.widthPixels, m.heightPixels)
        }

        private val encoders: List<MediaCodecInfo> by lazy {
            runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.filter { it.isEncoder } }.getOrDefault(emptyList())
        }

        fun isHardware(i: MediaCodecInfo): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) i.isHardwareAccelerated
            else !i.name.startsWith("OMX.google.") && !i.name.startsWith("c2.android.")

        /** 选编码器：硬件优先 */
        fun pickEncoder(codec: Codec): MediaCodecInfo? {
            val all = encoders.filter { i -> i.supportedTypes.any { it.equals(codec.mime, ignoreCase = true) } }
            return all.firstOrNull { isHardware(it) } ?: all.firstOrNull().takeIf { codec == Codec.H264 }
        }

        /** 手机能编码的格式（HEVC / AV1 只算硬件编码器） */
        val encodable: List<Codec> by lazy {
            Codec.entries.filter { pickEncoder(it) != null }.ifEmpty { listOf(Codec.H264) }
        }

        /** 编码尺寸：短边不超过 `maxShort`（0 = 不限），满足编码器的限制，宽高都是 16 的倍数 */
        fun encodeSize(w: Int, h: Int, maxShort: Int, caps: MediaCodecInfo.VideoCapabilities?): Pair<Int, Int> {
            val short = min(w, h)
            var scale = if (maxShort in 1 until short) maxShort.toDouble() / short else 1.0
            fun align(v: Double) = max(16, (v / 16).roundToInt() * 16)
            repeat(30) {
                val (ew, eh) = align(w * scale) to align(h * scale)
                if (caps == null || runCatching { caps.isSizeSupported(ew, eh) }.getOrDefault(false)) return ew to eh
                scale *= 0.9
            }
            return align(w * scale) to align(h * scale)
        }
    }
}

private fun MediaFormat.getByteBufferCompat(key: String): ByteBuffer? =
    if (containsKey(key)) runCatching { getByteBuffer(key) }.getOrNull() else null
