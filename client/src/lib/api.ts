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
import type { LocalFiles } from "./files";
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
  /**
   * 让对方把光标画进画面里（游戏模式，见 input.ts）。不是画质的一部分：由会话按鼠标模式在发出去的时候加上
   * （session.ts 的 effectiveSettings），保存的画质里没有它
   */
  drawCursor?: boolean;
}

/**
 * 鼠标模式（见 input.ts 和 airhand_proto::session 的"鼠标"）：
 * auto：平时跟着本机指针，对方隐藏光标时（游戏里转视角）自动锁定、发相对移动（默认）；
 * desktop：一直跟着本机指针，从不锁定；
 * game：锁定本机指针、一直发相对移动，对方把光标画进画面里
 */
export type MouseMode = "auto" | "desktop" | "game";
export const MOUSE_MODES: MouseMode[] = ["auto", "desktop", "game"];

export interface DisplayInfo {
  index: number;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  primary: boolean;
}

/**
 * 一路画面看哪个显示器（airhand_proto::session::StreamView，见那里的"多屏"）：`display` 是 DisplayInfo.index，
 * `maxShort` 是按这一路显示的大小要的短边上限（0 = 不限，和画质设置里的取较严的）
 */
export interface StreamView {
  display: number;
  maxShort: number;
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
  /** 上次用的鼠标模式；没有（自动、之前的版本、网页版刚记下的）就是自动 */
  mouse?: MouseMode | null;
  /** 操作这台 Mac 时互换物理 Ctrl 和 Command 键，默认关闭 */
  swapCtrlCommand?: boolean;
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
  /** 开始时看哪几个显示器（第 i 项是第 i 路画面）；不带、空的是只看主显示器（第一次连时还不知道对方有哪些显示器） */
  view?: StreamView[];
  /** 开始时要不要防窥模式：重连时带上现在的开关，对方的屏幕不会在重连的间隙里露出来；不带是不要 */
  privacy?: boolean;
}

/**
 * 剪贴板里的文件的一项（airhand_proto::files::FileEntry）：公告里 path 只有一段（名称），列表里是相对被列的目录的几段。
 * 链接带着它指向的路径（分隔符是 /）
 */
export interface FileEntry {
  path: string[];
  kind: "file" | "dir" | "link";
  size: number;
  /** 修改时间（Unix 毫秒） */
  mtime: number | null;
  target?: string;
}

/** 剪贴板里的文件为什么传不了（airhand_proto::files::FileError） */
export type FileError =
  | "notFound"
  | "denied"
  | "changed"
  | "io"
  | "tooLarge"
  | "expired"
  | "invalid"
  | "busy"
  | "tooMany"
  | "closed";

/** 被控端的平台，与 airhand_proto::session::Platform 对应（对方的版本更新、报了这边不认识的平台时是 linux） */
export type Platform = "windows" | "mac" | "linux" | "android" | "ios";

/**
 * 被控端能不能接本机的麦克风（airhand_proto::session::MicState）：unsupported = 那个平台还不支持；
 * noDevice = 打开转发时可安装驱动，steamMissing = 需要先安装 Steam，preparing = 正在准备设备，setupFailed = 驱动未就绪。
 */
export type MicState = "unsupported" | "noDevice" | "ready" | "busy" | "failed" | "steamMissing" | "preparing" | "setupFailed";

/**
 * 被控端的防窥模式（airhand_proto::session::PrivacyState）：unavailable = 对方没装虚拟显示器驱动（或不是为所有用户安装的），
 * Mac 没有允许辅助功能，notAllowed = 对方的设置不允许，starting = 正在打开（Windows 换成虚拟显示器），
 * failed = 刚才没能打开（已恢复原样）。
 */
export type PrivacyState = "unavailable" | "notAllowed" | "off" | "starting" | "on" | "failed";

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
      /** 第 0 路画面是哪个显示器 */
      current: number;
      /** 每一路画面是哪个显示器（第 i 项是第 i 路） */
      shown: number[];
      /** 对方最多能同时发几路画面（同时看几个显示器） */
      maxStreams: number;
      codecs: Codec[];
      platform: Platform;
      /** 能接收多点触摸（控制台用鼠标，不发触摸） */
      touch: boolean;
      /** 支持的系统操作 */
      actions: SystemAction[];
    }
  | { type: "displays"; displays: DisplayInfo[]; current: number; shown: number[] }
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
  /** 第 stream 路画面的编码器 */
  | { type: "encoder"; stream: number; codec: Codec; name: string; hardware: boolean; width: number; height: number }
  | { type: "notice"; notice: RemoteNotice }
  /** 被控端复制的文本（只有网页版会收到，由 backend/web.ts 处理） */
  | { type: "clipboard"; text: string }
  /** 被控端能不能接本机的麦克风：会话开始时一次，之后有变化再来；对方的版本旧时没有。detail：出错时的技术信息 */
  | { type: "mic"; state: MicState; detail: string | null }
  /** 被控端的防窥模式：会话开始时一次，之后有变化再来；对方不支持（Linux、手机、版本旧）时没有。detail：没能打开的原因 */
  | { type: "privacy"; state: PrivacyState; detail: string | null }
  /** 被控端复制了文件（只有网页版会收到；桌面版直接放进本机剪贴板）：要内容时用 remoteList / remoteRead 取 */
  | { type: "clipboardFiles"; clip: number; items: FileEntry[] }
  /** 剪贴板里的文件传不了（被控端那边的、这边的）：哪个文件、为什么 */
  | { type: "fileError"; name: string; error: FileError }
  | { type: "closed"; reason: Problem; retry: boolean };

export type MouseButton = "left" | "right" | "middle" | "back" | "forward";

export type TouchPhase = "down" | "move" | "up" | "cancel";

/** 与 airhand_proto::session::InputEvent 对应（以 type 区分） */
export type InputEvent =
  /** 移到这个位置（0..1，相对第 stream 路画面；第 0 路不写） */
  | { type: "mouseMove"; x: number; y: number; stream?: number }
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
  /** 请被控端马上给第 stream 路画面编一个关键帧（不写是第 0 路） */
  | { cmd: "keyframe"; sid: number; stream?: number }
  /** 只看被控端的第 index 个显示器 */
  | { cmd: "display"; sid: number; index: number }
  /** 看被控端的哪几个显示器：第 i 项是第 i 路画面（多屏同时显示） */
  | { cmd: "view"; sid: number; streams: StreamView[] }
  /** 开启 / 关闭被控端的声音 */
  | { cmd: "audio"; sid: number; enabled: boolean }
  /** 打开 / 关闭本机的麦克风（打开之后 Worker 把编好的声音包发给控制端服务，见 media.ts 的 micMessage） */
  | { cmd: "mic"; sid: number; enabled: boolean }
  /** 打开 / 关闭被控端的防窥模式（对方的显示器黑屏、键盘鼠标不起作用） */
  | { cmd: "privacy"; sid: number; on: boolean }
  /** 把这里复制的文本发给被控端（网页版：浏览器自己读剪贴板） */
  | { cmd: "clipboard"; sid: number; text: string }
  /** 打开 / 关闭剪贴板同步 */
  | { cmd: "clipboardSync"; sid: number; enabled: boolean }
  /** 这里的剪贴板换成了文件以外的东西：之前公告的文件不再是最新的（网页版） */
  | { cmd: "clipboardCleared"; sid: number }
  /** 收到了第 seq 帧画面（累积确认；控制台的 Worker 收到画面时自己发，走二进制，见 ack.ts） */
  | { cmd: "ack"; sid: number; seq: number; stream?: number }
  | { cmd: "disconnect"; sid: number };

/** 桌面版的弹出窗口（Tauri 窗口，按 label 找到它，见 src-tauri/src/popout.rs） */
export interface PopoutFrames {
  setFullscreen(label: string, on: boolean): Promise<void>;
  isFullscreen(label: string): Promise<boolean>;
  /** 把窗口拿到前面（最小化了的先还原） */
  focus(label: string): Promise<void>;
  close(label: string): Promise<void>;
}

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
  /**
   * 弹出窗口（把对方的一个显示器放到单独的窗口里，见 popout.ts）的全屏、关闭：桌面版按窗口的 label 由 Rust 处理
   * （网页全屏不会让 WebView2 的窗口变大，window.close 在 macOS、Linux 上关不掉窗口）；网页版没有，用浏览器自己的
   */
  readonly popouts?: PopoutFrames;
  /** 我的设备（桌面版：config.toml 里的）。返回 null 表示不能保存（网页版：只在本页面的内存里） */
  loadDevices(): Promise<SavedDevice[] | null>;
  /** 保存整个列表 */
  saveDevices(devices: SavedDevice[]): Promise<void>;
  /**
   * 剪贴板里的文件（见 files.ts）。这里复制的文件（粘贴到画面上、拖到画面上的）公告给被控端，它粘贴时才来取
   * （由 `files` 读）；桌面版的本机剪贴板由 Rust 端自己同步，只有拖放走这里
   */
  offerFiles(sid: number, files: LocalFiles): Promise<void>;
  /** 被控端复制的文件（clipboardFiles 事件）：列出目录 */
  remoteList(sid: number, clip: number, path: string[], recursive: boolean): Promise<FileEntry[]>;
  /** 被控端复制的文件：从 offset 开始最多读 len 字节（`file` 是这边选的编号，一个文件一个；读完了 remoteClose） */
  remoteRead(
    sid: number,
    clip: number,
    file: number,
    path: string[],
    size: number,
    mtime: number | null,
    offset: number,
    len: number,
  ): Promise<Uint8Array>;
  remoteClose(sid: number, file: number): void;
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
