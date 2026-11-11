// 远程会话：输入采集 + 光标同步 + 自动重连；画面和声音在控制台的 Worker 里解码、绘制（见 console.ts）。
//
// 性能要点：
// * 画面是被控端打好的媒体包，经 WebSocket 原样进到 Worker（桌面版和网页版一样，零 JSON），
//   读一下头就把码流切片交给 VideoDecoder，画到 OffscreenCanvas 上：主线程忙的时候也不耽误；
// * 被控端的光标隐藏时（游戏里转视角等）鼠标改发相对移动并锁定本机指针（见 input.ts）；
// * 解码器 optimizeForLatency，帧解码后立即绘制到 desynchronized Canvas，不排队等 vsync；
// * 鼠标移动按 8ms 合并，按键/点击立即发送；
// * 不可见的会话自动降到 5fps，节省带宽与对端 CPU；
// * 画面只缩放一次：按画面在窗口里显示的大小请求分辨率，收到后按 1:1 显示（见 fit.ts）；
// * 只有当前正在查看的会话播放声音，其余会话通知被控端停止采集声音；
// * 音画同步（可选）：按采集时间把画面推迟到与声音对齐后再解码显示，见 avsync.ts（也在 Worker 里）。
import {
  api,
  toProblem,
  type Codec,
  type Command,
  type DisplayInfo,
  type InputEvent,
  type Problem,
  type QualitySettings,
  type ResumeToken,
  type SessionEvent,
  type Platform,
  type SystemAction,
} from "./api";
import * as media from "./media";
import { AudioOutput } from "./audio";
import { ConnectionAttempt } from "./connection-attempt";
import { RemoteInput } from "./input";
import type { AvSyncStats } from "./avsync";
import { bestCodec, codecUsable } from "./codec";
import type { MediaView } from "./console";
import { RECOVERS, type SessionNotice } from "./messages";
import { CODEC_LABEL } from "./presets";
import { store } from "./store";
import { fitSize, stricter, viewMaxShort } from "./fit";

/** waiting = 对方掉线了，等它重新上线（服务器会通知，见 hostOnline） */
/** `approving`：连上了，在等对方的用户同意（之后收到 hello 才是 `live`） */
export type SessionState = "connecting" | "approving" | "live" | "reconnecting" | "waiting" | "closed";
export type ScaleMode = "fit" | "native";

export interface Stats {
  fps: number;
  kbps: number;
  rttMs: number | null;
  width: number;
  height: number;
  decodeMs: number;
  /** 画面是从直连来的（不经过服务器） */
  direct: boolean;
}

export interface EncoderState {
  codec: Codec;
  name: string;
  hardware: boolean;
  width: number;
  height: number;
}

const BACKGROUND_FPS = 5;
/** 显示区域的大小停下来这么久才按新大小请求分辨率（拖动窗口边框时不要每一下都让对方重建编码器） */
const VIEW_LIMIT_DELAY_MS = 400;
const MAX_RETRIES = 30;
/** 等对方上线时，保险起见每隔这么久也试一次（正常情况下服务器会在对方上线时马上通知） */
const WAIT_CHECK_MS = 60_000;
/** 没连上服务器（网页版还有控制端服务）时多久试一次；连上时马上会试（serverConnected） */
const NO_SERVER_RETRY_MS = 5_000;
/**
 * 服务器的明确回答：自动重试只会被当成在猜控制码。
 * kicked：重连凭证作废了（对方断开过所有控制端，或者换了控制码），连上过的会话不再自动重连（见 connect）。
 * 端到端握手的问题要用户决定：对方的身份变了（信任新的身份？）、要配对码、版本不兼容
 */
const GIVE_UP = new Set<Problem["kind"]>([
  "codeNotFound", "rateLimited", "invalidCode", "kicked",
  "hostKeyChanged", "pairingRequired", "pairingFailed", "pairingUnavailable", "peerIncompatible",
]);
/** 与服务器（网页版还有控制端服务）的连接断了：等重新连上再试，不计入重试次数 */
const NO_SERVER = new Set<Problem["kind"]>(["notConnected", "serverLost", "serverUnreachable", "consoleLost"]);

let nextId = 1;

export class RemoteSession {
  readonly id = `s${nextId++}`;
  readonly code: string;
  readonly canvas: HTMLCanvasElement;

  state: SessionState = "connecting";
  /** 上次断开的原因（显示时才变成文字，见 messages.ts） */
  closedReason: Problem | null = null;
  sid: number | null = null;
  hostName = "";
  hostOs = "";
  hostVersion = "";
  displays: DisplayInfo[] = [];
  currentDisplay = 0;
  hostCodecs: Codec[] = ["h264"];
  /** 对方报告的平台（hello 之前为 null，这时按 hostOs 猜） */
  hostPlatform: Platform | null = null;
  /** 对方支持的系统操作（手机的返回、主页……）；电脑为空 */
  hostActions: SystemAction[] = [];
  encoder: EncoderState | null = null;
  stats: Stats = { fps: 0, kbps: 0, rttMs: null, width: 0, height: 0, decodeMs: 0, direct: false };
  /** 画面上方的提示（显示时才变成文字，见 messages.ts） */
  notice: SessionNotice | null = null;
  /** 被控端为什么采集不到声音（英文技术信息）；正常、或者没开声音时为 null */
  audioError: string | null = null;
  settings: QualitySettings;
  scaleMode: ScaleMode = "fit";
  /** 双向同步剪贴板文本 */
  clipboardSync = true;
  /** 播放远程声音（用户开关；实际只在当前查看的会话上播放） */
  audioEnabled = true;
  /** 音画同步开关（用户设置；实际只在播放声音时生效） */
  avSync = false;
  /** 音画同步统计（约每秒更新） */
  syncStats: AvSyncStats = { skewMs: null, videoDelayMs: 0, extraMs: 0 };
  retries = 0;
  /** 这个标签里连上过：之后重连凭证作废了（对方断开过所有控制端）就不再自动重连，见 connect */
  private connectedOnce = false;

  /** 连上了（第一次和每次重连）：记进我的设备 */
  onConnected: ((s: RemoteSession) => void) | null = null;
  /** 画质设置变了：记进我的设备，下次用同样的画质连 */
  onSettingsChange: ((s: RemoteSession) => void) | null = null;

  /** 变化计数，供 React useSyncExternalStore 使用 */
  version = 0;
  private listeners = new Set<() => void>();

  /** 画面和声音（在 Worker 里） */
  private readonly view: MediaView;
  /** 画面的大小（像素） */
  private frame = { w: 16, h: 9 };
  /** Worker 最近报告的平均解码耗时、音画同步状态（随下一次 stats 事件显示） */
  private decodeMs = 0;
  private videoSync: AvSyncStats = { skewMs: null, videoDelayMs: 0, extraMs: 0 };
  private readonly attempt = new ConnectionAttempt();
  private disposed = false;
  private retryTimer: number | undefined;
  /** 控制码（数字），用来认出 hostOnline 通知 */
  private readonly codeNum: number;
  /** 重连凭证：连上过就有，重连时带上（对方换了控制码就作废） */
  private resume: ResumeToken | null = null;
  /** 对方的端到端加密公钥：我的设备里记着的，或者第一次连上时记下的；之后只认它（对不上是 hostKeyChanged） */
  hostKey: string | null = null;
  /** 安全码（连上之后才有）：对方托盘里显示的是同一个 */
  safety: string | null = null;
  /** 下一次连接用的配对码（用户刚输入的），连上之后就不要了 */
  private pairing: string | null = null;
  /** 最近一次开始连接、最近一次收到对方上线通知的时刻 */
  private attemptAt = 0;
  private onlineAt = -1;


  // 声音：解码在 Worker 里，解出来的直接交给音频线程
  readonly audio = new AudioOutput((port) => this.view.setAudio(port));
  private foreground = false;
  /** 当前是否已让被控端发送声音 */
  private audioOn = false;
  /** 音画同步是否生效 */
  private syncOn = false;

  // 布局
  private hostEl: HTMLElement | null = null;
  private resizeObs: ResizeObserver | null = null;
  /** 画面的显示区域（设备像素）；还没挂到页面上时为 null */
  private viewport: { w: number; h: number } | null = null;
  /** 按显示区域向被控端请求的短边上限（0 = 不限），与用户选的上限取较严的发过去 */
  private viewLimit = 0;
  private viewLimitTimer: number | undefined;
  /** 最近一次想改 viewLimit 的时刻（与 performance.now() 同一时基） */
  private viewLimitAt = -Infinity;

  // 输入
  private interactive = false;
  private visible = true;
  private readonly input: RemoteInput;

  // 光标
  private cursorRaw: CursorImage | null = null;
  /** 被控端按槽位缓存的光标形状（动画光标反复用到的那几帧，见 media.ts），只在一个会话里有效 */
  private cursorSlots = new Map<number, CursorImage>();
  private cursorVisible = true;
  private cursorCss = "default";

  /**
   * `saved`：我的设备里记着的这台电脑。带着上次的重连凭证连，对方不在线时等它上线（见 [`open`]），
   * 连上之前标签上就显示它的名字
   */
  constructor(
    code: string,
    settings: QualitySettings,
    saved?: { resume: ResumeToken | null; name: string; os: string; hostKey?: string | null },
    pairing?: string | null,
  ) {
    this.code = code;
    this.codeNum = Number(code.replace(/\D/g, ""));
    this.settings = settings;
    if (saved) {
      this.resume = saved.resume;
      this.hostName = saved.name;
      this.hostOs = saved.os;
      this.hostKey = saved.hostKey ?? null;
    }
    this.pairing = pairing ?? null;
    this.canvas = document.createElement("canvas");
    this.canvas.className = "remote-canvas";
    this.canvas.tabIndex = 0;
    this.canvas.width = this.frame.w;
    this.canvas.height = this.frame.h;
    this.view = api.view(this.canvas, {
      size: (w, h) => {
        this.frame = { w, h };
        this.layout();
      },
      cursor: (pkt) => this.onCursor(pkt),
      undecodable: (codec) => this.onUndecodable(codec),
      recovered: () => {
        if (this.notice?.kind === "h264Undecodable") { this.notice = null; this.emit(); }
      },
      stats: (decodeMs, sync) => {
        this.decodeMs = decodeMs;
        this.videoSync = sync;
      },
    });
    this.input = new RemoteInput(this.canvas, () => this.sid !== null && !this.disposed, (events) => {
      if (this.sid !== null) this.command({ cmd: "input", sid: this.sid, events });
    });
    // 鼠标切换了绝对 / 相对、锁定了 / 解除了：本机指针显示与否、工具栏的状态跟着变
    this.input.onModeChange = () => {
      this.applyCursor();
      this.emit();
    };
    this.audio.onClock = (hostUs, localMs, appliedMs) => this.view.audioClock(hostUs, localMs, appliedMs);
    this.audio.onUnsupported = () => {
      this.notice = { kind: "noOpus" };
      this.emit();
    };
  }

  // ───────────────────────── 订阅 ─────────────────────────

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };
  getVersion = () => this.version;

  private emit() {
    this.version++;
    for (const l of this.listeners) l();
  }

  // ───────────────────────── 连接 ─────────────────────────

  /** 建立连接。首次连接失败会抛出错误。 */
  async start(): Promise<void> {
    const gen = this.attempt.begin();
    // 重连后旧连接的事件可能还会到达，按 gen 丢掉（画面、声音由 Worker 按连接分开：新的连接从头开始）
    const onEvent = (ev: SessionEvent) => {
      if (this.attempt.accepts(gen)) this.onEvent(ev);
    };
    // 光标的槽位是被控端为一个会话分配的：新的会话从头开始
    this.cursorSlots.clear();
    // 在连接之前清掉旧提示：被控端紧跟在 Hello 后面发的提示可能比 connect 的返回值先到
    this.notice = null;
    this.audioError = null;
    this.attemptAt = performance.now();
    let info;
    // 开始时的画质和要不要声音在端到端握手里就告诉对方：它一开始就按这个建编码器，不用先建一个默认的再重建
    const sent = this.effectiveSettings();
    const audio = this.audioEnabled && this.foreground;
    try {
      info = await api.connect(
        this.code,
        { resume: this.resume, hostKey: this.hostKey, pairing: this.pairing, settings: sent, audio },
        this.view,
        onEvent,
        this.clipboardSync,
      );
    } catch (e) {
      // A Closed event already decided whether to retry. An old failure must
      // not overwrite that decision, or restart a deliberately declined call.
      if (!this.attempt.accepts(gen) || this.disposed) return;
      throw e;
    } finally {
      this.attempt.finish(gen);
    }
    if (!this.attempt.accepts(gen) || this.disposed) {
      this.command({ cmd: "disconnect", sid: info.sid });
      return;
    }
    this.resume = info.resume;
    // 第一次连上（或者用户信任了新的身份）：记下对方的公钥，之后只认它；配对码用过了
    this.hostKey = info.hostKey;
    this.safety = info.safety;
    this.pairing = null;
    this.connectedOnce = true;
    this.sid = info.sid;
    this.hostName = info.hostName;
    this.hostOs = info.hostOs;
    // 收到对方的 hello 才算开始（对方可能要先问它的用户，见 onEvent 的 awaitingApproval）；
    // hello 也可能比这里先到，那时已经是 live 了
    this.retries = 0;
    this.closedReason = null;
    // 连接过程中设置变了（例如 Hello 先到，换了编码格式、按显示区域改了分辨率，那时还没有 sid 发不出去）：重发
    if (!sameQuality(this.effectiveSettings(), sent)) this.pushSettings();
    // 对方已经按握手里说的开始（或者没有开始）发声音；连接期间变了（例如切换了标签）再告诉对方
    this.audioOn = audio;
    if (audio) this.audio.enable();
    this.applySyncState();
    this.syncAudio();
    this.emit();
    this.onConnected?.(this);
  }

  /**
   * 连一次。重连凭证作废了（服务器回答 kicked：对方断开过所有控制端，或者换了控制码）就丢掉它：
   * 还没连上过（从我的设备打开、等对方上线时）就用控制码直接连；连上过就不再自动重连（抛出 kicked，见 GIVE_UP）。
   */
  private async connect(): Promise<void> {
    try {
      await this.start();
    } catch (e) {
      if (toProblem(e).kind !== "kicked" || this.resume === null || this.disposed) throw e;
      this.resume = null;
      if (this.connectedOnce) throw e;
      await this.start();
    }
  }

  /**
   * 第一次连接。带着重连凭证（我的设备里保存的）时，对方不在线不算失败：
   * 进入"等待对方上线"，对方一上线就连上（和断线后等对方一样）。其它失败照样抛出。
   *
   * 凭证作废了服务器回答的是 kicked（connect 已经改用控制码连过了），不是 hostOffline：
   * 所以对方不在线时不用再拿控制码试一次——那样会算一次猜错，占掉这个 IP 的额度
   */
  async open(): Promise<void> {
    try {
      await this.connect();
    } catch (e) {
      const p = toProblem(e);
      if (p.kind !== "hostOffline" || this.disposed) throw e;
      this.scheduleRetry(p);
    }
  }

  /** 重连凭证（连上过才有）：记进我的设备，下次对方不在线时可以等它 */
  get resumeToken(): ResumeToken | null {
    return this.resume;
  }

  /**
   * 会话断了 / 重连失败之后：按原因决定什么时候再试。
   *
   * 重连都带着凭证，服务器不会把它们当成猜控制码，对方不在线时回答 hostOffline 并在对方上线时通知，
   * 所以等对方的时候不用反复尝试，也不限次数。
   */
  private scheduleRetry(reason: Problem | null) {
    if (this.disposed) return;
    this.sid = null;
    this.syncAudio();
    this.closedReason = reason;
    clearTimeout(this.retryTimer);
    const kind = reason?.kind;
    let delay: number;
    if (kind === "hostOffline") {
      this.state = "waiting";
      this.retries = 0;
      // 上线通知和 connect 的回答不走同一条通道，可能先到：这次连接开始之后收到过通知就马上再试
      delay = this.onlineAt >= this.attemptAt ? 0 : WAIT_CHECK_MS;
    } else if (kind && GIVE_UP.has(kind)) {
      this.state = "closed";
      this.emit();
      return;
    } else if (kind && NO_SERVER.has(kind)) {
      this.state = "reconnecting";
      delay = NO_SERVER_RETRY_MS;
    } else {
      if (this.retries >= MAX_RETRIES) {
        this.state = "closed";
        this.emit();
        return;
      }
      this.state = "reconnecting";
      delay = Math.min(1000 * 2 ** Math.min(this.retries, 3), 8000);
      this.retries++;
    }
    this.emit();
    this.retryTimer = window.setTimeout(() => this.retryNow(), delay);
  }

  private retryNow() {
    clearTimeout(this.retryTimer);
    this.retryTimer = undefined;
    if (this.disposed || this.attempt.attempting) return;
    this.connect().catch((e) => this.scheduleRetry(toProblem(e)));
  }

  /** 服务器通知：控制码为 `code` 的被控端重新上线了 */
  hostOnline(code: number) {
    if (code !== this.codeNum || this.disposed) return;
    this.onlineAt = performance.now();
    if (this.state === "waiting") this.retryNow();
  }

  /** 与服务器（重新）连上了：服务器不记得断线前谁在等谁，在等的会话马上再试一次 */
  serverConnected() {
    const kind = this.closedReason?.kind;
    if (this.state === "waiting" || (this.state === "reconnecting" && kind && NO_SERVER.has(kind))) this.retryNow();
  }

  /**
   * 带着用户输入的配对码重新连：对方要求配对（pairingRequired / pairingFailed），或者对方的身份变了
   * （hostKeyChanged，用户确认是对方重装了）。配对码认的就是对方，连上之后记下对方现在的公钥；
   * 身份变了不能不验证就换成新的：hostKeyChanged 服务器也伪造得出来（见 airhand_proto::handshake 的"身份"）
   */
  pairWith(pairing: string) {
    this.pairing = pairing.replace(/\D/g, "");
    this.reconnect();
  }

  /** 手动重连（关闭状态下） */
  reconnect() {
    if (this.disposed) return;
    this.retries = 0;
    this.state = "reconnecting";
    this.emit();
    this.retryNow();
  }

  dispose() {
    this.disposed = true;
    this.attempt.invalidate();
    clearTimeout(this.retryTimer);
    this.input.dispose();
    this.view.dispose();
    clearTimeout(this.viewLimitTimer);
    if (this.sid !== null) this.command({ cmd: "disconnect", sid: this.sid });
    this.sid = null;
    this.state = "closed";
    this.audio.disable();
    this.detach();
    this.emit();
  }

  // ───────────────────────── 事件 ─────────────────────────

  private onEvent(ev: SessionEvent) {
    switch (ev.type) {
      case "awaitingApproval":
        this.state = "approving";
        break;
      case "hello":
        this.state = "live";
        this.hostName = ev.hostname;
        this.hostOs = ev.os;
        this.hostPlatform = ev.platform;
        this.hostActions = ev.actions;
        this.hostVersion = ev.version;
        this.displays = ev.displays;
        this.currentDisplay = ev.current;
        this.hostCodecs = ev.codecs;
        this.fitCodec();
        this.updateViewLimit();
        break;
      case "displays":
        this.displays = ev.displays;
        this.currentDisplay = ev.current;
        this.updateViewLimit();
        break;
      case "stats":
        this.stats = {
          fps: ev.fps,
          kbps: ev.kbps,
          rttMs: ev.rttMs,
          width: ev.width,
          height: ev.height,
          decodeMs: this.decodeMs,
          direct: ev.direct,
        };
        this.syncStats = this.videoSync;
        break;
      case "encoder":
        this.encoder = { codec: ev.codec, name: ev.name, hardware: ev.hardware, width: ev.width, height: ev.height };
        break;
      case "notice": {
        const n = ev.notice;
        if (n.kind === "captureRecovered" || n.kind === "audioRecovered") {
          // 对方的问题自己恢复了：撤下对应的提示（别的提示不动）
          if (n.kind === "audioRecovered") this.audioError = null;
          if (this.notice?.kind === RECOVERS[n.kind]) this.notice = null;
        } else if (n.kind === "missingPermissions" && !n.screen && !n.input) {
          // 对方把权限都打开了：撤下之前的权限提示
          if (this.notice?.kind === "missingPermissions" || this.notice?.kind === "androidNoInput") this.notice = null;
        } else if (n.kind === "missingPermissions" && this.hostPlatform === "android") {
          // 对方是手机：缺的是 Airhand 的"远程操作"（无障碍服务），不是 Mac 的系统权限
          this.notice = { kind: "androidNoInput" };
        } else if (n.kind === "audioFailed") {
          this.audioError = n.detail;
          // 声音的问题不盖住正在显示的其它提示（例如缺少权限、Wayland），原因在声音菜单里一直能看到
          if (!this.notice || this.notice.kind === "audioFailed") this.notice = n;
        } else {
          this.notice = n;
        }
        break;
      }
      case "closed":
        if (this.disposed) return;
        this.attempt.invalidate();
        clearTimeout(this.retryTimer);
        this.input.reset();
        if (ev.retry) {
          this.scheduleRetry(ev.reason);
        } else {
          // 对方主动断开：不自动重连
          this.sid = null;
          this.syncAudio();
          this.closedReason = ev.reason;
          // 对方换了控制码 / 断开了所有控制端：凭证作废了（断开所有控制端时对方换了密钥），
          // 之后手动重连就是普通的连接请求
          if (ev.reason.kind === "codeChanged" || ev.reason.kind === "kicked") this.resume = null;
          this.state = "closed";
          this.emit();
        }
        return;
    }
    this.emit();
  }

  // ───────────────────────── 解码 ─────────────────────────

  /** 对会话的操作（失败了也没关系：会话断了会另外收到 closed） */
  private command(c: Command) {
    void api.command(c).catch(() => {});
  }

  /** 音画同步当前是否生效（开关打开且正在播放声音） */
  get syncActive(): boolean {
    return this.syncOn;
  }

  /**
   * 本机实际无法解码这种格式（探测时说支持，但一帧也解不出来）：
   * H.264 改用 AVC 格式重试；其它格式从可用列表里去掉，并让被控端按优先级改用下一个能用的格式。
   * 关键帧 Worker 已经要了。
   */
  private onUndecodable(codec: Codec) {
    if (codec === "h264") {
      this.notice = { kind: "h264Undecodable" };
      this.emit();
      return;
    }
    const decoders = store.get().decoders.filter((c) => c !== codec);
    store.setDecoders(decoders);
    if (this.settings.codec === codec) {
      const to = bestCodec(decoders, this.hostCodecs);
      console.warn(`can't decode ${codec} here, switching to ${to}`);
      this.notice = { kind: "codecFallback", codec: CODEC_LABEL[codec], to: CODEC_LABEL[to] };
      this.updateSettings({ ...this.settings, codec: to });
    } else {
      console.warn(`can't decode ${codec} here`);
    }
  }

  // ───────────────────────── 光标 ─────────────────────────

  /** 光标包（可能是更大的缓冲区中的一段：前面还有 4 字节的 ch） */
  private onCursor(pkt: Uint8Array) {
    const dv = new DataView(pkt.buffer, pkt.byteOffset, pkt.byteLength);
    const c = media.cursorHeader(dv);
    // 负载从头里写的长度处开始（以后的版本可能把头加长）
    const rgba = media.payload(pkt, dv);
    this.cursorVisible = c.visible;
    const s = c.shape;
    if (s && s.width > 0 && s.height > 0 && rgba.byteLength >= s.width * s.height * 4) {
      // 新的形状：存进被控端指定的槽位。复制一份，不一直占着整个包的缓冲区
      const img: CursorImage = {
        w: s.width,
        h: s.height,
        hx: s.hotX,
        hy: s.hotY,
        rgba: new Uint8ClampedArray(rgba.subarray(0, s.width * s.height * 4)),
      };
      this.cursorSlots.set(c.slot, img);
      this.cursorRaw = img;
    } else if (c.cached) {
      // 槽位里已有的形状（例如动画光标的下一帧）；万一没有（不应该发生）就保持原样
      this.cursorRaw = this.cursorSlots.get(c.slot) ?? this.cursorRaw;
    }
    // 被控端的光标隐藏了：鼠标改发相对移动（会回调 onModeChange 刷新指针）
    this.input.setRemoteCursorHidden(!c.visible);
    this.applyCursor();
  }

  /** 按当前显示比例缩放远端光标，生成 CSS cursor */
  private applyCursor() {
    if (!this.cursorVisible || this.input.relative) {
      // 发相对移动时本机指针的位置没有意义，不显示；用户按 Esc 解除了锁定就显示一个普通的指针
      this.setCursor(this.input.relative ? "none" : "default");
      return;
    }
    const c = this.cursorRaw;
    if (!c) {
      this.setCursor("default");
      return;
    }
    const display = this.displays[this.currentDisplay];
    const srcWidth = display?.width || this.frame.w;
    const cssWidth = this.canvas.getBoundingClientRect().width || this.frame.w;
    // 远端物理像素 → 本地 CSS 像素；限制在 Chromium 支持的 128px 以内
    let scale = cssWidth / srcWidth;
    scale = Math.min(Math.max(scale, 0.35), 2, 128 / Math.max(c.w, c.h));
    // 这个形状按差不多的比例生成过（动画光标循环回来）：直接用，不再生成一次 PNG
    if (c.css !== undefined && c.scale !== undefined && Math.abs(scale - c.scale) < 0.02) {
      if (this.cursorCss !== c.css) this.setCursor(c.css);
      return;
    }
    const src = new OffscreenCanvas(c.w, c.h);
    src.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(c.rgba), c.w, c.h), 0, 0);
    const w = Math.max(1, Math.round(c.w * scale));
    const h = Math.max(1, Math.round(c.h * scale));
    const out = document.createElement("canvas");
    out.width = w;
    out.height = h;
    const octx = out.getContext("2d")!;
    octx.imageSmoothingQuality = "high";
    octx.drawImage(src, 0, 0, w, h);
    const hx = Math.min(w - 1, Math.round(c.hx * scale));
    const hy = Math.min(h - 1, Math.round(c.hy * scale));
    c.css = `url(${out.toDataURL("image/png")}) ${hx} ${hy}, default`;
    c.scale = scale;
    this.setCursor(c.css);
  }

  private setCursor(css: string) {
    this.cursorCss = css;
    this.canvas.style.cursor = css;
  }

  // ───────────────────────── 布局 ─────────────────────────

  attach(host: HTMLElement) {
    this.detach();
    this.hostEl = host;
    host.appendChild(this.canvas);
    this.resizeObs = new ResizeObserver(() => this.layout());
    this.resizeObs.observe(host);
    this.layout();
  }

  detach() {
    this.resizeObs?.disconnect();
    this.resizeObs = null;
    if (this.hostEl && this.canvas.parentElement === this.hostEl) this.hostEl.removeChild(this.canvas);
    this.hostEl = null;
  }

  setScaleMode(m: ScaleMode) {
    this.scaleMode = m;
    this.layout();
    this.emit();
  }

  private layout() {
    const host = this.hostEl;
    if (!host) return;
    const { w: fw, h: fh } = this.frame;
    const dpr = devicePixelRatio || 1;
    const r = host.getBoundingClientRect();
    if (r.width > 0 && r.height > 0) this.viewport = { w: Math.floor(r.width * dpr), h: Math.floor(r.height * dpr) };
    // 画面的显示大小（设备像素），以及是否 1:1
    let size = { w: fw, h: fh, exact: true };
    if (!this.nativeScale && this.viewport) size = fitSize(fw, fh, this.viewport.w, this.viewport.h);
    const style = this.canvas.style;
    style.width = `${size.w / dpr}px`;
    style.height = `${size.h / dpr}px`;
    style.left = style.top = "";
    if (size.exact && !this.nativeScale) {
      // 1:1 显示时左上角要落在整数设备像素上：差半个像素浏览器也会插值，文字又糊了
      const c = this.canvas.getBoundingClientRect();
      const snap = (v: number) => (Math.round(v * dpr) - v * dpr) / dpr;
      style.left = `${snap(c.left)}px`;
      style.top = `${snap(c.top)}px`;
    }
    this.applyCursor();
    this.updateViewLimit();
  }

  /** 原始大小（可滚动）显示 */
  private get nativeScale() {
    return this.scaleMode === "native" && this.interactive;
  }

  /**
   * 按显示区域重新算向被控端请求的短边上限（见 fit.ts）。原始大小显示时不限。
   *
   * 一段时间没变过就马上发；连续变化时（拖动窗口边框、窗口动画）等停下来再发，免得对方一直重建编码器。
   */
  private updateViewLimit() {
    const d = this.displays[this.currentDisplay];
    let want: number;
    if (this.nativeScale) want = 0;
    else if (d && this.viewport) want = viewMaxShort(d.width, d.height, this.viewport.w, this.viewport.h);
    else return;
    clearTimeout(this.viewLimitTimer);
    this.viewLimitTimer = undefined;
    if (want === this.viewLimit) return;
    const apply = () => {
      this.viewLimitTimer = undefined;
      this.viewLimit = want;
      this.pushSettings();
    };
    const now = performance.now();
    const quiet = now - this.viewLimitAt >= VIEW_LIMIT_DELAY_MS;
    this.viewLimitAt = now;
    if (quiet) apply();
    else this.viewLimitTimer = window.setTimeout(apply, VIEW_LIMIT_DELAY_MS);
  }

  // ───────────────────────── 可见性 / 设置 ─────────────────────────

  /** 不可见（例如切到其它会话）时降帧率，节省带宽 */
  setVisible(v: boolean) {
    if (this.visible === v) return;
    this.visible = v;
    this.pushSettings();
    if (v) this.view.requestKeyframe();
  }

  /** 是否为当前正在查看的会话（只有它播放声音） */
  setForeground(v: boolean) {
    if (this.foreground === v) return;
    this.foreground = v;
    this.syncAudio();
  }

  setAudioEnabled(v: boolean) {
    this.audioEnabled = v;
    this.syncAudio();
    this.emit();
  }

  /** 音画同步开关：打开后画面会推迟到与声音对齐（操作反馈也会跟着变慢） */
  setAvSync(v: boolean) {
    this.avSync = v;
    this.applySyncState();
    this.emit();
  }

  /** 同步只在开关打开且正在播放声音时生效；不生效时排队的画面立即显示，声音也不再为画面额外等待 */
  private applySyncState() {
    this.syncOn = this.avSync && this.audioOn;
    this.view.setSync(this.syncOn);
  }

  private syncAudio() {
    const want = this.audioEnabled && this.foreground && this.sid !== null && !this.disposed;
    if (want === this.audioOn) return;
    this.audioOn = want;
    if (want) this.audio.enable();
    else this.audio.disable();
    this.applySyncState();
    // 关闭时也通知被控端停止采集，不占带宽
    if (this.sid !== null) this.command({ cmd: "audio", sid: this.sid, enabled: want });
    // 被控端不再采集，之前"无法采集声音"的状态也就作废了（再打开时它会重新报告）
    if (!want) this.clearAudioError();
  }

  private clearAudioError() {
    const shown = this.notice?.kind === "audioFailed";
    if (this.audioError === null && !shown) return;
    this.audioError = null;
    if (shown) this.notice = null;
    this.emit();
  }

  setInteractive(v: boolean) {
    this.interactive = v;
    this.input.interactive = v;
    if (!v) this.releaseAll();
    this.layout();
  }

  updateSettings(s: QualitySettings) {
    this.settings = s;
    this.pushSettings();
    this.emit();
    this.onSettingsChange?.(this);
  }

  /**
   * 知道对方支持哪些格式之后：当前选的格式双方不是都支持时，按优先级（AV1 > HEVC > H.264）换成能用的里最好的。
   * 例如 Mac 被控端没有 AV1 编码器：请求 AV1 时对方自己只会回退到 H.264，这里改为请求 HEVC。
   */
  private fitCodec() {
    const decoders = store.get().decoders;
    if (codecUsable(this.settings.codec, decoders, this.hostCodecs)) return;
    this.updateSettings({ ...this.settings, codec: bestCodec(decoders, this.hostCodecs) });
  }

  /** 实际发给被控端的画质：分辨率不超过画面的显示区域，不可见时降帧率 */
  private effectiveSettings(): QualitySettings {
    const s = { ...this.settings, maxShort: stricter(this.settings.maxShort, this.viewLimit) };
    if (!this.visible) s.fps = Math.min(s.fps, BACKGROUND_FPS);
    return s;
  }

  private pushSettings() {
    if (this.sid === null) return;
    this.command({ cmd: "settings", sid: this.sid, settings: this.effectiveSettings() });
  }

  selectDisplay(index: number) {
    if (this.sid === null || index === this.currentDisplay) return;
    this.releaseAll();
    this.command({ cmd: "display", sid: this.sid, index });
  }

  setClipboardSync(v: boolean) {
    this.clipboardSync = v;
    if (this.sid !== null) this.command({ cmd: "clipboardSync", sid: this.sid, enabled: v });
    this.emit();
  }

  dismissNotice() {
    this.notice = null;
    this.emit();
  }

  // ───────────────────────── 输入 ─────────────────────────

  /** 发送一组按键（例如 Win、Alt+Tab 等无法在本地直接按下的组合键） */
  sendCombo(codes: string[]) {
    const evs: InputEvent[] = [];
    for (const code of codes) evs.push({ type: "key", code, down: true });
    for (const code of [...codes].reverse()) evs.push({ type: "key", code, down: false });
    this.enqueue(evs, true);
  }

  /** 系统操作（对方是手机时的返回、主页……；只发对方在 hello 里列出的） */
  sendSystem(action: SystemAction) {
    if (this.hostActions.includes(action)) this.enqueue([{ type: "system", action }], true);
  }

  sendText(text: string) {
    if (!text) return;
    // 统一换行，按 Unicode 码点分块，不能把 UTF-16 代理对切成两条无效的 JSON 字符串。
    const t = text.replace(/\r\n/g, "\n").replace(/\n/g, "\r");
    let piece = "", count = 0;
    for (const char of t) {
      piece += char;
      if (++count === 512) {
        this.enqueue([{ type: "text", text: piece }], true);
        piece = "";
        count = 0;
      }
    }
    if (piece) this.enqueue([{ type: "text", text: piece }], true);
  }

  /** 用户手动锁定了鼠标（发相对移动） */
  get mouseLocked(): boolean { return this.input.manualLock; }

  /** 手动锁定 / 解锁鼠标（工具栏；快捷键 Ctrl+Alt+Shift+M 由 input.ts 处理）。要在点击里调用 */
  toggleMouseLock() {
    this.input.toggleLock();
    this.focus();
  }

  focus() { this.input.focus(); }
  private releaseAll() { this.input.releaseAll(); }
  private enqueue(events: InputEvent[], urgent: boolean) { this.input.enqueue(events, urgent); }
}

/** 光标的一个形状（RGBA，非预乘） */
interface CursorImage {
  w: number;
  h: number;
  hx: number;
  hy: number;
  rgba: Uint8ClampedArray;
  /** 按 scale 缩放、生成好的 CSS cursor */
  css?: string;
  scale?: number;
}

function sameQuality(a: QualitySettings, b: QualitySettings): boolean {
  return a.fps === b.fps && a.bitrateKbps === b.bitrateKbps && a.maxShort === b.maxShort
    && a.adaptive === b.adaptive && a.codec === b.codec
    && (a.priority ?? "latency") === (b.priority ?? "latency");
}
