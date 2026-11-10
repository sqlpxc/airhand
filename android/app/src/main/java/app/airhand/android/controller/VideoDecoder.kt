package app.airhand.android.controller

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import app.airhand.android.core.Codec
import java.nio.ByteBuffer

/** 一帧画面（`airhand_link` 的视频包去掉头），确认回调至多执行一次。 */
class VideoPacket(
    val seq: Int,
    val key: Boolean,
    val codec: Codec,
    val width: Int,
    val height: Int,
    val ptsUs: Long,
    val data: ByteArray,
    val offset: Int,
    val length: Int,
    ack: () -> Unit,
) {
    private val acknowledge = ack
    private var acknowledged = false
    var decodeDeadlineNs = 0L

    fun ack() {
        if (acknowledged) return
        acknowledged = true
        acknowledge()
    }
}

/**
 * 硬件解码（MediaCodec 异步模式），支持按音频播放时钟定时显示到 Surface（TextureView）。
 *
 * * 低延迟模式送入解码器后确认；音画同步模式在有界等待队列中确认，避免主动等待被误判为网络拥塞；
 * * 格式 / 分辨率变了、解码出错、Surface 换了：重建解码器，从下一个关键帧开始（主动向被控端要关键帧）；
 * * 没有 Surface（应用在后台）时收到的帧直接确认、丢掉；
 * * 画面尺寸超出本机解码器的能力（例如竖屏手机的 1080×2400 之于只到 1080p 的电视）：照样试着解码，
 *   同时通过 `onTooLarge` 让会话请对方缩小画面。
 */
class VideoDecoder(
    private val sync: AvSync,
    private val requestKeyframe: () -> Unit,
    private val onError: (String) -> Unit = {},
    private val onTooLarge: (Codec, Int, Int) -> Unit = { _, _, _ -> },
) {
    private val thread = HandlerThread("airhand-decode").apply { start() }
    private val handler = Handler(thread.looper)

    private var surface: Surface? = null
    private var codec: MediaCodec? = null
    private var config: Triple<Codec, Int, Int>? = null
    private val pending = ArrayDeque<VideoPacket>()
    private val freeInputs = ArrayDeque<Int>()
    private var needKey = true
    private var lastKeyRequest = 0L
    private var released = false
    private var pendingBytes = 0L
    private var decodeAvgNs = 0L
    private val decoding = LinkedHashMap<Long, Long>()
    private val feedTask = Runnable { feed() }

    /** 开关改变、声音结束：重新计算等待时间，让画面及时恢复低延迟。 */
    fun refreshTiming() {
        handler.post { feed() }
    }

    fun setSurface(s: Surface?) {
        handler.post {
            if (surface === s) return@post
            surface = s
            pending.forEach { it.ack() }
            pending.clear()
            releaseCodec()
            needKey = true
            if (s != null) askKeyframe(force = true)
        }
    }

    fun submit(p: VideoPacket) {
        handler.post { onPacket(p) }
    }

    /** 会话断了重连：之前排队的帧属于旧会话，丢掉（不再确认），从关键帧重新开始 */
    fun resetStream() {
        handler.post {
            pending.clear()
            releaseCodec()
            needKey = true
        }
    }

    fun release() {
        handler.post {
            released = true
            pending.forEach { it.ack() }
            pending.clear()
            releaseCodec()
            thread.quitSafely()
        }
    }

    private fun askKeyframe(force: Boolean = false) {
        val now = SystemClock.elapsedRealtime()
        if (force || now - lastKeyRequest > 500) {
            lastKeyRequest = now
            requestKeyframe()
        }
    }

    private fun onPacket(p: VideoPacket) {
        if (released || surface == null) {
            p.ack()
            return
        }
        val c = config
        val changed = c == null || codec == null || c.first != p.codec || (p.key && (c.second != p.width || c.third != p.height))
        if (changed) {
            if (!p.key) {
                p.ack()
                needKey = true
                askKeyframe()
                return
            }
            if (!fits(p.codec, p.width, p.height)) {
                Log.w(TAG, "${p.codec.label} ${p.width}x${p.height} is beyond what ${decoderInfo(p.codec)?.name} supports")
                onTooLarge(p.codec, p.width, p.height)
            }
            if (!configure(p)) {
                p.ack()
                return
            }
        }
        if (needKey) {
            if (!p.key) {
                p.ack()
                askKeyframe()
                return
            }
            needKey = false
        }
        val arrivalNs = System.nanoTime()
        sync.onVideo(p.ptsUs, arrivalNs, decodeAvgNs)
        p.decodeDeadlineNs = arrivalNs + 500_000_000L
        pending.addLast(p)
        pendingBytes += p.length
        if (pending.size > (if (sync.enabled) MAX_SYNC_PENDING else MAX_PENDING) || pendingBytes > MAX_PENDING_BYTES) {
            // 解码器卡住了：丢掉排队的帧，等下一个关键帧
            Log.w(TAG, "decoder is not keeping up (${pending.size} frames queued), waiting for a keyframe")
            pending.forEach { it.ack() }
            pending.clear()
            releaseCodec()
            needKey = true
            askKeyframe(force = true)
            return
        }
        if (sync.enabled) p.ack()
        feed()
    }

    private fun feed() {
        handler.removeCallbacks(feedTask)
        val mc = codec ?: return
        while (pending.isNotEmpty() && freeInputs.isNotEmpty() && decoding.size < MAX_PENDING) {
            val first = pending.first()
            val now = System.nanoTime()
            // 等待期间音频可能停了或重新缓冲，以最新播放时钟为准。
            val dueNs = minOf(first.decodeDeadlineNs, sync.decodeTimeNs(first.ptsUs, now, decodeAvgNs))
            val waitNs = dueNs - now
            if (waitNs > 1_000_000) {
                handler.postDelayed(feedTask, (waitNs / 1_000_000).coerceAtMost(20))
                return
            }
            val p = pending.removeFirst()
            pendingBytes -= p.length
            val idx = freeInputs.removeFirst()
            try {
                val buf: ByteBuffer = mc.getInputBuffer(idx) ?: throw IllegalStateException("no input buffer")
                buf.clear()
                if (buf.capacity() < p.length) {
                    throw IllegalStateException("frame of ${p.length} bytes doesn't fit the ${buf.capacity()} byte input buffer")
                }
                buf.put(p.data, p.offset, p.length)
                decoding[p.ptsUs] = System.nanoTime()
                mc.queueInputBuffer(idx, 0, p.length, p.ptsUs, if (p.key) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0)
            } catch (e: Exception) {
                Log.w(TAG, "can't queue a frame: $e")
                p.ack()
                fail("queue: $e")
                return
            }
            p.ack()
        }
    }

    private fun fail(why: String) {
        releaseCodec()
        pending.forEach { it.ack() }
        pending.clear()
        needKey = true
        onError(why)
        askKeyframe(force = true)
    }

    private fun configure(p: VideoPacket): Boolean {
        pending.forEach { it.ack() }
        pending.clear()
        releaseCodec()
        val s = surface ?: return false
        // MediaFormat(MediaFormat) is API 29+. Build both variants from the
        // same recipe so Android 7–9 never calls that constructor.
        val parameterSets = csd(p)
        fun basic() = MediaFormat.createVideoFormat(p.codec.mime, p.width, p.height).apply {
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, (p.width * p.height * 3 / 2).coerceAtLeast(p.length * 2))
            parameterSets.forEachIndexed { i, b -> setByteBuffer("csd-$i", ByteBuffer.wrap(b)) }
        }
        val format = basic()
        val lowLatency = basic().apply {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            setInteger(MediaFormat.KEY_PRIORITY, 0)
            // 各家芯片自己的低延迟开关（不认识的键会被忽略），与 Moonlight 相同
            setInteger("vendor.qti-ext-dec-low-latency.enable", 1)
            setInteger("vendor.qti-ext-dec-picture-order.enable", 1)
            setInteger("vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req", 1)
            setInteger("vendor.rtc-ext-dec-low-latency.enable", 1)
            setInteger("vendor.low-latency.enable", 1)
        }
        val name = decoderInfo(p.codec)?.name
        for (f in listOf(lowLatency, format)) {
            val mc = try {
                // 与判断尺寸用的是同一个解码器
                name?.let { runCatching { MediaCodec.createByCodecName(it) }.getOrNull() }
                    ?: MediaCodec.createDecoderByType(p.codec.mime)
            } catch (e: Exception) {
                Log.w(TAG, "no ${p.codec.mime} decoder: $e")
                onError("no ${p.codec.label} decoder")
                return false
            }
            try {
                mc.setCallback(Callback(mc), handler)
                mc.configure(f, s, null, 0)
                mc.start()
                codec = mc
                config = Triple(p.codec, p.width, p.height)
                freeInputs.clear()
                Log.i(TAG, "decoder ${mc.name} started: ${p.codec.label} ${p.width}x${p.height}${if (f === lowLatency) " (low latency)" else ""}")
                return true
            } catch (e: Exception) {
                Log.w(TAG, "can't start ${mc.name}: $e")
                runCatching { mc.release() }
            }
        }
        onError("can't start the ${p.codec.label} decoder")
        return false
    }

    private fun releaseCodec() {
        handler.removeCallbacks(feedTask)
        pendingBytes = 0
        decoding.clear()
        decodeAvgNs = 0
        val mc = codec ?: return
        codec = null
        config = null
        freeInputs.clear()
        runCatching { mc.stop() }
        runCatching { mc.release() }
    }

    private inner class Callback(private val mc: MediaCodec) : MediaCodec.Callback() {
        override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
            if (codec !== this@VideoDecoder.codec) return
            freeInputs.addLast(index)
            feed()
        }

        override fun onOutputBufferAvailable(codec: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
            if (codec !== this@VideoDecoder.codec) return
            try {
                val now = System.nanoTime()
                decoding.remove(info.presentationTimeUs)?.let { sent ->
                    val elapsed = (now - sent).coerceIn(0, 500_000_000)
                    decodeAvgNs = if (decodeAvgNs == 0L) elapsed else decodeAvgNs + (elapsed - decodeAvgNs) / 10
                }
                val render = info.size > 0 || (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM) == 0
                if (render && sync.enabled) codec.releaseOutputBuffer(index, sync.renderTimeNs(info.presentationTimeUs, now))
                else codec.releaseOutputBuffer(index, render)
                feed()
            } catch (e: Exception) {
                Log.w(TAG, "releaseOutputBuffer: $e")
                fail("render: $e")
            }
        }

        override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
            if (codec !== this@VideoDecoder.codec) return
            Log.w(TAG, "decoder error: ${e.diagnosticInfo}")
            fail("decoder error ${e.diagnosticInfo}")
        }

        override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}
    }

    companion object {
        private const val TAG = "airhand"
        private const val MAX_PENDING = 12
        private const val MAX_SYNC_PENDING = 120
        private const val MAX_PENDING_BYTES = 32 * 1024 * 1024

        /** 从关键帧里取出参数集，作为 csd-0 / csd-1（有的解码器只认这种方式） */
        fun csd(p: VideoPacket): List<ByteArray> {
            val nals = AnnexB.split(p.data, p.offset, p.length)
            val start = byteArrayOf(0, 0, 0, 1)
            fun nal(r: IntRange) = start + p.data.copyOfRange(r.first, r.last + 1)
            return when (p.codec) {
                Codec.H264 -> {
                    val sps = nals.firstOrNull { (p.data[it.first].toInt() and 0x1f) == 7 }
                    val pps = nals.firstOrNull { (p.data[it.first].toInt() and 0x1f) == 8 }
                    if (sps != null && pps != null) listOf(nal(sps), nal(pps)) else emptyList()
                }
                Codec.H265 -> {
                    val ps = nals.filter { ((p.data[it.first].toInt() shr 1) and 0x3f) in 32..34 }
                    if (ps.size >= 3) listOf(ps.fold(ByteArray(0)) { acc, r -> acc + nal(r) }) else emptyList()
                }
                Codec.AV1 -> emptyList()
            }
        }

        private val infos: List<MediaCodecInfo> by lazy {
            runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos.toList() }.getOrDefault(emptyList())
        }

        /** 解码各种格式用的解码器：硬件优先，跳过只能用于加密 / 隧道播放的 */
        private val decoders: Map<Codec, Pair<MediaCodecInfo, String>?> by lazy {
            Codec.entries.associateWith { c ->
                val all = infos.mapNotNull { i ->
                    if (i.isEncoder) return@mapNotNull null
                    val type = i.supportedTypes.firstOrNull { it.equals(c.mime, ignoreCase = true) } ?: return@mapNotNull null
                    val regular = runCatching {
                        val caps = i.getCapabilitiesForType(type)
                        !caps.isFeatureRequired(MediaCodecInfo.CodecCapabilities.FEATURE_SecurePlayback) &&
                            !caps.isFeatureRequired(MediaCodecInfo.CodecCapabilities.FEATURE_TunneledPlayback)
                    }.getOrDefault(false)
                    if (regular) i to type else null
                }
                all.firstOrNull { isHardware(it.first) } ?: all.firstOrNull()
            }
        }

        fun decoderInfo(c: Codec): MediaCodecInfo? = decoders[c]?.first

        /** 本机解码器支不支持这个尺寸（不知道时算支持）。有的解码器要求 16 对齐，对齐之后也试一次 */
        fun fits(c: Codec, w: Int, h: Int): Boolean {
            val (info, type) = decoders[c] ?: return true
            val caps = runCatching { info.getCapabilitiesForType(type).videoCapabilities }.getOrNull() ?: return true
            fun ok(a: Int, b: Int) = runCatching { caps.isSizeSupported(a, b) }.getOrDefault(false)
            return ok(w, h) || ok((w + 15) / 16 * 16, (h + 15) / 16 * 16)
        }

        /** 对方屏幕 `w`×`h`：本机解码器放得下的短边上限（放得下 `maxShort` 时原样返回，0 = 不限） */
        fun fitMaxShort(c: Codec, w: Int, h: Int, maxShort: Int): Int = VideoFit.maxShort(w, h, maxShort) { a, b -> fits(c, a, b) }

        /** 本机能硬件解码的格式（H.264 只要有解码器就算） */
        val decodable: List<Codec> by lazy {
            Codec.entries.filter { c ->
                infos.any { i ->
                    !i.isEncoder && i.supportedTypes.any { it.equals(c.mime, ignoreCase = true) } &&
                        (c == Codec.H264 || isHardware(i))
                }
            }.ifEmpty { listOf(Codec.H264) }
        }

        fun isHardware(i: MediaCodecInfo): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) i.isHardwareAccelerated
            else !i.name.startsWith("OMX.google.") && !i.name.startsWith("c2.android.")
    }
}

/** Annex-B 码流（起始码分隔的 NAL 单元） */
object AnnexB {
    /** 各个 NAL 单元（不含起始码）在 `data` 里的范围 */
    fun split(data: ByteArray, offset: Int, length: Int): List<IntRange> {
        val out = ArrayList<IntRange>()
        val end = offset + length
        var start = -1
        var i = offset
        while (i + 2 < end) {
            if (data[i].toInt() == 0 && data[i + 1].toInt() == 0 && data[i + 2].toInt() == 1) {
                if (start >= 0) {
                    var e = i
                    while (e > start && data[e - 1].toInt() == 0) e--
                    if (e > start) out += start until e
                }
                i += 3
                start = i
            } else {
                i++
            }
        }
        if (start in offset until end) out += start until end
        return out
    }
}
