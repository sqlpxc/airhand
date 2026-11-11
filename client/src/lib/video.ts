// 一个会话的画面（在 Worker 里，见 console-worker.ts）：WebCodecs 解码器、音画同步的推迟队列、绘制到 OffscreenCanvas。
// 连接、重连和编码格式的协商在主线程的 RemoteSession 里。
//
// 平时 canvas 和画面一样大，由浏览器缩放到显示的大小；打开画面增强时 canvas 就是显示的大小，
// 在显卡上放大、锐化之后画上去（见 enhance.ts）。最后一帧留着：显示区域变了、开关增强时画面静止也能马上重画。
import type { Codec } from "./api";
import type { AvSync } from "./avsync";
import { annexBToAvc, avcC, codecString, h264NeedsAvc, markH264NeedsAvc } from "./codec.ts";
import { enhance as enhanceFrame, renderPlan, type Area, type EnhanceLevel } from "./enhance.ts";
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
  /** 画面的大小（像素）变了 */
  resized(w: number, h: number): void;
  /** 增强（默认用 enhance.ts 的；测试里换掉）：返回画好的 canvas，null 时照原来的方法画 */
  enhance?: (frame: VideoFrame, w: number, h: number, level: EnhanceLevel) => OffscreenCanvas | null;
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
  /** 最后画的一帧（重画用） */
  private last: VideoFrame | null = null;
  private frameW = 0;
  private frameH = 0;
  /** 显示区域（设备像素；null = 1:1 显示）和增强的强度，见 setLayout */
  private area: Area | null = null;
  private level: EnhanceLevel = "off";
  /** 增强的结果画不上去（浏览器不支持把 WebGL 的 canvas 画到 2D canvas 上）：之后不再增强 */
  private enhanceBroken = false;

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

  /** 画面不要了：连最后一帧也放掉 */
  dispose() {
    this.reset();
    this.last?.close();
    this.last = null;
  }

  /**
   * 画面怎么显示：显示区域 `area`（设备像素；null = 1:1 显示）、增强的强度（off = 不增强，由浏览器缩放）。
   * 变了就用最后一帧马上重画（画面静止时被控端不再发新的帧）
   */
  setLayout(area: Area | null, level: EnhanceLevel) {
    if (this.level === level && this.area?.w === area?.w && this.area?.h === area?.h) return;
    this.area = area ? { w: area.w, h: area.h } : null;
    this.level = level;
    if (this.last) this.render(this.last);
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
    const t0 = this.pending.get(frame.timestamp);
    if (t0 !== undefined) {
      this.pending.delete(frame.timestamp);
      const ms = performance.now() - t0;
      this.decodeAvg = this.decodeAvg === 0 ? ms : this.decodeAvg * 0.9 + ms * 0.1;
    }
    if (this.pending.size > 32) this.pending.clear();
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    if (w !== this.frameW || h !== this.frameH) {
      this.frameW = w;
      this.frameH = h;
      this.options.resized(w, h);
    }
    this.last?.close();
    this.last = frame;
    this.render(frame);
  }

  private render(frame: VideoFrame) {
    const { canvas, ctx } = this.options;
    const fw = frame.displayWidth;
    const fh = frame.displayHeight;
    const plan = renderPlan(fw, fh, this.area, this.level);
    // 先增强（失败了 canvas 还没动过）；改 canvas 的大小会清空它，所以紧接着就画
    const enhanced = plan.enhance && !this.enhanceBroken
      ? (this.options.enhance ?? enhanceFrame)(frame, plan.w, plan.h, this.level)
      : null;
    if (canvas.width !== plan.w || canvas.height !== plan.h) {
      canvas.width = plan.w;
      canvas.height = plan.h;
    }
    if (enhanced && this.drawEnhanced(enhanced)) return;
    if (plan.w === fw && plan.h === fh) {
      ctx.drawImage(frame, 0, 0);
    } else {
      // 增强用不了：至少用浏览器的高质量缩放（不锐化）
      ctx.imageSmoothingEnabled = true;
      ctx.imageSmoothingQuality = "high";
      ctx.drawImage(frame, 0, 0, plan.w, plan.h);
    }
  }

  private drawEnhanced(enhanced: OffscreenCanvas): boolean {
    try {
      this.options.ctx.drawImage(enhanced, 0, 0);
      return true;
    } catch (e) {
      console.warn("can't draw the enhanced picture, using the browser's scaling", e);
      this.enhanceBroken = true;
      return false;
    }
  }
}
