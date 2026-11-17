// 控制台的 Worker：和控制端服务之间的 WebSocket，画面的解码（WebCodecs）和绘制（OffscreenCanvas），声音的解码。
// 主线程（console.ts）只处理 JSON 和界面，它忙的时候画面、确认和声音照样走。
//
// 协议见 crates/airhand-link/src/console.rs。文本消息原样交给主线程；二进制是 `ch u32 LE | 媒体包`（media.ts），
// 按 ch 交给会话的画面（View）：画面一到就确认（ack.ts）再解码，声音解码后直接交给音频线程，光标交给主线程（它要改 CSS）。
// 同时看对方的好几个显示器时（多屏）一个会话有好几路画面：每一路一个 canvas、一个解码器（VideoPlayer），
// 按画面包头里的第几路交给它，确认、要关键帧也各路各算各的。
// 本机麦克风的声音从音频线程直接送到这里（mic-encoder.ts），编码之后发给控制端服务（二进制，见 media.ts 的 micMessage）。
import { AckCoalescer, ackMessage } from "./ack";
import type { Codec } from "./api";
import { AudioStream } from "./audio-decoder";
import { AvSync, type AvSyncStats } from "./avsync";
import type { Area, EnhanceLevel } from "./enhance";
import * as media from "./media";
import { MicEncoder } from "./mic-encoder";
import { VideoPlayer } from "./video";

declare const self: DedicatedWorkerGlobalScope;

/** 主线程 → Worker */
export type ToWorker =
  /** 连接控制端服务（断了之后由主线程决定什么时候重连） */
  | { t: "open"; url: string }
  /** 断开（主线程等回复超时了）：之后和断线一样处理 */
  | { t: "close"; reason: string }
  /** 一条文本消息（JSON 命令） */
  | { t: "send"; text: string }
  /** 一条二进制消息（剪贴板里的文件的内容，见 files.ts） */
  | { t: "binary"; data: ArrayBuffer }
  /** 一个会话的画面（第 0 路的 canvas）：会话建立时交过来，之后一直在这里绘制 */
  | { t: "view"; id: number; canvas: OffscreenCanvas }
  /** 第 stream 路画面的 canvas（多屏）：要看这一路之前交过来 */
  | { t: "canvas"; id: number; stream: number; canvas: OffscreenCanvas }
  | { t: "drop"; id: number }
  /** 第 stream 路画面怎么显示：显示区域（设备像素；null = 1:1 显示）、画面增强的强度（见 enhance.ts） */
  | { t: "layout"; id: number; stream: number; area: Area | null; enhance: EnhanceLevel }
  /** 通道 ch（connect 时选的）的媒体包交给画面 id：这个画面之前的通道作废，解码器、缓冲从头开始 */
  | { t: "bind"; ch: number; id: number }
  /** ch 的会话 ID（connect 的回复）：现在可以确认画面了 */
  | { t: "sid"; ch: number; sid: number }
  /** 会话结束了：ch 的包不再要 */
  | { t: "unbind"; ch: number }
  /** 音画同步开关 */
  | { t: "sync"; id: number; on: boolean }
  /** 声音输出（音频线程的端口，见 audio.ts）；null = 不播放 */
  | { t: "audio"; id: number; port: MessagePort | null }
  /** 本机麦克风（采集它的音频线程的端口，见 mic.ts）；null = 不发 */
  | { t: "mic"; id: number; port: MessagePort | null }
  /** 声音时钟：被控端 hostUs 采集的声音在 at（performance.timeOrigin 时基的绝对时间，毫秒）被听到 */
  | { t: "clock"; id: number; hostUs: number; at: number; appliedMs: number }
  /** 主线程要关键帧（例如刚切回这个会话）：每一路 */
  | { t: "keyframe"; id: number };

/** Worker → 主线程 */
export type FromWorker =
  | { t: "open" }
  /** WebSocket 断了（所有通道随之作废） */
  | { t: "closed" }
  | { t: "text"; text: string }
  /** 光标包（见 media.ts） */
  | { t: "cursor"; id: number; pkt: Uint8Array }
  /** 第 stream 路画面的大小变了 */
  | { t: "size"; id: number; stream: number; w: number; h: number }
  | { t: "undecodable"; id: number; codec: Codec }
  | { t: "recovered"; id: number }
  /** 约每秒一次：平均解码耗时（最慢的一路）、音画同步 */
  | { t: "stats"; id: number; decodeMs: number; sync: AvSyncStats }
  /** 被控端复制的文件的一块（回答 remoteRead 的 req）：err 是 FileError 的编号，0xffffffff 是没有错误 */
  | { t: "fileData"; req: number; err: number; data: Uint8Array };

/** 服务 → 控制台的二进制消息里，这个通道号是剪贴板里的文件的内容（见 console.rs 的 FILE_CHANNEL） */
const FILE_CHANNEL = 0xffffffff;

/** 往控制端服务排着没发出去的超过这么多就断开（那边已经不收了） */
const MAX_BUFFERED = 2 * 1024 * 1024;
/** 剪贴板里的文件的内容（每块最多 1 MiB）：排着的超过这么多就等一会儿再发，加上这一块也到不了 MAX_BUFFERED */
const MAX_BUFFERED_FILES = 512 * 1024;
/** 同一个画面两次要关键帧至少隔这么久（解码出错时这里自己要的、主线程要的都算） */
const KEYFRAME_GAP_MS = 500;
const STATS_MS = 1000;

/** 会话的一次连接 */
interface Chan {
  ch: number;
  view: View;
  /** connect 回复之后才知道 */
  sid: number | null;
  /** sid 还不知道时每一路收到的最新一帧，知道之后补发确认（确认是累积的） */
  unacked: Map<number, number>;
  /** 每一路的画面确认（合并连着到的几帧） */
  acks: Map<number, AckCoalescer>;
}

/** 第 stream 路的确认合并（第一次用到时建） */
function acksOf(chan: Chan, stream: number): AckCoalescer {
  let a = chan.acks.get(stream);
  if (!a) {
    a = new AckCoalescer((seq) => ack(chan, stream, seq));
    chan.acks.set(stream, a);
  }
  return a;
}

/** 一路画面：canvas 和它的解码器 */
interface Screen {
  video: VideoPlayer;
  /** 上次请被控端给这一路编关键帧的时刻 */
  keyAt: number;
  /** 这次连接收到过这一路的画面（只看一个显示器时别的路的 canvas 闲着，不用替它们要关键帧） */
  seen: boolean;
}

/** 一个会话的画面（每一路一个）和声音 */
class View {
  readonly id: number;
  readonly sync = new AvSync();
  readonly audio = new AudioStream();
  /** 本机麦克风：编好的声音包发给控制端服务（还没连上、不知道 sid 时丢掉） */
  readonly mic = new MicEncoder((pkt) => {
    const sid = this.chan?.sid;
    if (sid != null) send(media.micMessage(sid, pkt));
  });
  /** 每一路画面（主线程交过来 canvas 的那几路） */
  readonly screens = new Map<number, Screen>();
  /** 现在显示的连接 */
  chan: Chan | null = null;

  constructor(id: number, canvas: OffscreenCanvas) {
    this.id = id;
    this.addCanvas(0, canvas);
  }

  /** 第 stream 路画面画在 canvas 上 */
  addCanvas(stream: number, canvas: OffscreenCanvas) {
    this.screens.get(stream)?.video.dispose();
    const id = this.id;
    const video = new VideoPlayer({
      canvas,
      ctx: canvas.getContext("2d", { alpha: false, desynchronized: true })!,
      sync: this.sync,
      requestKeyframe: () => this.requestKeyframe(stream),
      unsupported: (codec) => post({ t: "undecodable", id, codec }),
      recovered: () => post({ t: "recovered", id }),
      resized: (w, h) => post({ t: "size", id, stream, w, h }),
    });
    this.screens.set(stream, { video, keyAt: -Infinity, seen: false });
  }

  /** 新的连接：解码器、缓冲、音画同步的测量都从头开始 */
  reset() {
    for (const s of this.screens.values()) {
      s.video.reset();
      s.seen = false;
    }
    this.audio.reset();
    this.sync.reset();
  }

  /** 音画同步的输入变了：声音要不要额外等画面 */
  updateSync() {
    const extra = this.sync.update(performance.now());
    if (extra !== null) this.audio.setExtraDelay(extra);
  }

  /** 请被控端给第 stream 路编一个关键帧：所有的请求都经过这里限流（每一路各算各的）；还没连上（不知道 sid）时不要 */
  requestKeyframe(stream: number) {
    const sid = this.chan?.sid;
    const screen = this.screens.get(stream);
    const now = performance.now();
    if (sid == null || !screen || now - screen.keyAt < KEYFRAME_GAP_MS) return;
    screen.keyAt = now;
    send(JSON.stringify(stream === 0 ? { cmd: "keyframe", sid } : { cmd: "keyframe", sid, stream }));
  }

  /** 解码最慢的一路的平均耗时 */
  get decodeMs(): number {
    let ms = 0;
    for (const s of this.screens.values()) ms = Math.max(ms, s.video.decodeAvg);
    return ms;
  }
}

let ws: WebSocket | null = null;
const views = new Map<number, View>();
const chans = new Map<number, Chan>();

// 仅开发模式：方便用 DevTools / 自动化脚本查看画面的状态（例如 scripts/measure-avsync.mjs），和页面的 __airhand 一样
if (import.meta.env.DEV) Object.assign(self, { __airhand: { views } });

function post(m: FromWorker, transfer: Transferable[] = []) {
  self.postMessage(m, { transfer });
}

/** 排着要发的文件的内容：WebSocket 排着的不多了才发（不和画面确认、输入抢；也不因为排多了被当成对方不收而断开） */
const fileQueue: ArrayBuffer[] = [];
let fileTimer: ReturnType<typeof setTimeout> | null = null;

function pumpFiles() {
  if (fileTimer !== null) return;
  while (fileQueue.length > 0) {
    if (ws?.readyState !== WebSocket.OPEN) {
      fileQueue.length = 0;
      return;
    }
    if (ws.bufferedAmount > MAX_BUFFERED_FILES) {
      fileTimer = setTimeout(() => {
        fileTimer = null;
        pumpFiles();
      }, 20);
      return;
    }
    ws.send(fileQueue.shift()!);
  }
}

function send(data: string | ArrayBuffer) {
  if (ws?.readyState !== WebSocket.OPEN) return;
  if (ws.bufferedAmount > MAX_BUFFERED) {
    ws.close(1013, "outgoing queue overflow");
    return;
  }
  ws.send(data);
}

function open(url: string) {
  const s = new WebSocket(url);
  s.binaryType = "arraybuffer";
  ws = s;
  s.onopen = () => post({ t: "open" });
  s.onmessage = (e: MessageEvent<string | ArrayBuffer>) => {
    if (typeof e.data === "string") post({ t: "text", text: e.data });
    else onBinary(e.data);
  };
  // 出错之后总会触发 close。服务那边的会话随这条连接一起结束了
  s.onclose = () => {
    if (ws !== s) return;
    ws = null;
    for (const ch of [...chans.keys()]) unbind(ch);
    post({ t: "closed" });
  };
}

function onBinary(buf: ArrayBuffer) {
  if (buf.byteLength < 4) return;
  const ch = new DataView(buf).getUint32(0, true);
  if (ch === FILE_CHANNEL) {
    if (buf.byteLength < 12) return;
    const dv = new DataView(buf);
    post({ t: "fileData", req: dv.getUint32(4, true), err: dv.getUint32(8, true), data: new Uint8Array(buf, 12) }, [buf]);
    return;
  }
  const chan = chans.get(ch);
  if (!chan) return;
  const pkt = new Uint8Array(buf, 4);
  const dv = new DataView(buf, 4);
  const view = chan.view;
  switch (media.kind(dv)) {
    case media.VIDEO: {
      // 收到就确认（编码格式不认识的也确认，然后丢掉），被控端的在途帧控制因此把这一段也算进去。
      // 还没有 canvas 的路（不应该发生：主线程要看一路之前先交过来）也确认，画面丢掉
      const stream = media.videoStream(dv);
      acksOf(chan, stream).push(dv.getUint32(4, true));
      const h = media.videoHeader(dv);
      const screen = view.screens.get(stream);
      if (h && screen) {
        screen.seen = true;
        screen.video.push(h, media.payload(pkt, dv));
      }
      break;
    }
    case media.AUDIO: {
      const a = media.audioHeader(dv);
      view.audio.push(a.start, a.captureUs, media.payload(pkt, dv));
      break;
    }
    case media.CURSOR:
      post({ t: "cursor", id: view.id, pkt }, [buf]);
      break;
  }
}

function ack(chan: Chan, stream: number, seq: number) {
  if (chan.sid === null) chan.unacked.set(stream, seq);
  else send(ackMessage(chan.sid, seq, stream));
}

function bind(ch: number, id: number) {
  const view = views.get(id);
  if (!view) return;
  if (view.chan) unbind(view.chan.ch);
  view.reset();
  const chan: Chan = { ch, view, sid: null, unacked: new Map(), acks: new Map() };
  chans.set(ch, chan);
  view.chan = chan;
}

function unbind(ch: number) {
  const chan = chans.get(ch);
  if (!chan) return;
  chans.delete(ch);
  for (const a of chan.acks.values()) a.dispose();
  if (chan.view.chan === chan) {
    chan.view.chan = null;
    for (const s of chan.view.screens.values()) s.video.reset();
  }
}

self.onmessage = (e: MessageEvent<ToWorker>) => {
  const m = e.data;
  switch (m.t) {
    case "open":
      open(m.url);
      return;
    case "close":
      ws?.close(1013, m.reason);
      return;
    case "send":
      send(m.text);
      return;
    case "binary":
      fileQueue.push(m.data);
      pumpFiles();
      return;
    case "view":
      views.set(m.id, new View(m.id, m.canvas));
      return;
    case "bind":
      bind(m.ch, m.id);
      return;
    case "unbind":
      unbind(m.ch);
      return;
    case "sid": {
      const chan = chans.get(m.ch);
      if (!chan) return;
      chan.sid = m.sid;
      for (const [stream, seq] of chan.unacked) ack(chan, stream, seq);
      chan.unacked.clear();
      return;
    }
  }
  const view = views.get(m.id);
  if (!view) {
    if (m.t === "audio" || m.t === "mic") m.port?.close();
    return;
  }
  switch (m.t) {
    case "canvas":
      view.addCanvas(m.stream, m.canvas);
      return;
    case "drop":
      if (view.chan) unbind(view.chan.ch);
      for (const s of view.screens.values()) s.video.dispose();
      view.audio.setPort(null);
      view.mic.setPort(null);
      views.delete(m.id);
      return;
    case "sync":
      // 不生效时排着的画面马上显示，声音也不再为画面额外等
      view.sync.enabled = m.on;
      if (!m.on) for (const s of view.screens.values()) s.video.flush();
      view.updateSync();
      return;
    case "audio":
      view.audio.setPort(m.port);
      return;
    case "mic":
      view.mic.setPort(m.port);
      return;
    case "clock":
      view.sync.onAudioClock(m.hostUs, m.at - performance.timeOrigin, m.appliedMs);
      view.updateSync();
      return;
    case "keyframe":
      for (const [stream, s] of view.screens) if (stream === 0 || s.seen) view.requestKeyframe(stream);
      return;
    case "layout":
      view.screens.get(m.stream)?.video.setLayout(m.area, m.enhance);
      return;
  }
};

setInterval(() => {
  for (const v of views.values()) {
    if (v.chan) post({ t: "stats", id: v.id, decodeMs: v.decodeMs, sync: v.sync.stats() });
  }
}, STATS_MS);
