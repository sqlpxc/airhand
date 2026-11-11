// 一个会话的画面（在 Worker 里，见 console-worker.ts）：WebCodecs 解码器、音画同步的推迟队列、绘制到 OffscreenCanvas。
// 连接、重连和编码格式的协商在主线程的 RemoteSession 里。
import type { Codec } from "./api";
import type { AvSync } from "./avsync";
import { annexBToAvc, avcC, codecString, h264NeedsAvc, markH264NeedsAvc } from "./codec.ts";
import type { VideoHeader } from "./media.ts";

interface PendingVideo {
  key: boolean;
  codec: Codec;
  seq: number;
  width: number;
  height: number;
  hostUs: number;
  data: Uint8Array;
  due: number;
}
interface VideoOptions {
  canvas: OffscreenCanvas;
  ctx: OffscreenCanvasRenderingContext2D;
  sync: AvSync;
  requestKeyframe(): void;
  unsupported(codec: Codec): void;
  recovered(): void;
  resized(): void;
}
const MAX_QUEUED = 120;
const MAX_BYTES = 32 * 1024 * 1024;
export class VideoPlayer {
  decodeAvg = 0;
  private options: VideoOptions;
  private decoder: VideoDecoder | null = null;
  private configKey = "";
  private codec: Codec = "h264";
  private avc = false;
  private waitKey = true;
  private chunks = 0;
  private decoded = 0;
  private configuredAt = 0;
  private pending = new Map<number, number>();
  private queue: PendingVideo[] = [];
  private queuedBytes = 0;
  private timer: ReturnType<typeof setTimeout> | undefined;

  constructor(options: VideoOptions) { this.options = options; }

  reset() {
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
    const hostUs = h.captureUs;
    const v: PendingVideo = {
      key: h.key, codec: h.codec, seq: h.seq, width: h.width, height: h.height, hostUs, data, due: 0,
    };
    if (v.key) this.waitKey = false;
    else if (this.waitKey) { this.options.requestKeyframe(); return; }
    const now = performance.now();
    v.due = this.options.sync.onVideo(hostUs, now, this.decodeAvg);
    this.pump();
    if (this.queue.length === 0 && v.due <= now + 1) { this.decode(v); return; }
    this.queue.push(v);
    this.queuedBytes += v.data.byteLength;
    if (this.queue.length > MAX_QUEUED || this.queuedBytes > MAX_BYTES) {
      // Do not burst 120 stale frames into the decoder after a stalled UI.
      this.reset();
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
    this.reset();
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
        if (!conv.sps || !conv.pps) { this.waitKey = true; this.options.requestKeyframe(); return; }
        description = avcC(conv.sps, conv.pps);
        data = conv.data;
      }
      const key = `${codec}/${v.width}x${v.height}/${description?.join(",") ?? "annexb"}`;
      if (!this.decoder || this.decoder.state === "closed" || key !== this.configKey) {
        if (!this.configure(v.codec, codec, v.width, v.height, key, description)) return;
      }
    } else if (this.waitKey || !this.decoder || this.decoder.state === "closed") {
      this.waitKey = true;
      this.options.requestKeyframe();
      return;
    }
    const decoder = this.decoder!;
    if (this.decoded === 0 && this.chunks >= 10 && performance.now() - this.configuredAt > 2000) {
      this.unsupported(this.codec);
      return;
    }
    if (decoder.decodeQueueSize > 8) { this.reset(); this.options.requestKeyframe(); return; }
    if (this.avc && !v.key) data = annexBToAvc(data).data;
    this.chunks++;
    const timestamp = v.seq * 1000;
    this.pending.set(timestamp, performance.now());
    try {
      decoder.decode(new EncodedVideoChunk({ type: v.key ? "key" : "delta", timestamp, data }));
    } catch {
      this.reset();
      this.options.requestKeyframe();
    }
  }

  private configure(kind: Codec, codec: string, width: number, height: number, key: string, description?: Uint8Array): boolean {
    const old = this.decoder;
    this.decoder = null;
    if (old && old.state !== "closed") old.close();
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
          else { this.reset(); this.options.requestKeyframe(); }
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
      const t0 = this.pending.get(frame.timestamp);
      if (t0 !== undefined) {
        this.pending.delete(frame.timestamp);
        const ms = performance.now() - t0;
        this.decodeAvg = this.decodeAvg === 0 ? ms : this.decodeAvg * 0.9 + ms * 0.1;
      }
      if (this.pending.size > 32) this.pending.clear();
      const { canvas, ctx } = this.options;
      if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
        this.options.resized();
      }
      ctx.drawImage(frame, 0, 0);
    } finally { frame.close(); }
  }
}
