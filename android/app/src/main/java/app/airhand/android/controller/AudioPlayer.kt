package app.airhand.android.controller

import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTimestamp
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.os.Process
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.LinkedBlockingQueue

/** 一个 Opus 包（48 kHz 立体声，20 ms），保留与视频同源的采集时间。 */
class AudioPacket(val start: Boolean, val ptsUs: Long, val data: ByteArray, val offset: Int, val length: Int)

/** MediaCodec 解码 → 自适应 PCM 抖动缓冲 → AudioTrack。解码和输出都在专用音频线程。 */
class AudioPlayer(
    private val sync: AvSync,
    private val clockToken: Long,
    private val onFailure: (String) -> Unit,
) {
    private val queue = LinkedBlockingQueue<AudioPacket>(32)
    @Volatile private var running = true
    private val thread = Thread({ loop() }, "airhand-audio").apply { start() }

    fun submit(p: AudioPacket) {
        if (!running) return
        // 只在极端积压时淘汰最旧的包，保留最近的连续声音。
        while (!queue.offer(p)) queue.poll()
    }

    fun release() {
        running = false
        thread.interrupt()
    }

    private fun loop() {
        var dec: MediaCodec? = null
        var track: AudioTrack? = null
        try {
            Process.setThreadPriority(Process.THREAD_PRIORITY_AUDIO)
            dec = createDecoder()
            track = createTrack()
            track.play()
            val jitter = AudioBuffer(RATE)
            val inflight = ArrayDeque<AudioPacket>()
            val info = MediaCodec.BufferInfo()
            val timestamp = AudioTimestamp()
            val block = ShortArray(RATE / 100 * 2) // 10 ms；阻塞写入负责按声卡速度推进循环
            var pcm = ShortArray(0)
            var written = 0L
            var lastClockNs = Long.MIN_VALUE
            var lastPcmNs = System.nanoTime()

            fun drain() {
                while (running) {
                    val index = dec.dequeueOutputBuffer(info, 0)
                    if (index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                        val f = dec.outputFormat
                        check(f.getInteger(MediaFormat.KEY_SAMPLE_RATE) == RATE && f.getInteger(MediaFormat.KEY_CHANNEL_COUNT) == 2) {
                            "unexpected Opus output format: $f"
                        }
                        if (f.containsKey(MediaFormat.KEY_PCM_ENCODING)) check(f.getInteger(MediaFormat.KEY_PCM_ENCODING) == AudioFormat.ENCODING_PCM_16BIT)
                        continue
                    }
                    if (index < 0) return
                    try {
                        if (info.size == 0 || info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) continue
                        // 部分 Opus 解码器会连续推算输出 PTS，静音后不可信；按送入顺序取原始采集时间。
                        val p = inflight.removeFirstOrNull() ?: error("Opus output without an input packet")
                        val out = dec.getOutputBuffer(index) ?: error("no Opus output buffer")
                        check(info.size % FRAME_BYTES == 0) { "unaligned Opus PCM: ${info.size}" }
                        val count = info.size / 2
                        if (pcm.size < count) pcm = ShortArray(count)
                        out.position(info.offset)
                        out.limit(info.offset + info.size)
                        out.order(ByteOrder.nativeOrder()).asShortBuffer().get(pcm, 0, count)
                        // 第一包会裁掉编码器前瞻，裁掉的是开头的采样。
                        val skipped = (RATE / 50 - count / 2).coerceAtLeast(0)
                        jitter.push(pcm, count, p.start, p.ptsUs + skipped * 1_000_000L / RATE)
                        lastPcmNs = System.nanoTime()
                    } finally {
                        dec.releaseOutputBuffer(index, false)
                    }
                }
            }

            while (running) {
                drain()
                // 输入暂时不可用时保留包，下次再送；输出独立排空，格式变化也不会中断排空。
                repeat(8) {
                    val p = queue.peek() ?: return@repeat
                    val index = dec.dequeueInputBuffer(0)
                    if (index < 0) return@repeat
                    val packet = queue.poll() ?: p
                    dec.getInputBuffer(index)!!.apply { clear(); put(packet.data, packet.offset, packet.length) }
                    dec.queueInputBuffer(index, 0, packet.length, packet.ptsUs, 0)
                    inflight.addLast(packet)
                    drain()
                }
                jitter.setExtraDelay(sync.extraMs)
                val clock = jitter.pull(block, finishBurst = System.nanoTime() - lastPcmNs > 100_000_000L)
                val blockStart = written
                written += writeAudioFully(block, { running }) { offset, length ->
                    track.write(block, offset, length, AudioTrack.WRITE_BLOCKING)
                }
                val now = System.nanoTime()
                if (running && clock != null && (lastClockNs == Long.MIN_VALUE || now - lastClockNs >= 100_000_000L)) {
                    // AudioTimestamp 包含实际输出路径延迟（例如蓝牙）；不可用时退回播放头估算。
                    val timestampValid = track.getTimestamp(timestamp) &&
                        timestamp.nanoTime > 0 && now - timestamp.nanoTime in 0..500_000_000L &&
                        timestamp.framePosition <= written
                    val position = if (timestampValid) timestamp.framePosition else track.playbackHeadPosition.toLong() and 0xffffffffL
                    val localNs = if (timestampValid) timestamp.nanoTime else now
                    sync.onAudioClock(clockToken, clock.hostUs, localNs + (blockStart - position) * 1_000_000_000L / RATE, clock.appliedMs, now)
                    lastClockNs = now
                }
            }
        } catch (e: Exception) {
            if (running) {
                Log.w(TAG, "audio playback failed: $e")
                onFailure(e.toString())
            }
        } finally {
            running = false
            queue.clear()
            runCatching { track?.pause() }
            runCatching { track?.flush() }
            runCatching { track?.release() }
            runCatching { dec?.stop() }
            runCatching { dec?.release() }
        }
    }

    private fun createTrack(): AudioTrack {
        val min = AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT)
        check(min > 0) { "AudioTrack.getMinBufferSize: $min" }
        // 抖动由软件缓冲处理，硬件缓冲只留够连续输出的空间。
        val size = maxOf(min * 2, 40 * RATE / 1000 * FRAME_BYTES)
        val b = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                    .build(),
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                    .build(),
            )
            .setBufferSizeInBytes(size)
            .setTransferMode(AudioTrack.MODE_STREAM)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) b.setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
        return b.build().also { check(it.state == AudioTrack.STATE_INITIALIZED) { "AudioTrack initialization failed" } }
    }

    companion object {
        private const val TAG = "airhand"
        const val RATE = 48_000
        private const val FRAME_BYTES = 4

        /** Opus 解码器：csd-0 是 OpusHead，csd-1 / csd-2 是编码延迟和 seek 预滚（纳秒，本机字节序） */
        fun createDecoder(): MediaCodec {
            val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, RATE, 2)
            format.setInteger(MediaFormat.KEY_PCM_ENCODING, AudioFormat.ENCODING_PCM_16BIT)
            val preSkip = 312
            val head = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
                .put("OpusHead".toByteArray()).put(1).put(2).putShort(preSkip.toShort())
                .putInt(RATE).putShort(0).put(0)
            head.flip()
            format.setByteBuffer("csd-0", head)
            fun ns(v: Long) = ByteBuffer.allocate(8).order(ByteOrder.nativeOrder()).putLong(v).flip() as ByteBuffer
            format.setByteBuffer("csd-1", ns(preSkip * 1_000_000_000L / RATE))
            format.setByteBuffer("csd-2", ns(80_000_000L))
            val mc = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
            try {
                mc.configure(format, null, null, 0)
                mc.start()
                return mc
            } catch (e: Exception) {
                mc.release()
                throw e
            }
        }
    }
}
