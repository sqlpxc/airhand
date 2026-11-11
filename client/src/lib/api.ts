// 与 Rust 端的命令 / 数据结构一一对应。
//
// 控制台有两种运行方式，界面完全相同（见 Backend）。会话都走同一种 WebSocket（console.ts，协议见
// crates/airhand-link/src/console.rs），只是连的地方不同：
// * 桌面版：Tauri 窗口，连本程序只监听 127.0.0.1 的服务；窗口、我的设备这些走 invoke（backend/tauri.ts）；
// * 网页版：浏览器打开 airhand-web（web/）的端口，连它的 `/ws`（backend/web.ts）。
//
// 对会话的操作是 Command（与 Rust 的 airhand_link::controller::Command 相同，Android 也一样），
// 画面 / 光标 / 声音是媒体包（见 media.ts）。样例见 crates/airhand-link/tests/protocol.json。
//
// 和 Rust 之间的 JSON 全都是 camelCase：字段名、枚举值（"h264"、"android"、"quickSettings"），
// 带字段的联合类型用一个字段标明是哪一种（命令 cmd、事件和输入 type、问题和提示 kind）。
import { isTauri } from "@tauri-apps/api/core";
import { tauriBackend } from "./backend/tauri";
import { webBackend } from "./backend/web";
import type { MediaView, ViewOutput } from "./console";
import type { Lang } from "./i18n";

export type Codec = "h264" | "h265" | "av1";

/** 与 airhand_proto::session::QualitySettings 对应 */
export interface QualitySettings {
  fps: number;
  bitrateKbps: number;
  /** 输出画面短边上限，0 = 原始分辨率 */
  maxShort: number;
  adaptive: boolean;
  codec: Codec;
}

export interface DisplayInfo {
  index: number;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
}

export type ServerState =
  | { state: "connecting" }
  | { state: "connected" }
  | { state: "disconnected"; reason: Problem };

/**
 * 显示给用户的问题，与 Rust 端 airhand_link::Problem 一一对应（JSON 以 kind 区分）。
 *
 * Rust 端只传类型，界面显示时才按当前语言变成文字（见 messages.ts 的 problemText），
 * 所以不会出现别的电脑或服务器的语言，切换语言时已显示的问题也会跟着变。
 * `detail` 是技术信息（英文），原样显示。
 */
export type Problem =
  // 与中转服务器的连接
  | { kind: "serverUnreachable"; detail: string }
  | { kind: "certMismatch" }
  | { kind: "incompatible"; server: number; client: number }
  | { kind: "serverLost"; detail: string }
  // 连接被控端
  | { kind: "notConnected" }
  | { kind: "invalidCode" }
  | { kind: "codeNotFound" }
  | { kind: "rateLimited" }
  | { kind: "hostBusy" }
  | { kind: "serverTimeout" }
  /** 重连时对方不在线（服务器确认过是之前连上的那台）：对方重新上线时会收到 onHostOnline */
  | { kind: "hostOffline" }
  // 端到端加密（见 Rust 的 airhand_proto::handshake）
  /** 对方的身份（公钥）和记下的不一样：不自动重连，用户确认是对方重装之后输入配对码重新配对 */
  | { kind: "hostKeyChanged" }
  /** 对方要求配对码（第一次连这台电脑） */
  | { kind: "pairingRequired" }
  | { kind: "pairingFailed" }
  /** 对方暂停了配对（有人多次输错配对码） */
  | { kind: "pairingUnavailable" }
  /** 与对方的会话版本不兼容：peer 比 ours 大要更新本机，否则对方更新 */
  | { kind: "peerIncompatible"; peer: number; ours: number }
  // 会话
  | { kind: "hostEnded" }
  | { kind: "hostUnresponsive" }
  | { kind: "kicked" }
  | { kind: "codeChanged" }
  /** 被控端的用户拒绝了连接 */
  | { kind: "declined" }
  /** 被控端的用户一直没有回应连接请求 */
  | { kind: "unanswered" }
  | { kind: "sessionEnded" }
  // 其它
  | { kind: "noSession" }
  | { kind: "noClipboardText" }
  | { kind: "failed"; detail: string }
  // 只在控制台里产生（Rust 端没有）
  /** 与控制端服务（桌面版：本程序；网页版：airhand-web）的连接断了 */
  | { kind: "consoleLost" }
  /** 浏览器不允许读取剪贴板 */
  | { kind: "clipboardBlocked" };

/** 被控端发来的提示，与 Rust 端 airhand_link::controller::NoticeView 对应 */
export type RemoteNotice =
  | { kind: "noDisplays" }
  | { kind: "wayland" }
  | { kind: "captureFailed"; detail: string }
  /** 屏幕采集恢复：撤下 captureFailed（本身不显示） */
  | { kind: "captureRecovered" }
  /** 被控端无法采集声音（只发给开着声音的控制端） */
  | { kind: "audioFailed"; detail: string }
  /** 声音采集恢复：撤下 audioFailed（本身不显示） */
  | { kind: "audioRecovered" }
  /** 被控端（macOS）缺少的系统权限；两项都为 false 表示已经全部授予 */
  | { kind: "missingPermissions"; screen: boolean; input: boolean };

export interface AppInfo {
  version: string;
  /** 中转服务器地址 */
  server: string;
  pinned: boolean;
  /** 被控端看到的本控制端名称（桌面版是计算机名） */
  hostname: string;
  lang: Lang;
  /** 配置文件（也保存着"我的设备"）的路径；网页版没有 */
  configPath?: string | null;
}

/** "我的设备"里的一台：连接过的电脑（与 Rust 端 devices::Device 对应，保存在 config.toml 的 [[device]] 里） */
export interface SavedDevice {
  /** 6 位控制码 */
  code: string;
  /** 用户起的名字，null 表示用对方的设备名 */
  alias: string | null;
  /** 对方的设备名 */
  name: string;
  os: string;
  /** 重连凭证：对方不在线时等它上线，不算猜码 */
  resume: ResumeToken | null;
  /** 最后一次连上的时间（Unix 毫秒） */
  lastUsed: number;
  /** 上次用的画质 */
  quality: QualitySettings | null;
  /** 对方的端到端加密公钥（64 位十六进制，第一次连上时记下）：之后只认它 */
  hostKey: string | null;
}

/**
 * 重连凭证（与 airhand_proto::signal::ResumeToken 对应，16 个字节）：连上时得到，会话断了之后重连时原样带上。
 * 带凭证的重连不会被服务器当成猜控制码而限速；对方不在线时回答 hostOffline，对方回来时通知 onHostOnline
 */
export type ResumeToken = number[];

export interface SessionInfo {
  sid: number;
  code: string;
  hostName: string;
  hostOs: string;
  resume: ResumeToken;
  /** 对方的端到端加密公钥：记进我的设备，下次连时带上（ConnectOptions.hostKey） */
  hostKey: string;
  /** 安全码：对方托盘里显示的是同一个 */
  safety: string;
}

/**
 * 怎样连这台电脑（除了控制码），开始时的画质和声音。与 Rust 的 airhand_link::controller::ConnectOptions 对应，
 * 三种界面的 connect 都是这些字段加上控制码
 */
export interface ConnectOptions {
  /** 重连凭证：之前连上时得到的，第一次连接时为 null */
  resume: ResumeToken | null;
  /** 之前记下的对方公钥：只认它（对不上是 hostKeyChanged） */
  hostKey: string | null;
  /** 配对码（对方要求配对时） */
  pairing: string | null;
  /** 开始时的画质：在端到端握手里交给对方，它一开始就按这个建编码器 */
  settings: QualitySettings;
  /** 开始时要不要声音（同上） */
  audio: boolean;
}

/** 被控端的平台，与 airhand_proto::session::Platform 对应（对方的版本更新、报了这边不认识的平台时是 linux） */
export type Platform = "windows" | "mac" | "linux" | "android";

/** 系统操作（手机的导航键等），与 airhand_proto::session::SystemAction 对应 */
export type SystemAction =
  | "back"
  | "home"
  | "recents"
  | "notifications"
  | "quickSettings"
  | "screenshot"
  | "lockScreen"
  | "powerMenu"
  | "volumeUp"
  | "volumeDown";

export type SessionEvent =
  | {
      type: "hello";
      hostname: string;
      os: string;
      version: string;
      displays: DisplayInfo[];
      current: number;
      codecs: Codec[];
      platform: Platform;
      /** 能接收多点触摸（控制台用鼠标，不发触摸） */
      touch: boolean;
      /** 支持的系统操作 */
      actions: SystemAction[];
    }
  /** 被控端在等它的用户同意这次连接，同意之后才会收到 hello */
  | { type: "awaitingApproval" }
  | { type: "displays"; displays: DisplayInfo[]; current: number }
  | {
      type: "stats";
      fps: number;
      kbps: number;
      rttMs: number | null;
      width: number;
      height: number;
      frames: number;
      /** 画面是从直连来的（两台设备之间打通的路，不经过服务器） */
      direct: boolean;
    }
  | { type: "encoder"; codec: Codec; name: string; hardware: boolean; width: number; height: number }
  | { type: "notice"; notice: RemoteNotice }
  /** 被控端复制的文本（只有网页版会收到，由 backend/web.ts 处理） */
  | { type: "clipboard"; text: string }
  | { type: "closed"; reason: Problem; retry: boolean };

export type MouseButton = "left" | "right" | "middle" | "back" | "forward";

export type TouchPhase = "down" | "move" | "up" | "cancel";

/** 与 airhand_proto::session::InputEvent 对应（以 type 区分） */
export type InputEvent =
  /** 移到这个位置（0..1，相对当前显示器的画面） */
  | { type: "mouseMove"; x: number; y: number }
  /** 相对移动（被控端光标隐藏时，例如游戏里转视角）：鼠标的原始计数，约等于被控端的像素 */
  | { type: "mouseDelta"; dx: number; dy: number }
  | { type: "mouseButton"; button: MouseButton; down: boolean }
  | { type: "wheel"; dx: number; dy: number }
  | { type: "key"; code: string; down: boolean }
  | { type: "text"; text: string }
  /** 触摸点（只发给能接收触摸的被控端；控制台用鼠标，不发） */
  | { type: "touch"; id: number; phase: TouchPhase; x: number; y: number }
  | { type: "system"; action: SystemAction }
  | { type: "releaseAll" };

/** 对会话的操作，与 airhand_link::controller::Command 对应（JSON 以 cmd 区分） */
export type Command =
  | { cmd: "input"; sid: number; events: InputEvent[] }
  | { cmd: "settings"; sid: number; settings: QualitySettings }
  /** 请被控端马上编一个关键帧 */
  | { cmd: "keyframe"; sid: number }
  /** 切换到被控端的另一个显示器 */
  | { cmd: "display"; sid: number; index: number }
  /** 开启 / 关闭被控端的声音 */
  | { cmd: "audio"; sid: number; enabled: boolean }
  /** 把这里复制的文本发给被控端（网页版：浏览器自己读剪贴板） */
  | { cmd: "clipboard"; sid: number; text: string }
  /** 打开 / 关闭剪贴板同步 */
  | { cmd: "clipboardSync"; sid: number; enabled: boolean }
  /** 收到了第 seq 帧画面（累积确认；控制台的 Worker 收到画面时自己发，走二进制，见 ack.ts） */
  | { cmd: "ack"; sid: number; seq: number }
  | { cmd: "disconnect"; sid: number };

/** 控制台与控制端服务之间的通道。会话的部分两种运行方式共用（console.ts 的 ConsoleLink） */
export interface Backend {
  /** desktop = 桌面版（Tauri 窗口）；web = 网页版（浏览器） */
  readonly kind: "desktop" | "web";
  appInfo(): Promise<AppInfo>;
  serverStatus(): Promise<ServerState>;
  /** 返回取消订阅的函数 */
  onServerStatus(cb: (s: ServerState) => void): () => void;
  /** 会话的画面：canvas 交给 Worker 绘制（之后主线程不能再画它），`out` 收它的消息 */
  view(canvas: HTMLCanvasElement, out: ViewOutput): MediaView;
  /**
   * 连接。`opts`：重连凭证、记下的对方公钥、配对码、开始时的画质和声音（见 ConnectOptions）；
   * `view`：画面和声音交给它；`onEvent`：会话事件；`clipboard`：开始时是否同步剪贴板
   */
  connect(
    code: string,
    opts: ConnectOptions,
    view: MediaView,
    onEvent: (ev: SessionEvent) => void,
    clipboard: boolean,
  ): Promise<SessionInfo>;
  /** 在等的被控端（connect 返回过 hostOffline）重新上线了，参数是控制码（数字）。返回取消订阅的函数 */
  onHostOnline(cb: (code: number) => void): () => void;
  /** 对会话的操作（画面的确认由 Worker 在收到画面时自己发） */
  command(c: Command): Promise<void>;
  /** 读取本机剪贴板文本（用于"作为按键输入"） */
  localClipboardText(): Promise<string>;
  /** 切换到被控端（只有桌面版） */
  switchToHost(): Promise<void>;
  /** 切换界面语言（桌面版同时作用于托盘菜单） */
  setLanguage(lang: Lang): Promise<void>;
  /** 语言被切换（包括在托盘菜单里切换） */
  onLangChanged(cb: (lang: Lang) => void): () => void;
  /** 让窗口全屏（桌面版；网页版只有网页全屏，什么也不做） */
  setWindowFullscreen(on: boolean): Promise<void>;
  /** 窗口退出了全屏（例如按了系统快捷键） */
  onWindowFullscreenExit(cb: () => void): () => void;
  /** 我的设备（桌面版：config.toml 里的）。返回 null 表示不能保存（网页版：只在本页面的内存里） */
  loadDevices(): Promise<SavedDevice[] | null>;
  /** 保存整个列表 */
  saveDevices(devices: SavedDevice[]): Promise<void>;
  /** 桌面版的无边框窗口（标题栏和窗口按钮由界面自己画）；网页版在浏览器里，为 null */
  readonly frame: WindowFrame | null;
}

/** 无边框窗口的操作。拖动和双击最大化由 Tauri 按 data-tauri-drag-region 处理 */
export interface WindowFrame {
  minimize(): Promise<void>;
  toggleMaximize(): Promise<void>;
  isMaximized(): Promise<boolean>;
  /** 窗口大小变化（包括最大化 / 还原）。返回取消订阅的函数 */
  onResized(cb: () => void): () => void;
  /** 请求关闭：和点系统的关闭按钮、Alt+F4 一样，先经过 onCloseRequested */
  close(): Promise<void>;
  /** 直接关闭，不再询问（关闭控制台 = 回到被控端模式） */
  destroy(): Promise<void>;
  /** 用户要关闭窗口：回调返回 false 时先不关（界面询问之后再调用 destroy）。返回取消订阅的函数 */
  onCloseRequested(cb: () => boolean): () => void;
}

export const api: Backend = isTauri() ? tauriBackend() : webBackend();

/** Rust 命令失败时返回的是 Problem；其它异常（例如前端自己抛出的）包装成 failed */
export function toProblem(e: unknown): Problem {
  if (typeof e === "object" && e !== null && typeof (e as { kind?: unknown }).kind === "string") return e as Problem;
  return { kind: "failed", detail: e instanceof Error ? e.message : String(e) };
}
