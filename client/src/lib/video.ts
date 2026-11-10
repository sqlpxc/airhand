// Video-only state: WebCodecs lifecycle, bounded A/V delay queue and Canvas.
// Session connection/retry and codec negotiation stay with RemoteSession.
import type { Codec } from "./api";
import type { AvSync } from "./avsync";
import { annexBToAvc, avcC, codecString, h264NeedsAvc, markH264NeedsAvc } from "./codec.ts";
import type { VideoHeader } from "./media.ts";
import { VideoFeedbackCollector, sameContext, type VideoContext, type VideoFeedback } from "./video-model.ts";

interface PendingVideo {
  key: boolean;
  codec: Codec;
  seq: number;
  width: number;
  height: number;
  hostUs: number;
  data: Uint8Array;
  due: number;
  context?: VideoContext;
}
interface VideoOptions {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  sync: AvSync;
  requestKeyframe(): void;
  unsupported(codec: Codec): void;
  recovered(): void;
  resized(): void;
  feedback?(feedback: VideoFeedback): void;
}
const MAX_QUEUED = 120;
const MAX_BYTES = 32 * 1024 * 1024;
export class VideoPlayer {
  decodeAvg = 0;
  lastDrawnHostUs = 0;
  private options: VideoOptions;
  private decoder: VideoDecoder | null = null;
  private configKey = "";
  private codec: Codec = "h264";
  private avc = false;
  private waitKey = true;
  private chunks = 0;
  private decoded = 0;
  private configuredAt = 0;
  private pending = new Map<number, { t0: number; hostUs: number; seq: number; context?: VideoContext }>();
  private telemetry = new VideoFeedbackCollector();
  private feedbackTimer: ReturnType<typeof setInterval> | undefined;
  private context: VideoContext | undefined;
  private queue: PendingVideo[] = [];
  private queuedBytes = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor(options: VideoOptions) { this.options = options; }

  reset(preserveFeedback = false) {
    if (preserveFeedback) {
      this.telemetry.dropped(this.queue.filter(v => sameContext(v.context, this.context)).length
        + [...this.pending.values()].filter(v => sameContext(v.context, this.context)).length);
    }
    if (!preserveFeedback) {
      this.telemetry.reset();
      this.context = undefined;
      clearInterval(this.feedbackTimer);
      this.feedbackTimer = undefined;
    }
    const decoder = this.decoder;
    this.decoder = null; // reject any late callback from the old decoder
    if (decoder && decoder.state !== "closed") decoder.close();
    this.configKey = "";
    this.waitKey = true;
    this.pending.clear();
    clearTimeout(this.timer);
    this.timer = undefined;
    this.queue = [];
    this.queuedBytes = 0;
  }

  /** 一个画面包：头 + 码流（见 media.ts） */
  push(h: VideoHeader, data: Uint8Array) {
    this.context = h.context;
    this.telemetry.received(h.seq, h.context);
    if (h.context && this.options.feedback && this.feedbackTimer === undefined) {
      this.feedbackTimer = setInterval(() => {
        const pending = [...this.pending.values()].filter(p => sameContext(p.context, this.context));
        const age = pending.length ? performance.now() - Math.min(...pending.map(p => p.t0)) : 0;
        const sample = this.telemetry.sample(pending.length, age);
        if (sample) this.options.feedback?.(sample);
      }, 200);
    }
    const hostUs = h.captureUs;
    const v: PendingVideo = {
      key: h.key, codec: h.codec, seq: h.seq, width: h.width, height: h.height, hostUs, data, due: 0, context: h.context,
    };
    if (v.key) this.waitKey = false;
    else if (this.waitKey) { this.telemetry.dropped(); this.options.requestKeyframe(); return; }
    const now = performance.now();
    v.due = this.options.sync.onVideo(hostUs, now, this.decodeAvg);
    this.pump();
    if (this.queue.length === 0 && v.due <= now + 1) { this.decode(v); return; }
    this.queue.push(v);
    this.queuedBytes += v.data.byteLength;
    if (this.queue.length > MAX_QUEUED || this.queuedBytes > MAX_BYTES) {
      // Do not burst 120 stale frames into the decoder after a stalled UI.
      this.reset(true);
      this.options.requestKeyframe();
    } else this.schedule();
  }

  private pump = () => {
    clearTimeout(this.timer);
    this.timer = undefined;
    const now = performance.now();
    while (this.queue.length && this.queue[0].due <= now + 1) {
      const v = this.queue.shift()!;
      this.queuedBytes -= v.data.byteLength;
      this.decode(v);
    }
    this.schedule();
  };
  private schedule() {
    if (this.timer !== undefined || !this.queue.length) return;
    this.timer = setTimeout(this.pump, Math.max(0, this.queue[0].due - performance.now()));
  }
  flush() {
    clearTimeout(this.timer);
    this.timer = undefined;
    const q = this.queue;
    this.queue = [];
    this.queuedBytes = 0;
    for (const v of q) this.decode(v);
  }

  private unsupported(codec: Codec) {
    this.telemetry.error();
    this.reset(true);
    if (codec === "h264" && !h264NeedsAvc()) markH264NeedsAvc();
    else this.options.unsupported(codec);
    this.options.requestKeyframe();
  }

  private decode(v: PendingVideo) {
    let data = v.data;
    if (v.key) {
      const codec = codecString(v.codec, v.data);
      let description: Uint8Array | undefined;
      if (v.codec === "h264" && h264NeedsAvc()) {
        const conv = annexBToAvc(v.data);
        if (!conv.sps || !conv.pps) { this.telemetry.dropped(1, v.context); this.waitKey = true; this.options.requestKeyframe(); return; }
        description = avcC(conv.sps, conv.pps);
        data = conv.data;
      }
      const key = `${codec}/${v.width}x${v.height}/${description?.join(",") ?? "annexb"}`;
      if (!this.decoder || this.decoder.state === "closed" || key !== this.configKey) {
        if (!this.configure(v.codec, codec, v.width, v.height, key, description)) { this.telemetry.dropped(1, v.context); return; }
      }
    } else if (this.waitKey || !this.decoder || this.decoder.state === "closed") {
      this.telemetry.dropped(1, v.context);
      this.waitKey = true;
      this.options.requestKeyframe();
      return;
    }
    const decoder = this.decoder!;
    if (this.decoded === 0 && this.chunks >= 10 && performance.now() - this.configuredAt > 2000) {
      this.telemetry.dropped(1, v.context);
      this.unsupported(this.codec);
      return;
    }
    if (decoder.decodeQueueSize > 8) { this.telemetry.dropped(1, v.context); this.reset(true); this.options.requestKeyframe(); return; }
    if (this.avc && !v.key) data = annexBToAvc(data).data;
    this.chunks++;
    const timestamp = v.seq * 1000;
    this.pending.set(timestamp, { t0: performance.now(), hostUs: v.hostUs, seq: v.seq, context: v.context });
    try {
      decoder.decode(new EncodedVideoChunk({ type: v.key ? "key" : "delta", timestamp, data }));
    } catch {
      this.telemetry.error();
      this.reset(true);
      this.options.requestKeyframe();
    }
  }

  private configure(kind: Codec, codec: string, width: number, height: number, key: string, description?: Uint8Array): boolean {
    const old = this.decoder;
    this.decoder = null;
    if (old && old.state !== "closed") old.close();
    this.telemetry.dropped([...this.pending.values()].filter(v => sameContext(v.context, this.context)).length);
    this.pending.clear();
    this.codec = kind;
    this.avc = description !== undefined;
    this.chunks = this.decoded = 0;
    this.configuredAt = performance.now();
    let decoder: VideoDecoder | null = null;
    try {
      decoder = new VideoDecoder({
        output: (frame) => {
          if (this.decoder !== decoder) { frame.close(); return; }
          if (this.decoded++ === 0) this.options.recovered();
          this.draw(frame);
        },
        error: (e) => {
          console.warn("video decoder error", e);
          if (this.decoder !== decoder) return;
          if (this.decoded === 0) this.unsupported(kind);
          else { this.telemetry.error(); this.reset(true); this.options.requestKeyframe(); }
        },
      });
      this.decoder = decoder;
      decoder.configure({ codec, codedWidth: width, codedHeight: height, optimizeForLatency: true,
        hardwareAcceleration: "no-preference", ...(description ? { description } : {}) });
      this.configKey = key;
      this.waitKey = false;
      return true;
    } catch (e) {
      console.warn("can't configure video decoder", e);
      this.unsupported(kind);
      return false;
    }
  }

  private draw(frame: VideoFrame) {
    try {
      const pending = this.pending.get(frame.timestamp);
      if (pending) {
        this.pending.delete(frame.timestamp);
        const ms = performance.now() - pending.t0;
        this.decodeAvg = this.decodeAvg === 0 ? ms : this.decodeAvg * 0.9 + ms * 0.1;
        this.lastDrawnHostUs = pending.hostUs;
        this.telemetry.decoded(pending.seq, pending.context, ms);
      }
      if (this.pending.size > 32) {
        this.telemetry.dropped([...this.pending.values()].filter(v => sameContext(v.context, this.context)).length);
        this.pending.clear();
      }
      const { canvas, ctx } = this.options;
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
        this.options.resized();
      }
      ctx.drawImage(frame, 0, 0);
      if (pending) this.telemetry.presented(pending.seq, pending.context);
    } finally { frame.close(); }
  }
}
