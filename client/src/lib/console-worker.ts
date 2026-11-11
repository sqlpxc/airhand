// 控制台的 Worker：和控制端服务之间的 WebSocket，画面的解码（WebCodecs）和绘制（OffscreenCanvas），声音的解码。
// 主线程（console.ts）只处理 JSON 和界面，它忙的时候画面、确认和声音照样走。
//
// 协议见 crates/airhand-link/src/console.rs。文本消息原样交给主线程；二进制是 `ch u32 LE | 媒体包`（media.ts），
// 按 ch 交给会话的画面（View）：画面一到就确认（ack.ts）再解码，声音解码后直接交给音频线程，光标交给主线程（它要改 CSS）。
import { AckCoalescer, ackMessage } from "./ack";
import type { Codec } from "./api";
import { AudioStream } from "./audio-decoder";
import { AvSync, type AvSyncStats } from "./avsync";
import * as media from "./media";
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
  /** 一个会话的画面：会话建立时交过来，之后一直在这里绘制 */
  | { t: "view"; id: number; canvas: OffscreenCanvas }
  | { t: "drop"; id: number }
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
  /** 声音时钟：被控端 hostUs 采集的声音在 at（performance.timeOrigin 时基的绝对时间，毫秒）被听到 */
  | { t: "clock"; id: number; hostUs: number; at: number; appliedMs: number }
  /** 主线程要关键帧（例如刚切回这个会话） */
  | { t: "keyframe"; id: number };

/** Worker → 主线程 */
export type FromWorker =
  | { t: "open" }
  /** WebSocket 断了（所有通道随之作废） */
  | { t: "closed" }
  | { t: "text"; text: string }
  /** 光标包（见 media.ts） */
  | { t: "cursor"; id: number; pkt: Uint8Array }
  /** 画面的大小变了 */
  | { t: "size"; id: number; w: number; h: number }
  | { t: "undecodable"; id: number; codec: Codec }
  | { t: "recovered"; id: number }
  /** 约每秒一次：平均解码耗时、音画同步 */
  | { t: "stats"; id: number; decodeMs: number; sync: AvSyncStats };

/** 往控制端服务排着没发出去的超过这么多就断开（那边已经不收了） */
const MAX_BUFFERED = 2 * 1024 * 1024;
/** 同一个画面两次要关键帧至少隔这么久（解码出错时这里自己要的、主线程要的都算） */
const KEYFRAME_GAP_MS = 500;
const STATS_MS = 1000;

/** 会话的一次连接 */
interface Chan {
  ch: number;
  view: View;
  /** connect 回复之后才知道 */
  sid: number | null;
  /** sid 还不知道时收到的最新一帧，知道之后补发确认（确认是累积的） */
  unacked: number | null;
  /** 画面确认（合并连着到的几帧） */
  acks: AckCoalescer;
}

/** 一个会话的画面和声音 */
class View {
  readonly id: number;
  readonly sync = new AvSync();
  readonly audio = new AudioStream();
  readonly video: VideoPlayer;
  /** 现在显示的连接 */
  chan: Chan | null = null;
  private keyAt = -Infinity;

  constructor(id: number, canvas: OffscreenCanvas) {
    this.id = id;
    this.video = new VideoPlayer({
      canvas,
      ctx: canvas.getContext("2d", { alpha: false, desynchronized: true })!,
      sync: this.sync,
      requestKeyframe: () => this.requestKeyframe(),
      unsupported: (codec) => post({ t: "undecodable", id, codec }),
      recovered: () => post({ t: "recovered", id }),
      resized: () => post({ t: "size", id, w: canvas.width, h: canvas.height }),
    });
  }

  /** 新的连接：解码器、缓冲、音画同步的测量都从头开始 */
  reset() {
    this.video.reset();
    this.audio.reset();
    this.sync.reset();
  }

  /** 音画同步的输入变了：声音要不要额外等画面 */
  updateSync() {
    const extra = this.sync.update(performance.now());
    if (extra !== null) this.audio.setExtraDelay(extra);
  }

  /** 请被控端编一个关键帧：所有的请求都经过这里限流；还没连上（不知道 sid）时不要 */
  requestKeyframe() {
    const sid = this.chan?.sid;
    const now = performance.now();
    if (sid == null || now - this.keyAt < KEYFRAME_GAP_MS) return;
    this.keyAt = now;
    send(JSON.stringify({ cmd: "keyframe", sid }));
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
  const chan = chans.get(new DataView(buf).getUint32(0, true));
  if (!chan) return;
  const pkt = new Uint8Array(buf, 4);
  const dv = new DataView(buf, 4);
  const view = chan.view;
  switch (media.kind(dv)) {
    case media.VIDEO: {
      // 收到就确认（编码格式不认识的也确认，然后丢掉），被控端的在途帧控制因此把这一段也算进去
      chan.acks.push(dv.getUint32(4, true));
      const h = media.videoHeader(dv);
      if (h) view.video.push(h, media.payload(pkt, dv));
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

function ack(chan: Chan, seq: number) {
  if (chan.sid === null) chan.unacked = seq;
  else send(ackMessage(chan.sid, seq));
}

function bind(ch: number, id: number) {
  const view = views.get(id);
  if (!view) return;
  if (view.chan) unbind(view.chan.ch);
  view.reset();
  const chan: Chan = { ch, view, sid: null, unacked: null, acks: new AckCoalescer((seq) => ack(chan, seq)) };
  chans.set(ch, chan);
  view.chan = chan;
}

function unbind(ch: number) {
  const chan = chans.get(ch);
  if (!chan) return;
  chans.delete(ch);
  chan.acks.dispose();
  if (chan.view.chan === chan) {
    chan.view.chan = null;
    chan.view.video.reset();
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
      if (chan.unacked !== null) ack(chan, chan.unacked);
      chan.unacked = null;
      return;
    }
  }
  const view = views.get(m.id);
  if (!view) {
    if (m.t === "audio") m.port?.close();
    return;
  }
  switch (m.t) {
    case "drop":
      if (view.chan) unbind(view.chan.ch);
      view.video.reset();
      view.audio.setPort(null);
      views.delete(m.id);
      return;
    case "sync":
      // 不生效时排着的画面马上显示，声音也不再为画面额外等
      view.sync.enabled = m.on;
      if (!m.on) view.video.flush();
      view.updateSync();
      return;
    case "audio":
      view.audio.setPort(m.port);
      return;
    case "clock":
      view.sync.onAudioClock(m.hostUs, m.at - performance.timeOrigin, m.appliedMs);
      view.updateSync();
      return;
    case "keyframe":
      view.requestKeyframe();
      return;
  }
};

setInterval(() => {
  for (const v of views.values()) {
    if (v.chan) post({ t: "stats", id: v.id, decodeMs: v.video.decodeAvg, sync: v.sync.stats() });
  }
}, STATS_MS);
