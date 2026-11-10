package app.airhand.android.controller

import app.airhand.android.core.Codec
import app.airhand.android.core.Quality
import org.json.JSONArray
import org.json.JSONObject

/** Mirrors airhand_proto::video. Unknown decoder limits are null, not guessed from a probe size. */
data class DecodeCapability(val codec: Codec, val maxWidth: Int? = null, val maxHeight: Int? = null, val maxFps: Int? = null) {
    fun toJson() = JSONObject().put("codec", codec.wire).put("maxWidth", maxWidth ?: JSONObject.NULL)
        .put("maxHeight", maxHeight ?: JSONObject.NULL).put("maxFps", maxFps ?: JSONObject.NULL)
}
data class ReceiverCapabilities(val codecs: List<DecodeCapability>) {
    fun toJson() = JSONObject().put("codecs", JSONArray().also { a -> codecs.forEach { a.put(it.toJson()) } })
}
data class ViewRequest(val requestId: Long, val preference: Quality) {
    fun toJson() = JSONObject().put("requestId", requestId).put("preference", preference.toJson())
}
data class MediaInstance(val streamId: Long = 0, val generation: Long = 0) {
    fun toJson() = JSONObject().put("streamId", streamId).put("generation", generation)
}
data class VideoContext(val configId: Long, val instance: MediaInstance = MediaInstance()) {
    fun toJson() = JSONObject().put("configId", configId).put("instance", instance.toJson())
    companion object {
        fun parse(o: JSONObject): VideoContext {
            val i = o.getJSONObject("instance")
            return VideoContext(o.getLong("configId"), MediaInstance(i.getLong("streamId"), i.getLong("generation")))
        }
    }
}
data class VideoConfig(val context: VideoContext, val requestId: Long, val codec: Codec?, val width: Int,
    val height: Int, val fps: Int, val bitrateKbps: Int) {
    companion object {
        fun parse(o: JSONObject) = VideoConfig(VideoContext.parse(o.getJSONObject("context")), o.getLong("requestId"),
            Codec.fromWire(o.optString("codec")), o.getInt("width"), o.getInt("height"), o.getInt("fps"), o.getInt("bitrateKbps"))
    }
}

/** Cumulative counters scoped to one video context. Called exclusively on the decoder thread. */
class VideoFeedbackCollector {
    private var context: VideoContext? = null
    private var received = 0
    private var decoded: Int? = null
    private var presented: Int? = null
    private var sample = 0L
    private var dropped = 0L
    private var errors = 0L
    private var decodeUs = 0L
    private var dirty = false

    fun reset() { context = null; dirty = false }
    fun received(seq: Int, c: VideoContext?) {
        if (c == null) return
        if (c != context) {
            context = c
            decoded = null; presented = null; sample = 0; dropped = 0; errors = 0; decodeUs = 0
        }
        received = seq
        dirty = true
    }
    fun decoded(seq: Int, c: VideoContext?, timeUs: Long) {
        if (c == null || c != context) return
        if (decoded != null && seq - decoded!! < 0) return
        decoded = seq
        val us = timeUs.coerceIn(0, 60_000_000)
        decodeUs = if (decodeUs == 0L) us else (decodeUs * 9 + us) / 10
        dirty = true
    }
    fun presented(seq: Int, c: VideoContext?) {
        if (c == null || c != context) return
        if (presented == null || seq - presented!! >= 0) presented = seq
        dirty = true
    }
    fun dropped(count: Int = 1) { dropped = (dropped + count).coerceAtMost(0xffffffffL); dirty = true }
    fun error() { errors = (errors + 1).coerceAtMost(0xffffffffL); dirty = true }
    fun sample(queueFrames: Int, queueMs: Long): JSONObject? {
        val c = context ?: return null
        if (!dirty && queueFrames == 0 || sample == 0xffffffffL) return null
        dirty = false
        fun unsigned(seq: Int?) = seq?.let { it.toLong() and 0xffffffffL } ?: JSONObject.NULL
        return JSONObject().put("context", c.toJson()).put("sampleId", ++sample)
            .put("receivedSeq", unsigned(received)).put("decodedSeq", unsigned(decoded)).put("presentedSeq", unsigned(presented))
            .put("pendingFrames", queueFrames.coerceIn(0, 1024)).put("oldestPendingMs", queueMs.coerceIn(0, 60_000))
            .put("decodeTimeUs", decodeUs).put("droppedFrames", dropped).put("decoderErrors", errors)
    }
}
