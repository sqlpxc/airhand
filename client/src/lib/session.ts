// 远程会话：输入采集 + 光标同步 + 自动重连；画面和声音在控制台的 Worker 里解码、绘制（见 console.ts）。
//
// 性能要点：
// * 画面是被控端打好的媒体包，经 WebSocket 原样进到 Worker（桌面版和网页版一样，零 JSON），
//   读一下头就把码流切片交给 VideoDecoder，画到 OffscreenCanvas 上：主线程忙的时候也不耽误；
// * 鼠标三种模式（自动 / 桌面 / 游戏，见 input.ts），按设备记住：自动模式在被控端隐藏光标时（游戏里转视角等）
//   改发相对移动并锁定本机指针；游戏模式一直这样，并让被控端把光标画进画面里（QualitySettings.drawCursor）；
// * 解码器 optimizeForLatency，帧解码后立即绘制到 desynchronized Canvas，不排队等 vsync；
// * 鼠标移动按 8ms 合并，按键/点击立即发送；
// * 不可见的会话自动降到 5fps，节省带宽与对端 CPU；
// * 画面只缩放一次：按画面在窗口里显示的大小请求分辨率，收到后按 1:1 显示（见 fit.ts）；
// * 多屏（可选，默认只看一个显示器）：同时看对方的好几个显示器，每个显示器一路画面、一个 canvas，
//   按它们在对方桌面上的位置拼在一起（见 screens.ts），鼠标在哪一块上就操作哪个显示器；
// * 弹出窗口（可选）：多屏时可以把其中一个显示器放到单独的窗口里（本机也有好几个显示器时一个窗口放一个，各自全屏，
//   见 popout.ts）。那一路画面的 canvas 换到那个窗口里，还是同一个 Worker 画；主窗口里拼的是剩下的几路，至少留一路；
// * 画面增强（可选）：要放大时在 Worker 里用显卡放大、锐化，canvas 就是显示的大小（见 enhance.ts）；
// * 只有当前正在查看的会话播放声音，其余会话通知被控端停止采集声音；
// * 本机麦克风（可选，默认关）：也只发给当前正在查看的会话，被控端放进它的虚拟麦克风（见 mic.ts）；
// * 防窥模式（可选，默认关，装了虚拟显示器驱动的 Windows、允许了辅助功能的 macOS 被控端）：对方的显示器黑屏、键盘鼠标
//   不起作用（Windows 的画面换成一块虚拟显示器）。开关跟着这个标签走：断线重连时在连接选项里带上，对方一直保持防窥；
// * 音画同步（可选）：按采集时间把画面推迟到与声音对齐后再解码显示，见 avsync.ts（也在 Worker 里）。
import {
  api,
  toProblem,
  type Codec,
  type Command,
  type DisplayInfo,
  type InputEvent,
  type MouseMode,
  type Problem,
  type QualitySettings,
  type ResumeToken,
  type SavedDevice,
  type SessionEvent,
  type MicState,
  type Platform,
  type PrivacyState,
  type StreamView,
  type SystemAction,
  type FileEntry,
  type FileError,
} from "./api";
import { download as downloadFiles, LocalFiles } from "./files";
import * as media from "./media";
import { AudioOutput } from "./audio";
import { MicCapture, type MicError } from "./mic";
import { ConnectionAttempt } from "./connection-attempt";
import { RemoteInput } from "./input";
import type { AvSyncStats } from "./avsync";
import { bestCodec, codecUsable } from "./codec";
import type { MediaView } from "./console";
import { RECOVERS, type SessionNotice } from "./messages";
import { CODEC_LABEL } from "./presets";
import { store } from "./store";
import { fitSize, viewMaxShort } from "./fit";
import { renderPlan, type Area, type EnhanceLevel } from "./enhance";
import { hitTest, layoutTiles, placeDisplays, type Rect } from "./screens";
import { Popout } from "./popout";

/** waiting = 对方掉线了，等它重新上线（服务器会通知，见 hostOnline） */
export type SessionState = "connecting" | "live" | "reconnecting" | "waiting" | "closed";
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
/** 一个会话最多同时看几个显示器（airhand_proto::MAX_STREAMS） */
const MAX_STREAMS = 4;
/** 一个光标形状按几种比例生成的 CSS 留着（主窗口和弹出窗口里的比例可能不一样） */
const MADE_CURSORS = 4;
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
  /** 画面：包着每一路画面的 canvas（只看一个显示器时只有一个）。键盘焦点、鼠标事件、锁定指针都在它上面 */
  readonly screen: HTMLDivElement;

  state: SessionState = "connecting";
  /** 上次断开的原因（显示时才变成文字，见 messages.ts） */
  closedReason: Problem | null = null;
  sid: number | null = null;
  hostName = "";
  hostOs = "";
  hostVersion = "";
  displays: DisplayInfo[] = [];
  /** 第 0 路画面是哪个显示器 */
  currentDisplay = 0;
  /** 对方现在每一路画面是哪个显示器（第 i 项是第 i 路） */
  shown: number[] = [0];
  /** 对方最多能同时发几路画面（hello 之前不知道，当作 1） */
  maxStreams = 1;
  /** 同时看对方所有的显示器（用户开关，默认只看一个） */
  allDisplays = false;
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
  /** 对方能不能接本机的麦克风（hello 之后对方会说）；对方的版本旧、还没说时为 null：不显示麦克风 */
  micState: MicState | null = null;
  /** 对方打不开虚拟麦克风的原因（英文技术信息） */
  micDetail: string | null = null;
  /** 把本机麦克风发给对方（用户开关，默认关；实际只在当前查看的会话上发） */
  micEnabled = false;
  /** 本机采集不了麦克风的原因（用户再打开时清掉） */
  micError: MicError | null = null;
  /** 对方的防窥模式（hello 之后对方会说）；对方不支持、还没说时为 null：不显示这个开关 */
  privacyState: PrivacyState | null = null;
  /** 没能打开防窥的原因（英文技术信息） */
  privacyDetail: string | null = null;
  /** 要防窥模式（开关）：重连时在连接选项里带上，对方就一直保持防窥。跟着对方说的状态变 */
  privacyWanted = false;
  /** 对方复制的文件（网页版：等用户点下载，见 files.ts）；没有时为 null */
  remoteFiles: { clip: number; items: FileEntry[] } | null = null;
  /** 正在把对方复制的文件取下来：已经取了多少、一共多少（字节） */
  download: { done: number; total: number } | null = null;
  /** 音画同步统计（约每秒更新） */
  syncStats: AvSyncStats = { skewMs: null, videoDelayMs: 0, extraMs: 0 };
  retries = 0;
  /** 这个标签里连上过：之后重连凭证作废了（对方断开过所有控制端）就不再自动重连，见 connect */
  private connectedOnce = false;

  /** 连上了（第一次和每次重连）：记进我的设备 */
  onConnected: ((s: RemoteSession) => void) | null = null;
  /** 画质设置变了：记进我的设备，下次用同样的画质连 */
  onSettingsChange: ((s: RemoteSession) => void) | null = null;
  /** 按设备记住的偏好，只在对方报告 mac 平台时用于输入 */
  swapCtrlCommand = false;
  onInputSettingsChange: ((s: RemoteSession) => void) | null = null;

  /** 变化计数，供 React useSyncExternalStore 使用 */
  version = 0;
  private listeners = new Set<() => void>();

  /** 画面和声音（在 Worker 里） */
  private readonly view: MediaView;
  /** 每一路画面的 canvas（第 i 项是第 i 路，要看哪一路之前建好、交给 Worker） */
  private readonly canvases: HTMLCanvasElement[] = [];
  /** 每一路画面的 canvas 在哪个窗口里（第 i 项是第 i 路；null = 主窗口，见 placeCanvases） */
  private readonly canvasHome: (Popout | null)[] = [];
  /** 放到单独窗口里的显示器（按名字：对方插拔显示器时序号可能变）。正在打开、还没准备好的在 opening 里 */
  private readonly popouts = new Map<string, Detached>();
  private readonly opening = new Map<string, Popout>();
  /** 每一路画面的大小（像素） */
  private frames: { w: number; h: number }[] = [{ w: 16, h: 9 }];
  /**
   * 只看一个显示器时看哪个（同时看所有的时候它是第 0 路）：用户选的，还没选过时是对方开始时给的。按名字记着：
   * 对方插拔显示器时序号可能变
   */
  private wantName: string | null = null;
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
  // 麦克风：音频线程采集到的直接交给 Worker 编码、发出去
  private readonly mic = new MicCapture((port) => this.view.setMic(port));
  private foreground = false;
  /** 当前是否已让被控端发送声音 */
  private audioOn = false;
  /** 当前是否已让被控端打开麦克风 */
  private micOn = false;
  /** 在采集本机的麦克风（打开中也算） */
  private micCapturing = false;
  /** 音画同步是否生效 */
  private syncOn = false;

  // 布局
  private hostEl: HTMLElement | null = null;
  private resizeObs: ResizeObserver | null = null;
  /** 画面的显示区域（设备像素）；还没挂到页面上时为 null */
  private viewport: { w: number; h: number } | null = null;
  /**
   * 最近告诉被控端的看哪几个显示器（view 命令的 streams，每一路带着按显示区域要的短边上限，见 updateView）；
   * 还没告诉过时为 null
   */
  private sentView: StreamView[] | null = null;
  private viewTimer: number | undefined;
  /** 最近一次想改显示区域要的大小的时刻（与 performance.now() 同一时基） */
  private viewAt = -Infinity;
  /** 最近告诉 Worker 的每一路画面的显示方式（见 sendLayout） */
  private sentLayouts: string[] = [];

  // 输入
  private interactive = false;
  private visible = true;
  private readonly input: RemoteInput;

  // 光标
  private cursorRaw: CursorImage | null = null;
  /** 被控端按槽位缓存的光标形状（动画光标反复用到的那几帧，见 media.ts），只在一个会话里有效 */
  private cursorSlots = new Map<number, CursorImage>();
  private cursorVisible = true;
  /** 被控端的光标已经画在画面里了（软件光标）：鼠标照常用绝对坐标，本地光标显不显示看设置（store 的 localCursor） */
  private cursorInFrame = false;
  /** 每个画面（主窗口的、弹出窗口的）现在的 CSS cursor */
  private cursorCss = new WeakMap<object, string>();

  /**
   * `saved`：我的设备里记着的这台电脑。带着上次的重连凭证连，对方不在线时等它上线（见 [`open`]），
   * 连上之前标签上就显示它的名字
   */
  constructor(
    code: string,
    settings: QualitySettings,
    saved?: Pick<SavedDevice, "resume" | "name" | "os" | "hostKey" | "mouse" | "swapCtrlCommand">,
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
    this.screen = document.createElement("div");
    this.screen.className = "remote-screen";
    this.screen.tabIndex = 0;
    const first = this.newCanvas(null);
    this.canvases.push(first);
    this.canvasHome.push(null);
    this.view = api.view(first, {
      size: (stream, w, h) => {
        this.frames[stream] = { w, h };
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
    this.input = this.newInput(this.screen, (x, y) => this.locate(x, y));
    this.swapCtrlCommand = saved?.swapCtrlCommand ?? false;
    // 这台电脑上次用的鼠标模式（不算换：不用告诉谁）。之后用户换了（菜单、快捷键）：告诉对方画不画光标、记下来
    this.input.setMode(saved?.mouse ?? "auto", false);
    this.audio.onClock = (hostUs, localMs, appliedMs) => this.view.audioClock(hostUs, localMs, appliedMs);
    this.audio.onUnsupported = () => {
      this.notice = { kind: "noOpus" };
      this.emit();
    };
    // 本机的麦克风没了（拔掉了）：关掉，再打开时重新找
    this.mic.onEnded = () => this.micFailed("noDevice");
  }

  /**
   * 一个画面（主窗口的，或者弹出窗口的，见 popout.ts）上的鼠标键盘。`locate`：指针在第几路画面上的哪里。
   * 几个画面的鼠标模式是同一个：在哪个画面上换了（快捷键），别的也跟着换
   */
  private newInput(screen: HTMLElement, locate: (x: number, y: number) => { x: number; y: number; stream: number }) {
    const input = new RemoteInput(
      screen,
      () => this.sid !== null && !this.disposed,
      (events) => {
        if (this.sid !== null) this.command({ cmd: "input", sid: this.sid, events });
      },
      locate,
    );
    // 鼠标切换了绝对 / 相对、锁定了 / 解除了：本机指针显示与否、工具栏的状态跟着变
    input.onModeChange = () => {
      this.applyCursor();
      this.emit();
    };
    input.shouldSwapCtrlCommand = () => this.hostPlatform === "mac" && this.swapCtrlCommand;
    input.onMouseMode = (mode) => {
      for (const other of this.inputs) if (other !== input && other.mode !== mode) other.setMode(mode, false);
      this.pushSettings();
      this.emit();
      this.onInputSettingsChange?.(this);
    };
    // 网页版：在画面上按粘贴键时，浏览器给的剪贴板里有这里复制的文件就先公告给对方（见 files.ts），粘贴键随后才发。
    // 桌面版的本机剪贴板由 Rust 端自己同步
    if (api.kind === "web") {
      input.onPaste = (e) => {
        const files = LocalFiles.fromDataTransfer(e.clipboardData);
        if (files && this.offerFiles(files)) e.preventDefault();
      };
    }
    return input;
  }

  /** 所有画面上的鼠标键盘：主窗口的和每个弹出窗口的 */
  private get inputs(): RemoteInput[] {
    return [this.input, ...[...this.popouts.values()].map((d) => d.input)];
  }

  // ───────────────────────── 订阅 ─────────────────────────

  subscribe = (fn: () => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };
  getVersion = () => this.version;

  private emit() {
    // 弹出窗口里的画面：连着的时候就能操作（主窗口的由 CanvasHost 管，见 setInteractive）
    const live = this.state === "live" && !this.disposed;
    for (const d of this.popouts.values()) if (d.input.interactive !== live) d.input.interactive = live;
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
    this.hostPlatform = null;
    // 光标的槽位是被控端为一个会话分配的：新的会话从头开始
    this.cursorSlots.clear();
    // 在连接之前清掉旧提示：被控端紧跟在 Hello 后面发的提示可能比 connect 的返回值先到
    this.notice = null;
    this.audioError = null;
    // 新的会话：对方在 hello 之后重新说它能不能接麦克风，打开了的话那时再告诉它
    this.micState = null;
    this.micDetail = null;
    this.micOn = false;
    // 防窥模式也一样：对方重新说。要不要在握手里就告诉它（下面的连接选项）
    this.privacyState = null;
    this.privacyDetail = null;
    this.attemptAt = performance.now();
    let info;
    // 开始时的画质、要不要声音、看哪几个显示器在端到端握手里就告诉对方：它一开始就按这个建编码器，
    // 不用先建一个默认的再重建（重连时知道对方有哪些显示器；第一次连时不知道，对方先看主显示器）
    const sent = this.effectiveSettings();
    const audio = this.audioEnabled && this.foreground;
    const view = this.wantedView();
    this.placeCanvases(view.map((v) => v.display));
    this.sentView = null;
    try {
      info = await api.connect(
        this.code,
        {
          resume: this.resume,
          hostKey: this.hostKey,
          pairing: this.pairing,
          settings: sent,
          audio,
          view,
          privacy: this.privacyWanted,
        },
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
    // 收到对方的 hello 才算开始；
    // hello 也可能比这里先到，那时已经是 live 了
    this.retries = 0;
    this.closedReason = null;
    // 连接过程中设置变了（例如 Hello 先到，换了编码格式，那时还没有 sid 发不出去）：重发
    if (!sameQuality(this.effectiveSettings(), sent)) this.pushSettings();
    // 看哪几个显示器：对方按握手里的开始（空的是主显示器）。Hello 先到了的话，现在想看的可能已经不一样了
    if (view.length) this.sentView = view;
    this.updateView(true);
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
    this.syncMic();
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
    clearTimeout(this.viewTimer);
    if (this.sid !== null) this.command({ cmd: "disconnect", sid: this.sid });
    this.sid = null;
    this.state = "closed";
    this.audio.disable();
    this.syncMic();
    this.detach();
    for (const name of [...this.popouts.keys()]) this.dropPopout(name);
    for (const p of this.opening.values()) p.close();
    this.opening.clear();
    this.emit();
  }

  // ───────────────────────── 事件 ─────────────────────────

  private onEvent(ev: SessionEvent) {
    switch (ev.type) {
      case "hello":
        this.state = "live";
        this.hostName = ev.hostname;
        this.hostOs = ev.os;
        this.hostPlatform = ev.platform;
        // 手机点的是触屏，没有鼠标模式（工具栏也不显示）：一律按自动
        if ((ev.platform === "android" || ev.platform === "ios") && this.input.mode !== "auto") {
          for (const input of this.inputs) input.setMode("auto", false);
          this.pushSettings();
        }
        this.hostActions = ev.actions;
        // iPhone / iPad 只能看（iOS 不允许别的应用注入触摸）：说一声，免得以为是出了问题
        if (ev.platform === "ios") this.notice = { kind: "iosViewOnly" };
        this.hostVersion = ev.version;
        this.maxStreams = Math.max(1, ev.maxStreams ?? 1);
        this.setShown(ev.displays, ev.current, ev.shown);
        this.hostCodecs = ev.codecs;
        this.fitCodec();
        this.layout();
        this.updateView();
        break;
      case "displays":
        // 对方补上了显示器（虚拟显示器）、插回了显示器：撤下"没有显示器"
        if (ev.displays.length > 0 && this.notice?.kind === "noDisplays") this.notice = null;
        this.setShown(ev.displays, ev.current, ev.shown);
        this.layout();
        // 同时看所有显示器时：对方插上了新的显示器也要看
        this.updateView();
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
        // 工具栏、画质面板显示的是第 0 路画面的编码器（各路的画质设置一样）
        if ((ev.stream ?? 0) !== 0) return;
        this.encoder = { codec: ev.codec, name: ev.name, hardware: ev.hardware, width: ev.width, height: ev.height };
        break;
      case "mic":
        this.micState = ev.state;
        this.micDetail = ev.detail;
        this.syncMic();
        break;
      case "privacy":
        this.privacyState = ev.state;
        this.privacyDetail = ev.detail;
        // 开关跟着对方说的变：别的控制端打开 / 关掉了、没能打开，都以对方的为准
        this.privacyWanted = ev.state === "on" || ev.state === "starting";
        break;
      case "clipboardFiles":
        this.remoteFiles = { clip: ev.clip, items: ev.items };
        break;
      case "fileError":
        this.notice = { kind: "fileError", name: ev.name, error: ev.error };
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
        for (const input of this.inputs) input.reset();
        if (ev.retry) {
          this.scheduleRetry(ev.reason);
        } else {
          // 对方主动断开：不自动重连
          this.sid = null;
          this.syncAudio();
          this.syncMic();
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
    this.cursorInFrame = c.inFrame;
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
    for (const input of this.inputs) input.setRemoteCursorHidden(!c.visible);
    this.applyCursor();
  }

  /** 按每个画面（主窗口的、弹出窗口的）的显示比例缩放远端光标，生成它们的 CSS cursor */
  private applyCursor() {
    this.setCursor(this.screen, this.cursorFor(this.input, () => this.streamScale(this.mainStreams[0] ?? 0)));
    for (const d of this.popouts.values()) {
      const i = this.canvasHome.indexOf(d.popout);
      this.setCursor(d.popout.screen, this.cursorFor(d.input, () => this.streamScale(Math.max(0, i))));
    }
  }

  /** 一个画面上的本机指针：`input` 是这个画面上的鼠标，`scaleOf` 量出对方的一个物理像素在这里是几个 CSS 像素 */
  private cursorFor(input: RemoteInput, scaleOf: () => number): string {
    // 游戏模式：光标在画面里（对方画的），本机指针锁定着不显示；没锁定时显示一个普通的指针，好去点画面、点工具栏
    if (input.mode === "game") return input.relative ? "none" : "default";
    // 发相对移动时本机指针的位置没有意义，不显示；用户按 Esc 解除了锁定、桌面模式下对方隐藏了光标，就显示一个普通的指针
    if (!this.cursorVisible || input.relative) return input.relative ? "none" : "default";
    // 画面里已经有光标了（例如对方正在拖动、缩放窗口）：默认照样显示本机的（跟手）；设置里关掉了就只看画面里的，不会有两个
    if (this.cursorInFrame && !store.get().localCursor) return "none";
    const c = this.cursorRaw;
    if (!c) return "default";
    // 远端物理像素 → 本地 CSS 像素；限制在 Chromium 支持的 128px 以内
    const scale = Math.min(Math.max(scaleOf(), 0.35), 2, 128 / Math.max(c.w, c.h));
    // 这个形状按差不多的比例生成过（动画光标循环回来、几个窗口里差不多大）：直接用，不再生成一次 PNG
    const made = c.made?.find((m) => Math.abs(scale - m.scale) < 0.02);
    if (made) return made.css;
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
    const css = `url(${out.toDataURL("image/png")}) ${hx} ${hy}, default`;
    c.made = [{ scale, css }, ...(c.made ?? [])].slice(0, MADE_CURSORS);
    return css;
  }

  /**
   * 第 i 路画面上，对方的一个物理像素显示成几个 CSS 像素。主窗口里的几路是按同一个比例缩放的（见 screens.ts）：
   * 按主窗口里的第一路算就行
   */
  private streamScale(i: number): number {
    const display = this.displays[this.shown[i]] ?? this.displays[this.currentDisplay];
    const frame = this.frames[i] ?? this.frames[0];
    const srcWidth = display?.width || frame.w;
    const cssWidth = this.canvases[i]?.getBoundingClientRect().width || frame.w;
    return cssWidth / srcWidth;
  }

  private setCursor(screen: HTMLElement, css: string) {
    if (this.cursorCss.get(screen) === css) return;
    this.cursorCss.set(screen, css);
    screen.style.cursor = css;
  }

  // ───────────────────────── 布局 ─────────────────────────

  attach(host: HTMLElement) {
    this.detach();
    this.hostEl = host;
    host.appendChild(this.screen);
    this.resizeObs = new ResizeObserver(() => this.layout());
    this.resizeObs.observe(host);
    this.layout();
  }

  detach() {
    this.resizeObs?.disconnect();
    this.resizeObs = null;
    if (this.hostEl && this.screen.parentElement === this.hostEl) this.hostEl.removeChild(this.screen);
    this.hostEl = null;
  }

  setScaleMode(m: ScaleMode) {
    this.scaleMode = m;
    this.layout();
    this.emit();
  }

  /** 画面增强的设置（store 的 enhance）变了 */
  refreshLayout() {
    this.layout();
  }

  /** 本地光标的设置（store 的 localCursor）变了 */
  refreshCursor() {
    this.applyCursor();
  }

  /** 画面增强的强度：全局设置，只用在正在操作的会话上（多屏总览的缩略图不增强） */
  private get enhanceLevel(): EnhanceLevel {
    return this.interactive ? store.get().enhance : "off";
  }

  /**
   * 摆好每一路画面。主窗口：只有一路时和以前一样，画面在显示区域里 1:1 或等比缩放；好几路时按对方的显示器在桌面上的位置
   * 拼在一起、整体缩放到显示区域里（screens.ts），每一路在自己那一块里再 1:1 或等比缩放。
   * 放在弹出窗口里的各自在自己的窗口里摆（layoutPopout）
   */
  private layout() {
    const main = this.layoutMain();
    for (const d of this.popouts.values()) this.layoutPopout(d);
    if (!main && !this.popouts.size) return;
    this.applyCursor();
    this.updateView();
  }

  /** 摆好主窗口里的那几路画面。画面不在页面上（例如切到了别的标签）时什么也不做，返回 false */
  private layoutMain(): boolean {
    const host = this.hostEl;
    if (!host) return false;
    const dpr = devicePixelRatio || 1;
    const r = host.getBoundingClientRect();
    if (r.width > 0 && r.height > 0) this.viewport = { w: Math.floor(r.width * dpr), h: Math.floor(r.height * dpr) };
    // 画面的显示区域（设备像素；原始大小显示时不按窗口缩放）
    const area = this.nativeScale ? null : this.viewport;
    const enhance = this.enhanceLevel;
    // 在主窗口里的是第几路；每一路占的那一块（设备像素，相对整个画面的左上角）、整个画面的大小，以及要不要对齐到设备像素
    const main = this.mainStreams;
    let tiles: Rect[] = [];
    let box = { w: 0, h: 0 };
    // 好几路时每一块都在整数设备像素上：整个画面对齐了，1:1 显示的那几路就不会发虚
    let snap = true;
    if (main.length === 1) {
      const m = main[0];
      const { w: fw, h: fh } = this.frameSize(m);
      const size = area ? fitSize(fw, fh, area.w, area.h) : { w: fw, h: fh, exact: true };
      tiles = [{ x: 0, y: 0, w: size.w, h: size.h }];
      box = size;
      // 1:1 显示，或者画面增强时 canvas 就是显示的大小（Worker 按同样的算法放大好了）：不能再让浏览器插值
      snap = size.exact || renderPlan(fw, fh, area, enhance).enhance;
      this.sendLayout(m, area, enhance);
    } else if (main.length > 1) {
      const placed = layoutTiles(placeDisplays(main.map((i) => this.displays[this.shown[i]])), area);
      tiles = placed.tiles;
      box = placed;
    }
    const ss = this.screen.style;
    ss.width = `${box.w / dpr}px`;
    ss.height = `${box.h / dpr}px`;
    this.canvases.forEach((c, i) => {
      // 在弹出窗口里：在那边摆
      if (this.canvasHome[i]) return;
      const st = c.style;
      const k = main.indexOf(i);
      const tile = tiles[k];
      if (k < 0 || !tile) {
        st.display = "none";
        return;
      }
      st.display = "";
      let size = { w: tile.w, h: tile.h };
      if (main.length > 1) {
        // 这一路的画面还没到：先按它的显示器的宽高比
        const { w: fw, h: fh } = this.frameSize(i);
        size = fitSize(fw, fh, tile.w, tile.h);
        this.sendLayout(i, area ? { w: tile.w, h: tile.h } : null, enhance);
      }
      st.left = `${(tile.x + Math.floor((tile.w - size.w) / 2)) / dpr}px`;
      st.top = `${(tile.y + Math.floor((tile.h - size.h) / 2)) / dpr}px`;
      st.width = `${size.w / dpr}px`;
      st.height = `${size.h / dpr}px`;
    });
    ss.left = ss.top = "";
    if (snap && !this.nativeScale) {
      // 1:1 显示时左上角要落在整数设备像素上：差半个像素浏览器也会插值，文字又糊了
      const c = this.screen.getBoundingClientRect();
      const align = (v: number) => (Math.round(v * dpr) - v * dpr) / dpr;
      ss.left = `${align(c.left)}px`;
      ss.top = `${align(c.top)}px`;
    }
    return true;
  }

  /**
   * 摆好一个弹出窗口里的画面：和主窗口只有一路时一样，在窗口里 1:1 或等比缩放（弹出窗口总是按窗口大小显示）。
   * 窗口可能在另一个显示器上，设备像素比例按它的算
   */
  private layoutPopout(d: Detached) {
    const p = d.popout;
    const i = this.canvasHome.indexOf(p);
    const c = this.canvases[i];
    const area = p.viewport;
    if (!c || !area) return;
    const dpr = p.dpr;
    const enhance = store.get().enhance;
    const display = this.displays.find((x) => x.name === d.name);
    const { w: fw, h: fh } = this.frames[i] ?? (display ? { w: display.width, h: display.height } : this.frameSize(i));
    const size = fitSize(fw, fh, area.w, area.h);
    this.sendLayout(i, area, enhance);
    const ss = p.screen.style;
    ss.width = `${size.w / dpr}px`;
    ss.height = `${size.h / dpr}px`;
    const st = c.style;
    st.display = "";
    st.left = st.top = "0px";
    st.width = ss.width;
    st.height = ss.height;
    ss.left = ss.top = "";
    if (size.exact || renderPlan(fw, fh, area, enhance).enhance) {
      const r = p.screen.getBoundingClientRect();
      const align = (v: number) => (Math.round(v * dpr) - v * dpr) / dpr;
      ss.left = `${align(r.left)}px`;
      ss.top = `${align(r.top)}px`;
    }
  }

  /** 第 i 路画面的大小（像素）：画面还没到时按它的显示器（还不知道是哪个时是 16:9） */
  private frameSize(i: number): { w: number; h: number } {
    const f = this.frames[i];
    if (f) return f;
    const d = this.displays[this.shown[i]];
    return d ? { w: d.width, h: d.height } : { w: 16, h: 9 };
  }

  /** 告诉 Worker 第 stream 路画面怎么显示（变了才发）。不增强时显示区域用不上，不发：拖动窗口边框时不用每一下都重画 */
  private sendLayout(stream: number, area: Area | null, enhance: EnhanceLevel) {
    const shown = enhance === "off" ? null : area;
    const key = `${shown ? `${shown.w}x${shown.h}` : "-"}/${enhance}`;
    if (key === this.sentLayouts[stream]) return;
    this.sentLayouts[stream] = key;
    this.view.setLayout(stream, shown, enhance);
  }

  /** 原始大小（可滚动）显示 */
  private get nativeScale() {
    return this.scaleMode === "native" && this.interactive;
  }

  /** 现在显示几路画面：对方说的（显示器的序号都对得上、都有 canvas 时），至少一路 */
  private get streamCount(): number {
    const n = this.shown.length;
    const ok = n <= this.canvases.length && this.shown.every((i) => i < this.displays.length);
    return n > 1 && ok ? n : 1;
  }

  /** 对方说了现在的显示器列表和每一路画面是哪个显示器（对方没说的时候只有第 0 路） */
  private setShown(displays: DisplayInfo[], current: number, shown: number[] | undefined) {
    this.displays = displays;
    this.currentDisplay = current;
    this.shown = shown?.length ? shown : [current];
    // 第一次：只看一个显示器时就看对方开始时给的那个（主显示器）
    if (this.wantName === null) this.wantName = displays[current]?.name ?? null;
    // 弹出去的显示器没了（对方拔掉了）、对方不能同时发好几路了：关掉它的窗口
    for (const name of [...this.popouts.keys()]) {
      if (this.maxStreams < 2 || (displays.length > 0 && !displays.some((d) => d.name === name))) this.dropPopout(name);
    }
    this.placeCanvases(this.shown);
  }

  /** 一路画面的新 canvas，放进 `home`（null = 主窗口）的画面里，还没交给 Worker */
  private newCanvas(home: Popout | null): HTMLCanvasElement {
    const c = (home?.doc ?? document).createElement("canvas");
    c.className = "remote-canvas";
    c.width = 16;
    c.height = 9;
    (home?.screen ?? this.screen).appendChild(c);
    return c;
  }

  /**
   * 每一路画面的 canvas 放进它的显示器所在的窗口（`displays` 的第 i 项是第 i 路要显示的显示器），要在对方发这一路之前做好：
   * 还没有 canvas 的路新建；显示器换了窗口（弹出去了、放回来了）的路换一个新的 canvas 交给 Worker
   * （交出去的 canvas 不能挪到别的窗口里，Worker 换上新的解码器，自己要关键帧）。
   * 主窗口里至少留一路：都在弹出窗口里了（例如主窗口看的那个显示器拔掉了）就把第 0 路的放回来
   */
  private placeCanvases(displays: number[]) {
    const n = Math.min(displays.length, MAX_STREAMS);
    if (n > 0 && displays.slice(0, n).every((d) => this.homeOf(d))) this.dropPopout(this.displays[displays[0]].name);
    for (let i = 0; i < n; i++) {
      const home = this.homeOf(displays[i]);
      if (this.canvases[i] && this.canvasHome[i] === home) continue;
      this.canvases[i]?.remove();
      const c = this.newCanvas(home);
      this.canvases[i] = c;
      this.canvasHome[i] = home;
      // Worker 里是新的解码器：怎么显示要重新告诉它
      this.sentLayouts[i] = "";
      this.view.addCanvas(i, c);
    }
  }

  /** 第 display 个显示器在哪个窗口里：弹出去了的是它的弹出窗口，否则是主窗口（null） */
  private homeOf(display: number): Popout | null {
    const name = this.displays[display]?.name;
    return (name !== undefined && this.popouts.get(name)?.popout) || null;
  }

  /** 在主窗口里显示的是第几路（别的在弹出窗口里） */
  private get mainStreams(): number[] {
    const out: number[] = [];
    for (let i = 0; i < this.streamCount; i++) if (!this.canvasHome[i]) out.push(i);
    return out;
  }

  /** 指针（屏幕坐标）在主窗口里的第几路画面上的哪里：落在缝里、外面时是最近的那一路的边上 */
  private locate(x: number, y: number): { x: number; y: number; stream: number } {
    const main = this.mainStreams;
    const hit = hitTest(main.map((i) => this.canvases[i].getBoundingClientRect()), x, y);
    return hit ? { x: hit.x, y: hit.y, stream: main[hit.index] } : { x: 0, y: 0, stream: main[0] ?? 0 };
  }

  /** 指针（弹出窗口里的坐标）在那个窗口的画面上的哪里 */
  private locatePopout(popout: Popout, x: number, y: number): { x: number; y: number; stream: number } {
    const i = this.canvasHome.indexOf(popout);
    const c = this.canvases[i];
    const hit = c ? hitTest([c.getBoundingClientRect()], x, y) : null;
    return hit ? { x: hit.x, y: hit.y, stream: i } : { x: 0, y: 0, stream: Math.max(0, i) };
  }

  /**
   * 想看的显示器（第 i 项是第 i 路）：只看一个时是选的那个，再加上弹出去的；都看时它在第 0 路（切换时这一路不用重建），
   * 接着是别的。对方的路数不够时先保证选的那个和弹出去的
   */
  private wanted(): number[] {
    const n = this.displays.length;
    if (!n) return [];
    let first = this.displays.findIndex((d) => d.name === this.wantName);
    if (first < 0) first = this.currentDisplay < n ? this.currentDisplay : 0;
    const limit = Math.min(this.maxStreams, MAX_STREAMS);
    if (limit < 2) return [first];
    const popped = (i: number) => this.popouts.has(this.displays[i].name);
    const rest = this.displays.map((_, i) => i).filter((i) => i !== first && (this.allDisplays || popped(i)));
    const order = [first, ...rest];
    if (order.length <= limit) return order;
    const keep = new Set(order.filter((i) => i === first || popped(i)).slice(0, limit));
    for (const i of order) if (keep.size < limit) keep.add(i);
    return order.filter((i) => keep.has(i));
  }

  /**
   * 想看的显示器，带着每一路按它在显示区域里的大小要的短边上限（见 fit.ts；不用缩小、原始大小显示、
   * 还不知道显示区域时是 0：不限）。主窗口里的按主窗口算，弹出去的按它的窗口算。对方和画质设置里的上限取较严的
   */
  private wantedView(): StreamView[] {
    const ds = this.wanted();
    const limits = ds.map(() => 0);
    const vp = this.viewport;
    // 在主窗口里的是第几项
    const main = ds.map((_, k) => k).filter((k) => !this.homeOf(ds[k]));
    const infos = main.map((k) => this.displays[ds[k]]);
    if (vp && !this.nativeScale && infos.length === 1) {
      limits[main[0]] = viewMaxShort(infos[0].width, infos[0].height, vp.w, vp.h);
    } else if (vp && !this.nativeScale && infos.length > 1) {
      const { tiles } = layoutTiles(placeDisplays(infos), vp);
      infos.forEach((d, j) => (limits[main[j]] = viewMaxShort(d.width, d.height, tiles[j].w, tiles[j].h)));
    }
    ds.forEach((display, k) => {
      const area = this.homeOf(display)?.viewport;
      const d = this.displays[display];
      if (area) limits[k] = viewMaxShort(d.width, d.height, area.w, area.h);
    });
    return ds.map((display, k) => ({ display, maxShort: limits[k] }));
  }

  /**
   * 告诉对方看哪几个显示器、每一路要多大的画面（变了才发）。换了显示器马上发；只是显示区域变了时，
   * 一段时间没变过就马上发，连续变化时（拖动窗口边框、窗口动画）等停下来再发，免得对方一直重建编码器。
   * `now`：不等（用户刚切换了、刚连上）
   */
  private updateView(now = false) {
    if (this.sid === null || this.disposed) return;
    const want = this.wantedView();
    if (!want.length) return;
    clearTimeout(this.viewTimer);
    this.viewTimer = undefined;
    const sent = this.sentView;
    if (sent && sameView(sent, want)) return;
    const apply = () => {
      this.viewTimer = undefined;
      if (this.sid === null) return;
      // 对方开始发这几路之前，Worker 里就要有画它们的 canvas（放在各自的窗口里）
      this.placeCanvases(want.map((v) => v.display));
      this.sentView = want;
      this.command({ cmd: "view", sid: this.sid, streams: want });
    };
    const moved = !sent || sent.length !== want.length || sent.some((s, i) => s.display !== want[i].display);
    const t = performance.now();
    const quiet = t - this.viewAt >= VIEW_LIMIT_DELAY_MS;
    this.viewAt = t;
    if (now || moved || quiet) apply();
    else this.viewTimer = window.setTimeout(apply, VIEW_LIMIT_DELAY_MS);
  }

  // ───────────────────────── 可见性 / 设置 ─────────────────────────

  /** 不可见（例如切到其它会话）时降帧率，节省带宽 */
  setVisible(v: boolean) {
    if (this.visible === v) return;
    this.visible = v;
    this.pushSettings();
    if (v) this.view.requestKeyframe();
  }

  /** 是否为当前正在查看的会话（只有它播放声音、只有它收本机的麦克风） */
  setForeground(v: boolean) {
    if (this.foreground === v) return;
    this.foreground = v;
    this.syncAudio();
    this.syncMic();
  }

  /** 把本机麦克风发给对方（第一次打开时浏览器会问能不能用麦克风） */
  setMicEnabled(v: boolean) {
    this.micEnabled = v;
    if (v) this.micError = null;
    this.syncMic();
    this.emit();
  }

  /**
   * 打开 / 关闭对方的防窥模式。先显示成正在打开 / 已关闭，对方换好了再说（换成虚拟显示器要几秒）。
   * 还没连上时只记下来，连上时在连接选项里告诉对方
   */
  setPrivacy(on: boolean) {
    this.privacyWanted = on;
    if (this.sid !== null && this.privacyState !== null) {
      this.command({ cmd: "privacy", sid: this.sid, on });
      this.privacyState = on ? "starting" : "off";
      this.privacyDetail = null;
    }
    this.emit();
  }

  /** 正在把本机麦克风发给对方 */
  get micLive(): boolean {
    return this.micCapturing && this.micState === "ready";
  }

  /**
   * 麦克风：开关打开、正在查看这个会话、对方能接（不是 unsupported）时告诉对方打开（对方没有设备、别人占着的时候
   * 它会一直等着，能用了自动接上）；对方说 ready 时才采集本机的麦克风。不满足了就停止采集、告诉对方关掉（放开设备）
   */
  private syncMic() {
    const want =
      this.micEnabled && this.foreground && this.sid !== null && !this.disposed
      && this.micState !== null && this.micState !== "unsupported";
    if (want !== this.micOn && this.sid !== null) this.command({ cmd: "mic", sid: this.sid, enabled: want });
    this.micOn = want;
    const capture = want && this.micState === "ready";
    if (capture === this.micCapturing) return;
    this.micCapturing = capture;
    if (!capture) {
      this.mic.stop();
      return;
    }
    this.mic.start().catch((e: unknown) => {
      if (this.micCapturing) this.micFailed(typeof e === "string" ? (e as MicError) : "failed");
    });
  }

  /** 本机采集不了麦克风：关掉开关（原因显示在麦克风的菜单里），用户再打开时重试 */
  private micFailed(e: MicError) {
    this.micCapturing = false;
    this.mic.stop();
    this.micError = e;
    this.micEnabled = false;
    this.syncMic();
    this.emit();
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

  /**
   * 实际发给被控端的画质：不可见时降帧率（有弹出窗口时不算不可见：那里还看着）。分辨率不超过画面的显示区域另外按每一路说（view 命令，见 updateView），
   * 对方和这里的短边上限取较严的
   */
  private effectiveSettings(): QualitySettings {
    const s = { ...this.settings };
    // 游戏模式：让对方把光标画进画面里（本机指针锁定着、不显示，看到的就是对方真实的光标）。它跟着鼠标模式走，
    // 不算画质：保存的画质里就算带着也不用
    delete s.drawCursor;
    if (this.input.mode === "game") s.drawCursor = true;
    if (!this.visible && !this.popouts.size) s.fps = Math.min(s.fps, BACKGROUND_FPS);
    return s;
  }

  private pushSettings() {
    if (this.sid === null) return;
    this.command({ cmd: "settings", sid: this.sid, settings: this.effectiveSettings() });
  }

  /** 只看对方的第 index 个显示器（它在单独的窗口里的话先放回来） */
  selectDisplay(index: number) {
    const d = this.displays[index];
    if (d && this.isPopped(index)) this.dockDisplay(index);
    if (!d || (!this.allDisplays && this.wantName === d.name && this.currentDisplay === index)) return;
    this.releaseAll();
    this.allDisplays = false;
    this.wantName = d.name;
    this.updateView(true);
    this.emit();
  }

  /** 能同时看对方所有的显示器：对方有不止一个显示器，也能同时发好几路画面 */
  get canShowAll(): boolean {
    return this.displays.length > 1 && this.maxStreams > 1;
  }

  /** 同时看对方所有的显示器（每个显示器一路画面，按它们在对方桌面上的位置拼在一起）/ 只看一个 */
  setAllDisplays(on: boolean) {
    if (this.allDisplays === on) return;
    this.releaseAll();
    this.allDisplays = on;
    this.updateView(true);
    this.emit();
  }

  // ───────────────────────── 弹出窗口（见 popout.ts） ─────────────────────────

  /** 放在单独窗口里的显示器（第几个）和它们的窗口：界面在这些窗口里画工具栏（PopoutView） */
  get popoutWindows(): { display: number; popout: Popout; gamePaused: boolean }[] {
    const out: { display: number; popout: Popout; gamePaused: boolean }[] = [];
    for (const d of this.popouts.values()) {
      const display = this.displays.findIndex((x) => x.name === d.name);
      if (display >= 0) out.push({ display, popout: d.popout, gamePaused: d.input.gamePaused });
    }
    return out;
  }

  /** 第 index 个显示器放在单独的窗口里了（或者正在打开） */
  isPopped(index: number): boolean {
    const name = this.displays[index]?.name;
    return name !== undefined && (this.popouts.has(name) || this.opening.has(name));
  }

  /**
   * 能把第 index 个显示器放到单独的窗口里：连着、对方能同时发好几路画面；正在看着的（同时看所有的显示器时）
   * 放出去之后主窗口里还要留下至少一个，没在看的要对方还能多发一路
   */
  canPopOut(index: number): boolean {
    if (!this.displays[index] || this.state !== "live" || this.maxStreams < 2 || this.isPopped(index)) return false;
    const want = this.wanted();
    if (want.includes(index)) return want.filter((i) => !this.isPopped(i)).length > 1;
    const pending = [...this.opening.keys()].filter((name) => !want.some((i) => this.displays[i].name === name));
    return want.length + pending.length < Math.min(this.maxStreams, MAX_STREAMS);
  }

  /**
   * 把第 index 个显示器放到单独的窗口里（要在点击里调用：浏览器只在用户操作里让开新窗口）。
   * 窗口准备好了才把画面换过去；打不开（例如浏览器拦住了）时返回 false
   */
  popOut(index: number): boolean {
    if (!this.canPopOut(index)) return false;
    const name = this.displays[index].name;
    const popout = Popout.open(this.hostName || this.code);
    if (!popout) return false;
    this.opening.set(name, popout);
    popout.onClosed = () => {
      if (this.opening.get(name) === popout) this.opening.delete(name);
      this.emit();
    };
    popout.onReady = () => {
      this.opening.delete(name);
      if (this.disposed || this.popouts.has(name) || !this.displays.some((d) => d.name === name)) popout.close();
      else this.attachPopout(name, popout);
    };
    this.emit();
    return true;
  }

  /** 弹出窗口准备好了：这个显示器的画面换过去，挂上鼠标键盘 */
  private attachPopout(name: string, popout: Popout) {
    const input = this.newInput(popout.screen, (x, y) => this.locatePopout(popout, x, y));
    input.setMode(this.input.mode, false);
    input.setRemoteCursorHidden(!this.cursorVisible);
    const d: Detached = { name, popout, input };
    this.popouts.set(name, d);
    // 用户关掉了窗口：放回主窗口
    popout.onClosed = () => {
      if (this.popouts.get(name) !== d) return;
      this.dropPopout(name);
      this.popoutsChanged();
    };
    popout.onResize = () => this.layout();
    popout.onChange = () => this.emit();
    // 主窗口里按着的键、鼠标键先放开
    this.releaseAll();
    this.popoutsChanged();
    popout.focus();
  }

  /** 关掉 name 的弹出窗口（不重新摆画面，调用的地方接着摆）。在那边按着的键、鼠标键让对方都放开 */
  private dropPopout(name: string) {
    const d = this.popouts.get(name);
    if (!d) return;
    this.popouts.delete(name);
    d.input.dispose();
    d.popout.onClosed = null;
    d.popout.close();
    this.releaseAll();
  }

  /** 弹出 / 放回了显示器：重新摆好画面，马上告诉对方看哪几个、每一路要多大 */
  private popoutsChanged() {
    this.placeCanvases(this.wanted());
    this.layout();
    this.updateView(true);
    // 不在前台时：有没有弹出窗口决定降不降帧率
    if (!this.visible) this.pushSettings();
    this.emit();
  }

  /** 把第 index 个显示器放回主窗口（关掉它的窗口） */
  dockDisplay(index: number) {
    const name = this.displays[index]?.name;
    if (name === undefined) return;
    const opening = this.opening.get(name);
    if (opening) {
      this.opening.delete(name);
      opening.close();
    }
    if (!this.popouts.has(name)) return this.emit();
    this.dropPopout(name);
    this.popoutsChanged();
  }

  /** 切到第 index 个显示器的弹出窗口 */
  focusPopout(index: number) {
    const name = this.displays[index]?.name;
    if (name !== undefined) this.popouts.get(name)?.popout.focus();
  }

  setClipboardSync(v: boolean) {
    this.clipboardSync = v;
    if (this.sid !== null) this.command({ cmd: "clipboardSync", sid: this.sid, enabled: v });
    this.emit();
  }

  // ───────────────────────── 剪贴板里的文件（见 files.ts） ─────────────────────────

  /**
   * 这里复制的文件（粘贴、拖到画面上的）：公告给对方，它粘贴时才来取。剪贴板同步关着、还没连上时不发。
   * 返回公告了没有
   */
  offerFiles(files: LocalFiles): boolean {
    if (this.sid === null || !this.clipboardSync) return false;
    api.offerFiles(this.sid, files).catch((e) => {
      // 对方的版本旧，不支持剪贴板里的文件
      this.notice = { kind: "filesUnsupported" };
      console.warn("can't offer the files", e);
      this.emit();
    });
    return true;
  }

  /** 把对方复制的文件（remoteFiles）取下来 */
  async downloadRemoteFiles() {
    const offer = this.remoteFiles;
    if (!offer || this.sid === null || this.download) return;
    this.download = { done: 0, total: 0 };
    this.emit();
    let last = 0;
    try {
      const saved = await downloadFiles(api, this.sid, offer.clip, offer.items, (done, total) => {
        this.download = { done, total };
        // 进度条不用每一块都刷新
        const now = performance.now();
        if (now - last > 200 || done === total) {
          last = now;
          this.emit();
        }
      });
      if (saved && this.remoteFiles === offer) this.remoteFiles = null;
    } catch (e) {
      const error = typeof e === "string" ? (e as FileError) : "io";
      const name = offer.items.length === 1 ? offer.items[0].path[0] : `${offer.items.length}`;
      this.notice = { kind: "fileError", name, error };
    } finally {
      this.download = null;
      this.emit();
    }
  }

  /** 不要对方复制的文件了（关掉下载的提示） */
  dismissRemoteFiles() {
    this.remoteFiles = null;
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

  setSwapCtrlCommand(enabled: boolean) {
    if (this.hostPlatform !== "mac" || this.swapCtrlCommand === enabled) return;
    this.swapCtrlCommand = enabled;
    this.onInputSettingsChange?.(this);
    this.emit();
  }

  /** 鼠标模式（见 input.ts） */
  get mouseMode(): MouseMode { return this.input.mode; }
  /** 本机指针锁定在画面上了 */
  get pointerLocked(): boolean { return this.input.locked; }
  /** 游戏模式下指针没锁定：鼠标不发给对方，点一下画面重新锁定（界面上提示一下） */
  get gamePaused(): boolean { return this.input.gamePaused; }

  /** 换鼠标模式（工具栏的鼠标菜单；快捷键 Ctrl+Alt+Shift+M 由 input.ts 处理）。要在点击里调用：游戏模式马上锁定 */
  setMouseMode(mode: MouseMode) {
    this.input.setMode(mode);
    this.focus();
  }

  focus() { this.input.focus(); }
  private releaseAll() { this.input.releaseAll(); }
  private enqueue(events: InputEvent[], urgent: boolean) { this.input.enqueue(events, urgent); }
}

/** 放在单独窗口里的一个显示器（见 popout.ts） */
interface Detached {
  /** 显示器的名字 */
  name: string;
  popout: Popout;
  /** 那个窗口里的画面上的鼠标键盘 */
  input: RemoteInput;
}

/** 光标的一个形状（RGBA，非预乘） */
interface CursorImage {
  w: number;
  h: number;
  hx: number;
  hy: number;
  rgba: Uint8ClampedArray;
  /** 按几种比例缩放、生成好的 CSS cursor（最近用的在前面） */
  made?: { scale: number; css: string }[];
}

function sameView(a: StreamView[], b: StreamView[]): boolean {
  return a.length === b.length && a.every((v, i) => v.display === b[i].display && v.maxShort === b[i].maxShort);
}

function sameQuality(a: QualitySettings, b: QualitySettings): boolean {
  return a.fps === b.fps && a.bitrateKbps === b.bitrateKbps && a.maxShort === b.maxShort
    && a.adaptive === b.adaptive && a.codec === b.codec
    && !!a.drawCursor === !!b.drawCursor;
}
