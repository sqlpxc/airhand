//! 会话的控制消息（被控端 ⇄ 控制端，经服务器透传，端到端加密）：带编号的 CBOR 消息（见 [`crate::framing`] 的"兼容规则"）。
//!
//! 画面和声音不在这里：它们各走一条流，用定长头 + 原始数据的媒体包（见 [`crate::media`]）。
//!
//! # 走哪条通道
//!
//! 消息走两条通道（[`crate::SessionLane`]），编码一样、各自有序：
//!
//! * **控制**：输入、确认、Ping / Pong、设置、提示……都很小，要快；
//! * **大块**：剪贴板（[`HostMsg::Clipboard`] / [`ControllerMsg::Clipboard`]，最多 1 MiB）和光标图像
//!   （[`HostMsg::CursorImage`]，几十 KB）。它们要是和控制消息挤在一条有序的流里，上行 5 Mbps 时一个 1 MiB 的剪贴板
//!   会把后面的消息挡住大约 1.7 秒：确认迟到，被控端的在途帧窗口停住、测到的往返时延变大，自适应码率误判为拥塞；
//!   输入晚到超过 [`INPUT_MAX_AGE`] 还会被当成过时的丢掉。
//!
//! 光标拆成两半：可见性和用哪个形状（[`HostMsg::Cursor`]，很小）走控制通道，形状的图像走大块通道。
//! 形状按槽位和代际引用（[`CursorRef`]），两条通道之间乱序也不会让晚到的旧图像盖掉新的选择。
//!
//! 大块通道的优先级和画面一样（QUIC 轮流发，谁也不独占），比控制、声音低。
//!
//! # 会话开始
//!
//! 控制通道上先做端到端握手（见 [`crate::handshake`]）：双方确认对方的身份、协商会话版本和能力位，
//! 被控端决定让不让这个控制端连（配对过的、输对了配对码的、或者被控端允许任何人连）。
//! 画质设置和要不要声音在控制端的介绍里（[`crate::handshake::ControllerIntro`]），被控端一开始就按它建编码器，
//! 不用先按默认设置建一个、等设置到了再重建（硬件编码器初始化就要上百毫秒，还要多发一个关键帧）。
//! 握手之后被控端通常马上发 [`HostMsg::Hello`]，之后开始发画面。
//!
//! 被控端也可以先请坐在它面前的人同意（[`HostMsg::AwaitingApproval`]，例如手机默认这样做）：
//! 同意之后才发 `Hello` 和画面，拒绝或者超时没人理会就发 [`HostMsg::Bye`]（[`ByeReason::Declined`] /
//! [`ByeReason::Unanswered`]）。等待期间照常回应 Ping，控制端不会把它当成失联。
//! 同时在等回答的请求有上限：再来的被控端不问，直接发 `Bye`（[`ByeReason::TooManyRequests`]）。
//! 被控端认得出同意过的控制端（端到端握手里控制端的公钥），断线重连时可以不再询问。
//!
//! # 鼠标
//!
//! 平时鼠标用绝对坐标（[`InputEvent::MouseMove`]）：控制端的指针在哪里，被控端的光标就在哪里。
//! 被控端的光标隐藏了（[`HostMsg::Cursor`] 的 `visible` 为 false，例如游戏里按住右键转视角、第一人称游戏）时，
//! 控制端改发相对移动（[`InputEvent::MouseDelta`]）：游戏这时自己把光标拉回原位、只看移动了多少，
//! 绝对坐标会和它打架（视角乱跳、拖到画面边上就转不动了）。
//!
//! # 过时的输入
//!
//! 网络卡住的时候，输入会积在路上：控制端的命令队列里、QUIC 的发送缓冲里（断网时往里写不会阻塞）、服务器上。
//! 网络恢复后把它们补发出去，就是在对方电脑上重放一串几秒、几分钟之前的操作。所以每批输入都带着控制端
//! 收到它的时刻（[`ControllerMsg::Input::t`]，和 [`ControllerMsg::Ping`] 是同一个时钟），比平时晚到超过
//! [`INPUT_MAX_AGE`] 的输入被控端不执行，改成释放所有按键（丢掉的里面可能有松开按键）。
//! 两边的时钟不需要同步，见 `airhand_link::host` 的 `InputClock`。

use std::time::Duration;

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::SessionLane;
use crate::cbor::open_enum;

/// 被控端 → 控制端。变体的编号是线上的类型，不能改、不能复用。走哪条通道见模块说明
#[derive(Encode, Decode, Debug, Clone)]
pub enum HostMsg {
    /// 会话开始（需要本机用户同意时，同意之后才发）
    #[n(1)]
    Hello(#[n(0)] HostHello),
    /// 正在等被控端的用户同意这次连接（见模块说明）
    #[n(2)]
    AwaitingApproval,
    /// 回应 [`ControllerMsg::Ping`]，用于测量端到端往返时延。
    #[n(3)]
    Pong {
        #[n(0)]
        t: u64,
    },
    /// 显示器列表或当前显示器变化。
    #[n(4)]
    Displays {
        #[n(0)]
        displays: Vec<DisplayInfo>,
        #[n(1)]
        current: u8,
    },
    /// 给控制端的提示（例如采集失败）。不认识的提示（对方版本新增的）是 `None`：不显示。发的时候总是 `Some`
    #[n(5)]
    Notice(#[cbor(n(0), with = "crate::cbor::opt", has_nil)] Option<Notice>),
    /// 编码器（重新）创建后通知控制端当前实际使用的编码参数。
    #[n(6)]
    Encoder(#[n(0)] EncoderInfo),
    /// 光标的最新状态：可见性，用哪个形状（`None`：还没有形状）。控制端用本地光标显示该形状（移动零延迟），
    /// 光标隐藏时改用相对移动（见模块说明）。走控制通道，只留最新值。
    /// 形状的图像还没到时先应用可见性，等那一代图像到了再换形状（见 [`HostMsg::CursorImage`]）
    #[n(7)]
    Cursor {
        #[n(0)]
        visible: bool,
        #[n(1)]
        shape: Option<CursorRef>,
    },
    /// 被控端剪贴板文本变化。走大块通道
    #[n(8)]
    Clipboard {
        #[n(0)]
        text: String,
    },
    /// 被控端主动结束会话（例如用户在托盘点了"断开所有控制端"），控制端不应自动重连。
    /// 原因不认识（[`ByeReason::Unknown`]，对方版本新增的）时会话照样结束
    #[n(9)]
    Bye(#[n(0)] ByeReason),
    /// 一个槽位的新图像（之前没发过的形状）。走大块通道；不带可见性，也不选择形状（那是 [`HostMsg::Cursor`] 的事）
    #[n(10)]
    CursorImage {
        #[n(0)]
        reference: CursorRef,
        #[n(1)]
        image: CursorShape,
    },
}

impl HostMsg {
    /// 这条消息走哪条通道（见模块说明的"走哪条通道"）
    pub fn lane(&self) -> SessionLane {
        match self {
            HostMsg::CursorImage { .. } | HostMsg::Clipboard { .. } => SessionLane::Bulk,
            HostMsg::Hello(_)
            | HostMsg::AwaitingApproval
            | HostMsg::Pong { .. }
            | HostMsg::Displays { .. }
            | HostMsg::Notice(_)
            | HostMsg::Encoder(_)
            | HostMsg::Cursor { .. }
            | HostMsg::Bye(_) => SessionLane::Control,
        }
    }
}

/// 被控端给控制端的提示。
///
/// 发送的是类型而不是文字：控制端按**自己的**界面语言显示，不会出现对方电脑的语言；
/// 怎么解决（例如去哪里授权）因平台而异，由控制端按 [`HostHello::platform`] 说明。
/// 会自动恢复的问题成对出现（`XxxFailed` / `XxxRecovered`）：恢复时控制端撤下之前的提示，`XxxRecovered` 本身不显示。
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// 被控端没有可以采集的显示器
    #[n(0)]
    NoDisplays,
    /// 被控端是 Wayland 桌面：只能采集和操作 X11（XWayland）程序
    #[n(1)]
    Wayland,
    /// 屏幕采集失败（被控端会自动重试）。`detail` 是给排查问题用的技术信息：英文，
    /// 不含操作系统按本地语言给出的错误说明（Windows 只带 HRESULT 错误码）。
    /// 缺少系统权限不算采集失败，用 [`Notice::MissingPermissions`] 说明。
    #[n(2)]
    CaptureFailed {
        #[n(0)]
        detail: String,
    },
    /// 屏幕采集恢复正常（之前发过 [`Notice::CaptureFailed`]）。
    #[n(3)]
    CaptureRecovered,
    /// 无法采集声音（被控端会自动重试），只发给开着声音的控制端。`detail` 同 [`Notice::CaptureFailed`]。
    /// 缺少系统权限时不发（已经用 [`Notice::MissingPermissions`] 说明）。
    #[n(4)]
    AudioFailed {
        #[n(0)]
        detail: String,
    },
    /// 声音采集恢复正常（之前发过 [`Notice::AudioFailed`]）。
    #[n(5)]
    AudioRecovered,
    /// 被控端缺少只能由那台设备的用户授予的权限：`screen` 缺了就没有画面和声音，`input` 缺了就不能操作。
    /// macOS 是"系统设置 → 隐私与安全性"里的"屏幕与系统录音"和"辅助功能"；Android 是 Airhand 的
    /// "远程操作"（无障碍服务），屏幕录制在开始共享时已经同意过，不会缺。
    ///
    /// 会话开始时缺少权限就发一次，之后权限有变化再发；两项都为 false 表示已经全部授予，
    /// 控制端据此撤下之前的提示。
    #[n(6)]
    MissingPermissions {
        #[n(0)]
        screen: bool,
        #[n(1)]
        input: bool,
    },
}

open_enum! {
    /// 被控端结束会话的原因
    pub enum ByeReason {
        /// 被控端的用户断开了所有控制端
        Kicked = 0 => "kicked",
        /// 被控端的用户更换了控制码（所有控制端随之断开）
        CodeChanged = 1 => "codeChanged",
        /// 被控端的用户拒绝了这次连接
        Declined = 2 => "declined",
        /// 被控端的用户一直没有回应连接请求
        Unanswered = 3 => "unanswered",
        /// 被控端上已经有好几个连接请求在等它的用户回答，这一个没有问（过一会儿再试）
        TooManyRequests = 4 => "tooManyRequests",
    }
}

/// 控制端按槽位缓存光标形状，最多这么多个。动画光标（例如忙碌时的转圈）反复用到同几个形状，
/// 被控端只在控制端还没有它时发图像，之后只发引用。哪个槽位放什么由被控端决定（控制端照做），
/// 两边不会对不上。槽位只在一个会话里有效
pub const CURSOR_SLOTS: u8 = 32;

/// 槽位 `slot`（`< CURSOR_SLOTS`）里的第 `generation` 代图像。槽位复用时代际变大：
/// 跨通道晚到的旧图像认得出来，不会盖掉新的
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRef {
    #[n(0)]
    pub slot: u8,
    #[n(1)]
    pub generation: u64,
}

#[derive(Encode, Decode, Debug, Clone, PartialEq, Hash)]
pub struct CursorShape {
    #[n(0)]
    pub width: u16,
    #[n(1)]
    pub height: u16,
    #[n(2)]
    pub hot_x: u16,
    #[n(3)]
    pub hot_y: u16,
    /// RGBA8，非预乘
    #[cbor(n(4), with = "minicbor::bytes")]
    pub rgba: Vec<u8>,
}

/// 被控端实际在用的编码器。JSON（Android 的 Kotlin 报给 Rust 的）是 camelCase
#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInfo {
    #[n(0)]
    pub codec: Codec,
    /// 例如 "NVIDIA H.264 Encoder MFT" / "OpenH264"
    #[n(1)]
    pub name: String,
    #[n(2)]
    pub hardware: bool,
    #[n(3)]
    pub width: u32,
    #[n(4)]
    pub height: u32,
}

#[derive(Encode, Decode, Debug, Clone)]
pub struct HostHello {
    #[n(0)]
    pub hostname: String,
    /// 操作系统的名称和版本，仅用于展示（例如 "Windows 11 Pro 24H2"、"Android 15"）
    #[n(1)]
    pub os: String,
    #[n(2)]
    pub version: String,
    #[n(3)]
    pub displays: Vec<DisplayInfo>,
    #[n(4)]
    pub current_display: u8,
    /// 被控端可用的编码格式（硬件或软件）。控制端不认识的（`Unknown`）不用管
    #[n(5)]
    pub codecs: Vec<Codec>,
    /// 被控端是什么平台：控制端据此选择默认的操作方式、快捷键，以及提示里的说法
    #[n(6)]
    pub platform: Platform,
    /// 能接收多点触摸（[`InputEvent::Touch`]）
    #[n(7)]
    pub touch: bool,
    /// 支持的系统操作（[`InputEvent::System`]），控制端只显示这些。控制端不认识的（`Unknown`）不显示
    #[n(8)]
    pub actions: Vec<SystemAction>,
}

open_enum! {
    /// 被控端的平台。控制端不认识的（对方版本新增的平台）当作一台普通的电脑（[`Platform::or_linux`]）
    pub enum Platform {
        Windows = 0 => "windows",
        Mac = 1 => "mac",
        Linux = 2 => "linux",
        Android = 3 => "android",
    }
}

impl Platform {
    /// 不认识的平台当作 Linux：一台普通的电脑（只有鼠标键盘、按通用的快捷键）
    pub fn or_linux(self) -> Self {
        if self.is_known() {
            self
        } else {
            Platform::Linux
        }
    }
}

open_enum! {
    /// 系统操作：手机上的导航键和系统界面（不是键盘上的按键，被控端用系统自己的方式执行）
    pub enum SystemAction {
        Back = 0 => "back",
        Home = 1 => "home",
        Recents = 2 => "recents",
        Notifications = 3 => "notifications",
        QuickSettings = 4 => "quickSettings",
        Screenshot = 5 => "screenshot",
        LockScreen = 6 => "lockScreen",
        PowerMenu = 7 => "powerMenu",
        VolumeUp = 8 => "volumeUp",
        VolumeDown = 9 => "volumeDown",
    }
}

#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    #[n(0)]
    pub index: u8,
    #[n(1)]
    pub name: String,
    #[n(2)]
    pub x: i32,
    #[n(3)]
    pub y: i32,
    #[n(4)]
    pub width: u32,
    #[n(5)]
    pub height: u32,
    #[n(6)]
    pub primary: bool,
}

open_enum! {
    /// 视频编码格式。控制端统一使用 WebCodecs（GPU 硬件解码）。编号和媒体包头里的一样（见 [`crate::media`]）
    #[derive(Default)]
    pub enum Codec {
        /// H.264 Annex-B：兼容性最好，支持软件编码兜底
        #[default]
        H264 = 0 => "h264",
        /// H.265/HEVC Annex-B：同画质约节省 30~40% 带宽（需要硬件编码器）
        H265 = 1 => "h265",
        /// AV1 OBU：同画质约节省 40~50% 带宽（需要较新的 GPU）
        AV1 = 2 => "av1",
    }
}

impl Codec {
    /// 不认识的格式换成 H.264（谁都支持的）
    pub fn or_h264(self) -> Self {
        if self.is_known() { self } else { Codec::H264 }
    }
}

/// 控制端 → 被控端。变体的编号是线上的类型，不能改、不能复用（v9 之前的 0 号是 `Hello`，
/// 现在由端到端握手代替）。走哪条通道见模块说明
#[derive(Encode, Decode, Debug, Clone)]
pub enum ControllerMsg {
    /// 批量输入事件（鼠标移动会被合并）。不认识的事件（对方版本新增的）被控端跳过，同一批里的其它照常执行。
    ///
    /// `t`：控制端收到这批输入的时刻（微秒，和 [`ControllerMsg::Ping::t`] 同一个时钟）。
    /// 被控端据此丢掉晚到太久的输入（见模块说明的"过时的输入"）
    #[n(1)]
    Input {
        #[n(0)]
        t: u64,
        #[cbor(n(1), with = "crate::cbor::open_vec")]
        events: Vec<InputEvent>,
    },
    /// 界面收到了第 `seq` 帧（累积确认），用于被控端的在途控制和码率控制（防止延迟堆积）。
    ///
    /// `arrived`：这一帧开始从网络上到达的时刻（读到它的长度前缀时，控制端的时钟，微秒，和
    /// [`ControllerMsg::Ping::t`] 同一个时钟；控制端不知道时为 `None`）。确认要等界面收到才发，往返时延里混着
    /// 界面那一段；到达时刻只算网络：被控端拿它减去自己的发送时刻，两边时钟之差是常数，变化的部分就是路上的排队
    /// （用开始到达的时刻，这一帧自己要传多久不算在里面），两次到达之间收到的字节数就是交付速率
    #[n(2)]
    Ack {
        #[n(0)]
        seq: u32,
        #[n(1)]
        arrived: Option<u64>,
    },
    /// 画质设置变了（开始时的设置在握手里，见 [`crate::handshake::ControllerIntro`]）
    #[n(3)]
    Settings(#[n(0)] QualitySettings),
    #[n(4)]
    RequestKeyframe,
    #[n(5)]
    SelectDisplay {
        #[n(0)]
        index: u8,
    },
    /// 每秒一次，被控端回 [`HostMsg::Pong`]。`t` 是控制端的时钟（微秒），被控端也用它估算输入晚到了多久
    #[n(6)]
    Ping {
        #[n(0)]
        t: u64,
    },
    /// 控制端剪贴板文本变化。走大块通道
    #[n(7)]
    Clipboard {
        #[n(0)]
        text: String,
    },
    /// 开启/关闭被控端声音（开始时开不开在握手里，见 [`crate::handshake::ControllerIntro`]）
    #[n(8)]
    Audio {
        #[n(0)]
        enabled: bool,
    },
}

impl ControllerMsg {
    pub fn lane(&self) -> SessionLane {
        match self {
            Self::Clipboard { .. } => SessionLane::Bulk,
            Self::Input { .. }
            | Self::Ack { .. }
            | Self::Settings(_)
            | Self::RequestKeyframe
            | Self::SelectDisplay { .. }
            | Self::Ping { .. }
            | Self::Audio { .. } => SessionLane::Control,
        }
    }
}

/// 剪贴板同步的文本上限
pub const MAX_CLIPBOARD_TEXT: usize = 1 << 20;

/// 输入在路上（控制端的队列、网络）耗了这么久就过时了：控制端不再发，被控端收到了也不执行
/// （见模块说明的"过时的输入"）
pub const INPUT_MAX_AGE: Duration = Duration::from_secs(2);

/// 画质设置（由控制端 UI 选择，被控端会做范围校验）。JSON（界面传来的）是 camelCase
#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QualitySettings {
    /// 最大帧率 1..=120
    #[n(0)]
    pub fps: u8,
    /// 目标码率 kbps
    #[n(1)]
    pub bitrate_kbps: u32,
    /// 输出画面短边上限（如 720/1080），0 表示不限制
    #[n(2)]
    pub max_short: u16,
    /// 根据网络状况自动下调/恢复码率
    #[n(3)]
    pub adaptive: bool,
    /// 期望的编码格式（被控端不支持时自动回退到 H.264；不认识的格式也是 H.264，见 [`QualitySettings::sanitized`]）
    #[n(4)]
    pub codec: Codec,
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate_kbps: 4_000,
            max_short: 1080,
            adaptive: true,
            codec: Codec::H264,
        }
    }
}

impl QualitySettings {
    /// 范围校验；不认识的编码格式（对方版本新增的）换成 H.264
    pub fn sanitized(mut self) -> Self {
        self.fps = self.fps.clamp(1, 120);
        self.bitrate_kbps = self.bitrate_kbps.clamp(100, 100_000);
        self.codec = self.codec.or_h264();
        self
    }
}

open_enum! {
    pub enum MouseButton {
        Left = 0 => "left",
        Right = 1 => "right",
        Middle = 2 => "middle",
        Back = 3 => "back",
        Forward = 4 => "forward",
    }
}

open_enum! {
    /// 触摸点的阶段
    pub enum TouchPhase {
        Down = 0 => "down",
        Move = 1 => "move",
        Up = 2 => "up",
        /// 手势被打断（例如控制端的系统接管了触摸）：被控端松开这个触摸点
        Cancel = 3 => "cancel",
    }
}

/// 同时按下的触摸点上限（与 Android 手势的上限一致）
pub const MAX_TOUCH_POINTS: u8 = 10;

/// 输入事件。
///
/// 里面的枚举值（按钮、触摸阶段、系统操作）是开放的：对方版本新增的值是 `Unknown`，被控端不执行这个事件。
/// 对方版本新增的事件整个跳过（[`ControllerMsg::Input`] 的 `events` 是 [`crate::cbor::open_vec`]）。
///
/// 界面传给控制端服务的 JSON：`{"type":"mouseMove","x":0.5,"y":0.5}`、`{"type":"releaseAll"}`（和其它界面 JSON 一样是 camelCase）
#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InputEvent {
    /// 移到这个位置：归一化坐标（0..=1），相对当前显示器画面
    #[n(0)]
    MouseMove {
        #[n(0)]
        x: f32,
        #[n(1)]
        y: f32,
    },
    /// 相对移动（被控端光标隐藏时，见模块说明）。单位是鼠标的原始计数，约等于被控端的一个像素；
    /// 被控端按真实鼠标的移动注入（游戏读到的就是这个移动量），不换算成坐标
    #[n(1)]
    MouseDelta {
        #[n(0)]
        dx: i32,
        #[n(1)]
        dy: i32,
    },
    #[n(2)]
    MouseButton {
        #[n(0)]
        button: MouseButton,
        #[n(1)]
        down: bool,
    },
    /// 滚轮，120 = 一格
    #[n(3)]
    Wheel {
        #[n(0)]
        dx: i16,
        #[n(1)]
        dy: i16,
    },
    /// 物理按键，取值为 W3C `KeyboardEvent.code`（如 `KeyA`、`ControlLeft`）
    #[n(4)]
    Key {
        #[n(0)]
        code: String,
        #[n(1)]
        down: bool,
    },
    /// 直接输入一段 Unicode 文本
    #[n(5)]
    Text {
        #[n(0)]
        text: String,
    },
    /// 释放所有按下的键（控制端失焦时发送，防止按键粘连）
    #[n(6)]
    ReleaseAll,
    /// 触摸点（只发给 [`HostHello::touch`] 的被控端）。`id` 区分同时按下的手指（小于 [`MAX_TOUCH_POINTS`]），
    /// 坐标同 `MouseMove`。同一批里的触摸事件是同一时刻的
    #[n(7)]
    Touch {
        #[n(0)]
        id: u8,
        #[n(1)]
        phase: TouchPhase,
        #[n(2)]
        x: f32,
        #[n(3)]
        y: f32,
    },
    /// 系统操作（只发被控端在 [`HostHello::actions`] 里列出的）
    #[n(8)]
    System {
        #[n(0)]
        action: SystemAction,
    },
}

impl InputEvent {
    /// 把一批输入里相邻的鼠标移动合并：绝对移动只留最后一个，相对移动相加。
    /// 按键、点击的先后顺序不变（移动不会越过点击）
    pub fn coalesce(events: &mut Vec<InputEvent>) {
        let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
        for e in events.drain(..) {
            match (out.last_mut(), e) {
                (Some(InputEvent::MouseMove { x, y }), InputEvent::MouseMove { x: nx, y: ny }) => {
                    (*x, *y) = (nx, ny);
                }
                (
                    Some(InputEvent::MouseDelta { dx, dy }),
                    InputEvent::MouseDelta { dx: ndx, dy: ndy },
                ) => {
                    *dx = dx.saturating_add(ndx);
                    *dy = dy.saturating_add(ndy);
                }
                (_, e) => out.push(e),
            }
        }
        *events = out;
    }

    /// 事件里的枚举值都认识（对方版本新增的按钮、触摸阶段、系统操作不认识）。不认识的事件被控端不执行
    pub fn understood(&self) -> bool {
        match self {
            InputEvent::MouseButton { button, .. } => button.is_known(),
            InputEvent::Touch { phase, .. } => phase.is_known(),
            InputEvent::System { action } => action.is_known(),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_keeps_order_and_sums_deltas() {
        let mut v = vec![
            InputEvent::MouseMove { x: 0.1, y: 0.1 },
            InputEvent::MouseMove { x: 0.2, y: 0.3 },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
            },
            InputEvent::MouseDelta { dx: 3, dy: -1 },
            InputEvent::MouseDelta { dx: 4, dy: -2 },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: false,
            },
            InputEvent::MouseDelta { dx: 1, dy: 1 },
        ];
        InputEvent::coalesce(&mut v);
        assert_eq!(
            v,
            [
                InputEvent::MouseMove { x: 0.2, y: 0.3 },
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    down: true
                },
                InputEvent::MouseDelta { dx: 7, dy: -3 },
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    down: false
                },
                InputEvent::MouseDelta { dx: 1, dy: 1 },
            ]
        );
    }

    #[test]
    fn codec_codes_match_the_media_header() {
        for (c, id) in [(Codec::H264, 0), (Codec::H265, 1), (Codec::AV1, 2)] {
            assert_eq!(c.code(), id);
            assert_eq!(Codec::from_code(id), c);
            // CBOR 里的编号和媒体包头里的一样
            assert_eq!(minicbor::to_vec(c).unwrap(), [id as u8]);
        }
        assert_eq!(Codec::from_code(3), Codec::Unknown(3));
        assert_eq!(Codec::KNOWN, [Codec::H264, Codec::H265, Codec::AV1]);
    }

    /// 对方版本新增的编码格式、结束原因：认识的照用，不认识的留着原值，由收的一方决定怎么办
    #[test]
    fn new_enum_values_from_a_newer_peer() {
        let wire = |codec: u8| {
            let mut e = minicbor::Encoder::new(Vec::new());
            e.array(5).unwrap();
            e.u8(60).unwrap().u32(8000).unwrap().u16(1080).unwrap();
            e.bool(true).unwrap().u8(codec).unwrap();
            e.into_writer()
        };
        let q: QualitySettings = minicbor::decode(&wire(9)).unwrap();
        assert_eq!((q.fps, q.codec), (60, Codec::Unknown(9)));
        assert_eq!(q.sanitized().codec, Codec::H264);
        let q: QualitySettings = minicbor::decode(&wire(2)).unwrap();
        assert_eq!(q.codec, Codec::AV1);

        // Bye(77)：[9, [77]]
        let mut e = minicbor::Encoder::new(Vec::new());
        e.array(2).unwrap().u32(9).unwrap();
        e.array(1).unwrap().u32(77).unwrap();
        let bye = e.into_writer();
        let m: HostMsg = minicbor::decode(&bye).unwrap();
        assert!(matches!(m, HostMsg::Bye(ByeReason::Unknown(77))));

        // 新的平台、编码格式、系统操作：留在列表里，其它字段照常
        let hello = HostHello {
            hostname: "PC".into(),
            os: "OS".into(),
            version: "1".into(),
            displays: vec![],
            current_display: 0,
            codecs: vec![Codec::Unknown(7), Codec::H265],
            platform: Platform::Unknown(9),
            touch: true,
            actions: vec![SystemAction::Unknown(40), SystemAction::Home],
        };
        let back: HostHello = minicbor::decode(&minicbor::to_vec(&hello).unwrap()).unwrap();
        assert_eq!(back.codecs, hello.codecs);
        assert_eq!(back.platform.or_linux(), Platform::Linux);
        assert_eq!(back.actions, hello.actions);
        assert!(back.touch);

        // 按钮不认识的输入事件：解得出来，被控端不执行
        let e = InputEvent::MouseButton {
            button: MouseButton::Unknown(12),
            down: true,
        };
        let back: InputEvent = minicbor::decode(&minicbor::to_vec(&e).unwrap()).unwrap();
        assert_eq!(back, e);
        assert!(!back.understood());
        assert!(InputEvent::ReleaseAll.understood());
    }

    /// 界面的 JSON 都是 camelCase（和 `airhand_link::controller` 的命令、事件一样）
    #[test]
    fn json_is_camel_case() {
        use serde_json::json;
        let q = QualitySettings {
            fps: 60,
            bitrate_kbps: 8000,
            max_short: 1080,
            adaptive: true,
            codec: Codec::AV1,
        };
        assert_eq!(
            serde_json::to_value(q).unwrap(),
            json!({ "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "av1" })
        );
        let events = vec![
            InputEvent::MouseMove { x: 0.5, y: 0.25 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
            InputEvent::Touch {
                id: 1,
                phase: TouchPhase::Cancel,
                x: 0.0,
                y: 1.0,
            },
            InputEvent::System {
                action: SystemAction::QuickSettings,
            },
            InputEvent::ReleaseAll,
        ];
        let v = serde_json::to_value(&events).unwrap();
        assert_eq!(
            v,
            json!([
                { "type": "mouseMove", "x": 0.5, "y": 0.25 },
                { "type": "mouseButton", "button": "left", "down": true },
                { "type": "touch", "id": 1, "phase": "cancel", "x": 0.0, "y": 1.0 },
                { "type": "system", "action": "quickSettings" },
                { "type": "releaseAll" }
            ])
        );
        assert_eq!(
            serde_json::from_value::<Vec<InputEvent>>(v).unwrap(),
            events
        );
        assert_eq!(
            serde_json::to_value(Platform::Android).unwrap(),
            json!("android")
        );
    }
}
