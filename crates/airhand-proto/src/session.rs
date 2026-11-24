//! 会话的控制消息（被控端 ⇄ 控制端，经服务器透传，端到端加密）：带编号的 CBOR 消息（见 [`crate::framing`] 的"兼容规则"）。
//!
//! 画面和声音不在这里：它们各走一条流，用定长头 + 原始数据的媒体包（见 [`crate::media`]）。
//!
//! # 走哪条通道
//!
//! 消息走两条通道（[`crate::SessionLane`]）和控制数据报，编码一样：
//!
//! * **控制数据报**（[`ControllerDatagram`] / [`HostDatagram`]，`crate::datagram::CONTROL`）：Ping / Pong、画面确认、
//!   探测和回声。只看最新值、丢了也不要紧，不用排在流里别的消息后面、也不会被丢了的包挡住（被控端的码率控制、
//!   控制端测的往返时延都靠它们）。丢了的确认由控制端重发（见 [`ControllerDatagram::Ack`]）。直连通着时 Ping、Pong、
//!   确认两条路各发一份（同一个计数，后到的那份被接收端的重放窗口挡掉）；探测和回声只发在要量的那条路上；
//! * **控制**：输入、设置、提示……都很小，要快，不能丢、不能乱序；
//! * **大块**：剪贴板（[`HostMsg::Clipboard`] / [`ControllerMsg::Clipboard`]，最多 1 MiB）、光标图像
//!   （[`HostMsg::CursorImage`]，几十 KB）、剪贴板里的文件的公告和列表（[`crate::files::FileMsg::lane`]）。它们要是和控制消息挤在一条有序的流里，上行 5 Mbps 时一个 1 MiB 的剪贴板
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
//! 控制通道上先做端到端握手（见 [`crate::handshake`]）：双方确认对方的身份，校验会话版本一致，
//! 被控端决定让不让这个控制端连（配对过的、输对了配对码的；只凭控制码进不来）。
//! 画质设置和要不要声音在控制端的介绍里（[`crate::handshake::ControllerIntro`]），被控端一开始就按它建编码器，
//! 不用先按默认设置建一个、等设置到了再重建（硬件编码器初始化就要上百毫秒，还要多发一个关键帧）。
//! 握手之后被控端马上发 [`HostMsg::Hello`]，之后开始发画面。
//!
//! # 鼠标
//!
//! 鼠标有两种发法，光标也有两种显示法，原则是**谁决定光标的位置，就由谁画光标**：
//!
//! * **绝对坐标**（[`InputEvent::MouseMove`]）：控制端的指针在哪里，被控端的光标就在哪里。位置由控制端决定，
//!   光标也就用控制端的本地光标显示（形状是被控端的，见 [`HostMsg::Cursor`]），移动零延迟；
//! * **相对移动**（[`InputEvent::MouseDelta`]）：只说移动了多少，被控端像真鼠标一样注入。光标落在哪里由被控端决定
//!   （指针速度和加速、屏幕边缘、游戏自己把光标拉回原位），控制端不知道，本地光标表示不了它：要看光标，
//!   就让被控端把它画进画面里（[`QualitySettings::draw_cursor`]）。
//!
//! 控制端（控制台）据此有三种鼠标模式：
//!
//! * 自动（默认）：平时发绝对坐标；被控端的光标隐藏了（[`CursorVisibility::Hidden`]，例如游戏里按住右键转视角、
//!   第一人称游戏）时改发相对移动：游戏这时自己把光标拉回原位、只看移动了多少，绝对坐标会和它打架
//!   （视角乱跳、拖到画面边上就转不动了）。光标隐藏着，画面里也就不用有光标；
//! * 桌面：一直发绝对坐标，不管被控端的光标隐藏没有；
//! * 游戏：一直发相对移动，被控端把光标画进画面里（`draw_cursor`），控制端不显示本地光标。
//!   游戏里转视角、点界面都是对方真实的光标，不用看对方的光标隐藏没有，也就不会反复切换。
//!
//! 光标显示着、但已经画在画面里了（[`CursorVisibility::InFrame`]，被控端改用了软件光标，例如 Windows 拖动、
//! 缩放窗口时）：不算隐藏，控制端照常发绝对坐标。本地光标还显示不显示由控制端的用户决定：显示着跟手，
//! 只是和画面里的一起有两个；不显示就只看画面里的（控制台和 Android 的设置里都默认显示）。
//!
//! # 过时的输入
//!
//! 网络卡住的时候，输入会积在路上：控制端的命令队列里、QUIC 的发送缓冲里（断网时往里写不会阻塞）、服务器上。
//! 网络恢复后把它们补发出去，就是在对方电脑上重放一串几秒、几分钟之前的操作。所以每批输入都带着控制端
//! 收到它的时刻（[`ControllerMsg::Input::t`]，和 [`ControllerDatagram::Ping`] 是同一个时钟），比平时晚到超过
//! [`INPUT_MAX_AGE`] 的输入被控端不执行，改成释放所有按键（丢掉的里面可能有松开按键）。
//! 两边的时钟不需要同步，见 `airhand_link::host` 的 `InputClock`。
//!
//! # 直连
//!
//! 会话开始之后（被控端发了 [`HostMsg::Hello`] 之后）被控端发 [`HostMsg::Direct`]（本端的候选地址和直连证书的指纹），
//! 控制端回 [`ControllerMsg::Direct`]（它的），然后两端打洞、建直连（见 crate 说明的"直连"）。这些都在加密的控制通道上：
//! 中转看不到局域网地址，也换不掉证书的指纹。直连断了被控端可以再发一轮（`round` 加 1），控制端按最新的一轮回答。
//! 不想建直连的一端不发（被控端）/ 不回答（控制端）就是了。
//!
//! # 关键帧
//!
//! 控制端要关键帧时带着它收到的最后一帧的序号（[`ControllerMsg::RequestKeyframe::after`]）：被控端在那之后已经发过
//! 关键帧（正在路上）就不再编一个。控制端的几个地方（解码出错、换路时缺了帧、界面刚切回来）各自要、在一个往返里
//! 连着要，被控端也只编一个：关键帧是最大的帧，偏偏又是在网络最差的时候要。
//!
//! # 帧流
//!
//! v10 每帧使用一条 QUIC 单向流，直连和中转使用相同的格式。
//!
//! * 一帧丢了包只卡住它自己那一条流，后面的帧照样到；
//! * 增量帧带着参考帧（[`crate::media`] 的"参考帧"），控制端据此判断哪些帧交得给解码器：缺了参考帧的先留着，
//!   等它补到；
//! * 后面的帧到了、前面的一帧却一直等不到（重传也丢了、路上堵着），控制端发 [`ControllerDatagram::Lost`]，带着它交给
//!   解码器的最后一帧（`decoded`）。被控端放弃在途的、`decoded` 之后的帧（整条流 reset，不再重传过时的数据），
//!   下一帧只参考 `decoded` 和它之前控制端已经有的帧：编码器支持长期参考帧时就参考其中一个长期参考帧（参考帧失效，
//!   比关键帧小得多），不支持就编关键帧。控制端在恢复之前定时重发 `Lost`；被控端收到恢复点已经在路上时的重复请求不再编；
//! * 一直恢复不了（等了一秒多，见 `airhand_link::merge`）就照旧要关键帧（[`ControllerMsg::RequestKeyframe`]，
//!   走控制通道，不会丢）。
//!
//! 控制端每 [`REPORT_EVERY`] 按显示器报告收完的帧数和等重传停顿的帧数，供被控端调整码率。
//!
//! # 麦克风
//!
//! 控制端可以把本机麦克风的声音交给被控端，被控端把它放进一个虚拟麦克风，那台电脑上的程序（会议、语音输入）
//! 就像用着一个本地麦克风。能不能用由被控端的平台和设备决定：
//!
//! * 被控端在 [`HostMsg::Hello`] 之后发 [`HostMsg::Mic`]，说明它现在能不能接（[`MicState`]：平台不支持、
//!   没有虚拟麦克风、需要安装 Steam、准备设备、能接、被别的控制端占着、出错了），之后有变化再发；
//! * 控制端在能接时发 [`ControllerMsg::Mic`] 打开，之后的声音和被控端的声音一样走声音数据报（方向相反，
//!   同样带着之前几包的副本，见 [`crate::media`] 的"声音数据报"）；关掉时也发 [`ControllerMsg::Mic`]。
//!   被控端只在打开着的时候接（同一时刻只给一个控制端，后来的是 [`MicState::Busy`]）；
//! * 被控端没有声音放进去的时候虚拟麦克风里是静音。
//!
//! # 多屏
//!
//! 被控端有好几个显示器时，控制端可以同时看、同时操作其中几个（默认只看一个）。每个显示器是一**路**画面：
//!
//! * 第 `i` 路走画面通道的第 `i` 路（[`crate::SessionLane::Video`]，每一路一条通道、一把密钥、一串计数），
//!   画面包头也带有路号。每一路独立编码、确认和恢复。
//! * 控制端用 [`ControllerMsg::View`] 说看哪几个显示器（第 `i` 项是第 `i` 路），以及每一路按显示的大小要多大的画面
//!   （[`StreamView::max_short`]，和画质设置里的短边上限取较严的）。开始时看哪几个在握手里
//!   （[`crate::handshake::ControllerIntro::view`]，空的是只看主显示器）。被控端最多同时发 [`HostHello::streams`] 路
//!   （不超过 [`crate::MAX_STREAMS`]），多出来的、重复的、不存在的显示器不要。[`ControllerMsg::SelectDisplay`] 是只看一个
//!   显示器的简便说法（这一路的大小只看画质设置）；
//! * 被控端在 [`HostMsg::Displays`] 里告诉控制端现在每一路是哪个显示器（[`HostMsg::Displays::shown`]，显示器插拔时
//!   跟着它走，拔掉了就不要那一路）；
//! * 和画面有关的反馈都带着是第几路：确认（[`ControllerDatagram::Ack::stream`]）、等不到帧了
//!   （[`ControllerDatagram::Lost::stream`]）、收得怎么样（[`ControllerDatagram::Report`]）、要关键帧
//!   （[`ControllerMsg::RequestKeyframe::stream`]）、编码器（[`EncoderInfo::stream`]）；
//! * 绝对坐标的鼠标移动说的是第几路画面上的位置（[`InputEvent::MouseMove::stream`]）。键盘、相对移动和画面无关；
//! * 画质设置（帧率、码率、编码格式……）对每一路分别生效；光标、声音、剪贴板是整个会话一份。
//!
//! 第 0 路编号为 0，省略末尾的路号也表示第 0 路。
//!
//! # 剪贴板里的文件
//!
//! 剪贴板里除了文字还可以是文件：复制时只发清单
//! （[`crate::files::FileMsg::Offer`]），粘贴时才传内容（文件通道 [`crate::SessionLane::File`]），两个方向一样，
//! 见 [`crate::files`]。消息包在 [`HostMsg::Files`] / [`ControllerMsg::Files`] 里，走哪条通道看里面的消息。
//! 文字和文件互相取代：剪贴板里只有最近的一样。
//!
//! # 防窥模式
//!
//! 控制端可以让被控端的本机屏幕看不到画面、本机的键盘鼠标不起作用，只有控制端能看、能操作
//! （装了虚拟显示器驱动的 Windows 被控端、允许了"辅助功能"的 macOS 被控端支持）：
//!
//! * 被控端在 [`HostMsg::Hello`] 之后发 [`HostMsg::Privacy`]，说明现在能不能开、开着没有（[`PrivacyState`]），
//!   之后有变化再发。不支持的被控端不发，控制端也就不显示这个开关；
//! * 控制端用 [`ControllerMsg::Privacy`] 打开、关闭。开着是整台被控端的事：几个控制端看到的是同一个状态，
//!   哪一个都能关；打开它的控制端都断开了，过一会儿（给它重连的时间）自动关闭；
//! * 重连时控制端在握手的介绍里说要不要开着（[`crate::handshake::ControllerIntro::privacy`]）：被控端一开始就
//!   保持防窥，本机屏幕不会在重连的间隙露出来。

use std::net::SocketAddr;
use std::time::Duration;

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::SessionLane;
use crate::cbor::open_enum;
use crate::files::FileMsg;

/// 被控端 → 控制端。变体的编号是线上的类型，不能改、不能复用。走哪条通道见模块说明
#[derive(Encode, Decode, Debug, Clone)]
pub enum HostMsg {
    /// 会话开始
    #[n(1)]
    Hello(#[n(0)] HostHello),
    // 2：以前的"等被控端的用户同意"，已经去掉了，编号不再用
    /// 显示器列表或者正在看的显示器变了。
    #[n(4)]
    Displays {
        #[n(0)]
        displays: Vec<DisplayInfo>,
        /// 第 0 路画面是哪个显示器（`displays` 里的序号）
        #[n(1)]
        current: u8,
        /// 每一路画面是哪个显示器：第 `i` 项是第 `i` 路（见模块说明的"多屏"），第 0 项就是 `current`
        #[cbor(n(2), default)]
        shown: Vec<u8>,
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
        visibility: CursorVisibility,
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
    /// 被控端的直连候选地址：开始（第 `round` 轮）打洞（见模块说明的"直连"）
    #[n(11)]
    Direct(#[n(0)] DirectOffer),
    /// 被控端现在能不能接控制端的麦克风。Hello 之后发一次，之后有变化再发。
    /// `detail`：出错时给排查问题用的技术信息（英文，同 [`Notice::CaptureFailed`]）
    #[n(12)]
    Mic {
        #[n(0)]
        state: MicState,
        #[n(1)]
        detail: Option<String>,
    },
    /// 剪贴板里的文件（见模块说明的"剪贴板里的文件"）。
    /// 不认识的（对方版本新增的）是 `None`：跳过
    #[n(13)]
    Files(#[cbor(n(0), with = "crate::cbor::opt", has_nil)] Option<FileMsg>),
    /// 防窥模式现在怎么样（见模块说明的"防窥模式"）。Hello 之后发一次，之后有变化再发；不支持的被控端不发。
    /// `detail`：开不了时给排查问题用的技术信息（英文，同 [`Notice::CaptureFailed`]）
    #[n(14)]
    Privacy {
        #[n(0)]
        state: PrivacyState,
        #[n(1)]
        detail: Option<String>,
    },
}

open_enum! {
    /// 被控端的防窥模式（见模块说明的"防窥模式"）。不认识的值（对方版本新增的）控制端当作不支持
    pub enum PrivacyState {
        /// 这台被控端还不能开：Windows 没有装虚拟显示器驱动，或者 Airhand 不是为所有用户安装的；
        /// macOS 没有允许 Airhand 使用"辅助功能"（挡住本机的键盘鼠标要用到）
        Unavailable = 0 => "unavailable",
        /// 被控端的设置不允许
        NotAllowed = 1 => "notAllowed",
        /// 能开，现在关着
        Off = 2 => "off",
        /// 正在打开（Windows 换成虚拟显示器要几秒钟）
        Starting = 3 => "starting",
        /// 开着：本机的显示器不显示画面，本机的键盘鼠标不起作用
        On = 4 => "on",
        /// 刚才没能打开（已经恢复原样），detail 给出原因
        Failed = 5 => "failed",
    }
}

open_enum! {
    /// 被控端的虚拟麦克风（见模块说明的"麦克风"）。不认识的值（对方版本新增的）控制端当作接不了
    pub enum MicState {
        /// 这台被控端的平台还不支持
        Unsupported = 0 => "unsupported",
        /// 支持，但没有可用的虚拟麦克风。Windows 被控端可以在打开转发时从 Steam 安装驱动
        NoDevice = 1 => "noDevice",
        /// 能接：控制端打开麦克风之后，声音放进虚拟麦克风
        Ready = 2 => "ready",
        /// 另一个控制端正在用它（同一时刻只给一个）
        Busy = 3 => "busy",
        /// 打不开（例如被别的程序独占了），被控端在重试
        Failed = 4 => "failed",
        /// Windows 上既没有可用的设备，也没找到 Steam
        SteamMissing = 5 => "steamMissing",
        /// 正在打开设备、安装驱动或等 Windows 发布声音端点
        Preparing = 6 => "preparing",
        /// Steam 在，但驱动包不可用、安装失败或设备没有出现。detail 给出原因
        SetupFailed = 7 => "setupFailed",
    }
}

/// 一端的直连候选地址和证书（见模块说明的"直连"）
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
pub struct DirectOffer {
    /// 第几轮（被控端发起，从 1 开始；控制端回答时照抄）
    #[n(0)]
    pub round: u32,
    /// 本端的地址：服务器看到的公网地址、局域网地址……（最多 [`MAX_DIRECT_ADDRS`] 个）
    #[n(1)]
    pub addrs: Vec<SocketAddr>,
    /// 本端直连用的 TLS 证书（临时生成的、自签名的）的 SHA-256：对面只认它
    #[cbor(n(2), with = "minicbor::bytes")]
    pub cert: [u8; 32],
}

/// [`DirectOffer`] 里最多这么多个地址（多出来的不用）
pub const MAX_DIRECT_ADDRS: usize = 8;

impl HostMsg {
    /// 这条消息走哪条通道（见模块说明的"走哪条通道"）
    pub fn lane(&self) -> SessionLane {
        match self {
            HostMsg::Files(Some(m)) => m.lane(),
            HostMsg::CursorImage { .. } | HostMsg::Clipboard { .. } => SessionLane::Bulk,
            HostMsg::Files(None) => SessionLane::Control,
            HostMsg::Hello(_)
            | HostMsg::Displays { .. }
            | HostMsg::Notice(_)
            | HostMsg::Encoder(_)
            | HostMsg::Cursor { .. }
            | HostMsg::Bye(_)
            | HostMsg::Direct(_)
            | HostMsg::Mic { .. }
            | HostMsg::Privacy { .. } => SessionLane::Control,
        }
    }
}

/// 第 0 路（只看一个显示器时）的这一项不写进线上的数组（见模块说明的"多屏"）
pub(crate) fn is_zero(v: &u8) -> bool {
    *v == 0
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
        // 2–4：以前请本机用户同意时的拒绝 / 没人回答 / 请求太多，已经去掉了，编号不再用
    }
}

open_enum! {
    /// 被控端的光标显示成什么样（见模块说明的"鼠标"）。
    /// 不认识的值（对方版本新增的）控制端按 `Shown` 对待：照常显示本地光标、发绝对坐标
    pub enum CursorVisibility {
        /// 隐藏了（游戏里转视角等）：控制端改发相对移动
        Hidden = 0 => "hidden",
        /// 显示着：控制端用本地光标显示它的形状
        Shown = 1 => "shown",
        /// 显示着，已经画在画面里了（软件光标）：控制端照常发绝对坐标，本地光标显示不显示看控制端的设置
        InFrame = 2 => "inFrame",
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
    /// 第几路画面的编码器（见模块说明的"多屏"）
    #[cbor(n(5), skip_if = "is_zero")]
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stream: u8,
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
    /// 第 0 路画面是哪个显示器（`displays` 里的序号）
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
    /// 每一路画面是哪个显示器（见 [`HostMsg::Displays::shown`]）
    #[cbor(n(9), default)]
    pub shown: Vec<u8>,
    /// 最多能同时发几路画面（同时看几个显示器，见模块说明的"多屏"）。不超过 [`crate::MAX_STREAMS`]；0 和 1 一样：
    /// 只能看一个
    #[cbor(n(10), default)]
    pub streams: u8,
}

impl HostHello {
    /// 最多能同时看几个显示器（至少 1 个，不超过 [`crate::MAX_STREAMS`]）
    pub fn max_streams(&self) -> u8 {
        self.streams.clamp(1, crate::MAX_STREAMS)
    }
}

/// 一路画面看哪个显示器（[`ControllerMsg::View`]，见模块说明的"多屏"）。JSON（界面传来的）是 camelCase
#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StreamView {
    /// 显示器的序号（[`DisplayInfo::index`]）
    #[n(0)]
    pub display: u8,
    /// 这一路画面的短边上限：控制端按它显示的大小算的（0 = 不限）。被控端和画质设置里的
    /// [`QualitySettings::max_short`] 取较严的
    #[cbor(n(1), default)]
    #[serde(default)]
    pub max_short: u16,
}

impl StreamView {
    /// 第 `display` 个显示器，大小只看画质设置
    pub fn display(display: u8) -> Self {
        Self {
            display,
            max_short: 0,
        }
    }
}

/// 两个短边上限取较严的（0 = 不限）
pub fn stricter_short(a: u16, b: u16) -> u16 {
    match (a, b) {
        (0, x) | (x, 0) => x,
        (a, b) => a.min(b),
    }
}

open_enum! {
    /// 被控端的平台。控制端不认识的（对方版本新增的平台）当作一台普通的电脑（[`Platform::or_linux`]）
    pub enum Platform {
        Windows = 0 => "windows",
        Mac = 1 => "mac",
        Linux = 2 => "linux",
        Android = 3 => "android",
        /// iPhone / iPad：只能被看（iOS 不允许别的应用注入触摸），屏幕由 ReplayKit 的广播扩展采集
        Ios = 4 => "ios",
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

/// 控制端 → 被控端。变体的编号是线上的类型，不能改、不能复用。走哪条通道见模块说明
#[derive(Encode, Decode, Debug, Clone)]
pub enum ControllerMsg {
    /// 批量输入事件（鼠标移动会被合并）。不认识的事件（对方版本新增的）被控端跳过，同一批里的其它照常执行。
    ///
    /// `t`：控制端收到这批输入的时刻（微秒，和 [`ControllerDatagram::Ping::t`] 同一个时钟）。
    /// 被控端据此丢掉晚到太久的输入（见模块说明的"过时的输入"）
    #[n(1)]
    Input {
        #[n(0)]
        t: u64,
        #[cbor(n(1), with = "crate::cbor::open_vec")]
        events: Vec<InputEvent>,
    },
    /// 画质设置变了（开始时的设置在握手里，见 [`crate::handshake::ControllerIntro`]）
    #[n(3)]
    Settings(#[n(0)] QualitySettings),
    /// 要一个关键帧。`after`：控制端收到的这一路的最后一帧的序号（还没收到过时为 0），被控端在这之后发过关键帧就不再编
    /// （见模块说明的"关键帧"）
    #[n(4)]
    RequestKeyframe {
        #[n(0)]
        after: u32,
        /// 第几路画面（见模块说明的"多屏"）
        #[cbor(n(1), skip_if = "is_zero")]
        stream: u8,
    },
    /// 只看第 `index` 个显示器（[`ControllerMsg::View`] 的简便说法，这一路的大小只看画质设置）
    #[n(5)]
    SelectDisplay {
        #[n(0)]
        index: u8,
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
    /// 控制端的直连候选地址，回答 [`HostMsg::Direct`]（见模块说明的"直连"）
    #[n(9)]
    Direct(#[n(0)] DirectOffer),
    /// 打开 / 关闭控制端的麦克风；打开之后的声音走声音数据报。
    #[n(10)]
    Mic {
        #[n(0)]
        enabled: bool,
    },
    /// 看哪几个显示器：第 `i` 项是第 `i` 路画面（见模块说明的"多屏"）。空的是只看主显示器
    #[n(11)]
    View {
        #[n(0)]
        streams: Vec<StreamView>,
    },
    /// 剪贴板里的文件（见模块说明的"剪贴板里的文件"）。
    /// 不认识的（对方版本新增的）是 `None`：跳过
    #[n(12)]
    Files(#[cbor(n(0), with = "crate::cbor::opt", has_nil)] Option<FileMsg>),
    /// 打开 / 关闭防窥模式（见模块说明的"防窥模式"）。被控端不支持时什么也不做
    #[n(13)]
    Privacy {
        #[n(0)]
        on: bool,
    },
}

impl ControllerMsg {
    pub fn lane(&self) -> SessionLane {
        match self {
            Self::Files(Some(m)) => m.lane(),
            Self::Files(None) => SessionLane::Control,
            Self::Clipboard { .. } => SessionLane::Bulk,
            Self::Input { .. }
            | Self::Settings(_)
            | Self::RequestKeyframe { .. }
            | Self::SelectDisplay { .. }
            | Self::View { .. }
            | Self::Audio { .. }
            | Self::Direct(_)
            | Self::Mic { .. }
            | Self::Privacy { .. } => SessionLane::Control,
        }
    }
}

/// 控制端 → 被控端的控制数据报（见模块说明的"走哪条通道"）。变体的编号不能改、不能复用
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerDatagram {
    /// 每秒一次，被控端回 [`HostDatagram::Pong`]（也是保活）。`t` 是控制端的时钟（微秒），被控端也用它估算输入晚到了多久
    #[n(0)]
    Ping {
        #[n(0)]
        t: u64,
    },
    /// 界面收到了第 `seq` 帧（累积确认），用于被控端的在途控制和码率控制（防止延迟堆积）。
    ///
    /// `arrived`：这一帧的到达时刻（控制端的时钟，微秒，和 [`ControllerDatagram::Ping::t`] 同一个时钟）：
    /// 它开始从网络上到达（第一个字节、第一块到了）和前一帧交出去这两个时刻里晚的那个，也就是画面是一条有序流时
    /// 读到它的长度前缀的时刻（前面的帧在等重传时它也算晚到，见 `airhand_link::merge` 的"到达时刻"）。
    /// 确认要等界面收到才发，往返时延里混着界面那一段；到达时刻只算网络：被控端拿它减去自己的发送时刻，
    /// 两边时钟之差是常数，变化的部分就是路上的排队（这一帧自己要传多久不算在里面），两次到达之间收到的字节数
    /// 就是交付速率。
    ///
    /// 数据报会丢：丢的要是最后一个（画面停了，或者被控端的在途帧满了、正等着它），被控端就一直等。所以控制端在没有
    /// 新的确认时把最近的一个重发几次，之后随每次 Ping 再发一次；被控端收到重复的确认什么也不做
    #[n(1)]
    Ack {
        #[n(0)]
        seq: u32,
        #[n(1)]
        arrived: u64,
        /// 第几路画面（见模块说明的"多屏"）：每一路各自确认
        #[cbor(n(2), skip_if = "is_zero")]
        stream: u8,
    },
    /// 回答 [`HostDatagram::Probe`]：`t` 原样带回，从收到探测的那条路发回去
    #[n(2)]
    Echo {
        #[n(0)]
        t: u64,
    },
    /// 等不到一帧了（见模块说明的"帧流"）：控制端交给解码器的最后一帧是第 `decoded` 帧（还没有时为 0），
    /// 被控端从控制端有的参考帧恢复，控制端在恢复之前定时重报。
    #[n(3)]
    Lost {
        #[n(0)]
        decoded: u32,
        /// 第几路画面（见模块说明的"多屏"）：只恢复这一路
        #[cbor(n(1), skip_if = "is_zero")]
        stream: u8,
    },
    /// 最近一个报告周期内该路帧流的交付情况，供被控端调整码率。
    #[n(4)]
    Report {
        #[n(0)]
        report: VideoReport,
        #[cbor(n(1), skip_if = "is_zero")]
        stream: u8,
    },
}

/// 控制端多久报告一次画面收得怎么样（[`ControllerDatagram::Report`]）
pub const REPORT_EVERY: Duration = Duration::from_millis(500);

/// 一段时间里画面收得怎么样（[`ControllerDatagram::Report`]）。都是这段时间里的个数，不是累计的
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoReport {
    /// 从帧流上收完的帧
    #[n(0)]
    pub frames: u32,
    /// 其中等重传卡住过的：读到一半停住了一阵（丢了包，QUIC 只按顺序交出数据），后面的帧却先到了
    #[n(1)]
    pub stalled: u32,
}

impl VideoReport {
    /// 什么都没收到
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }
}

/// 被控端 → 控制端的控制数据报（见模块说明的"走哪条通道"）。变体的编号不能改、不能复用
#[derive(Encode, Decode, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDatagram {
    /// 回应 [`ControllerDatagram::Ping`]，用于测量端到端往返时延。
    #[n(0)]
    Pong {
        #[n(0)]
        t: u64,
    },
    /// 量一条路（中转、直连）的往返时延：直连通着时被控端定时在两条路上各发一个（`t` 是被控端的时钟，微秒），
    /// 控制端从收到它的那条路回 [`ControllerDatagram::Echo`]。画面、声音走哪条路据此决定（见 crate 说明的"直连"）
    #[n(1)]
    Probe {
        #[n(0)]
        t: u64,
    },
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
    /// 把被控端的光标画进画面里（控制端的游戏模式，见模块说明的"鼠标"）：光标显示着、在这一路的显示器上时画上去，
    /// 光标单独移动（画面没变）也出新的一帧。隐藏了就不画；已经是画面里的软件光标（[`CursorVisibility::InFrame`]）
    /// 也不再画一遍。不画（默认）不写进线上的数组
    #[cbor(n(5), skip_if = "is_false")]
    #[serde(default)]
    pub draw_cursor: bool,
}

/// 默认是 false 的开关不写进线上的数组（光标、介绍里的防窥模式）
pub(crate) fn is_false(v: &bool) -> bool {
    !*v
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate_kbps: 4_000,
            max_short: 1080,
            adaptive: true,
            codec: Codec::H264,
            draw_cursor: false,
        }
    }
}

impl QualitySettings {
    /// 范围校验；不认识的编码格式按 H.264、传输方式按自动处理
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
    /// 移到这个位置：归一化坐标（0..=1），相对第 `stream` 路画面（见模块说明的"多屏"）
    #[n(0)]
    MouseMove {
        #[n(0)]
        x: f32,
        #[n(1)]
        y: f32,
        #[cbor(n(2), skip_if = "is_zero")]
        #[serde(default, skip_serializing_if = "is_zero")]
        stream: u8,
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
                (
                    Some(InputEvent::MouseMove { x, y, stream }),
                    InputEvent::MouseMove {
                        x: nx,
                        y: ny,
                        stream: ns,
                    },
                ) => {
                    (*x, *y, *stream) = (nx, ny, ns);
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
        let mv = |x, y, stream| InputEvent::MouseMove { x, y, stream };
        let mut v = vec![
            mv(0.1, 0.1, 0),
            // 移到另一路画面上：只留最后一个，连同它在哪一路
            mv(0.2, 0.3, 1),
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
                mv(0.2, 0.3, 1),
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
        assert!(!q.draw_cursor);

        let report = VideoReport {
            frames: 60,
            stalled: 2,
        };
        for stream in [0, 3] {
            let r = ControllerDatagram::Report { report, stream };
            let back: ControllerDatagram = minicbor::decode(&minicbor::to_vec(r).unwrap()).unwrap();
            assert_eq!(back, r);
        }
        assert!(VideoReport::default().is_empty());

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
            shown: vec![0],
            streams: 9,
        };
        let back: HostHello = minicbor::decode(&minicbor::to_vec(&hello).unwrap()).unwrap();
        assert_eq!(back.codecs, hello.codecs);
        assert_eq!(back.platform.or_linux(), Platform::Linux);
        assert_eq!(back.actions, hello.actions);
        assert!(back.touch);
        assert_eq!(back.shown, [0]);
        // 最多几路不超过本端的上限；不说（0）就是只能看一个
        assert_eq!(back.max_streams(), crate::MAX_STREAMS);
        let one = HostHello { streams: 0, ..back };
        assert_eq!(one.max_streams(), 1);

        // 多屏：第 0 路的字段不写（和只有一路时的字节一样），别的路写上
        let ack = |stream| ControllerDatagram::Ack {
            seq: 5,
            arrived: 6,
            stream,
        };
        assert_eq!(minicbor::to_vec(ack(0)).unwrap(), [0x82, 1, 0x82, 5, 6]);
        for m in [
            ack(2),
            ControllerDatagram::Lost {
                decoded: 3,
                stream: 1,
            },
        ] {
            let back: ControllerDatagram = minicbor::decode(&minicbor::to_vec(m).unwrap()).unwrap();
            assert_eq!(back, m);
        }
        let view = ControllerMsg::View {
            streams: vec![
                StreamView::display(1),
                StreamView {
                    display: 0,
                    max_short: 720,
                },
            ],
        };
        let back: ControllerMsg = minicbor::decode(&minicbor::to_vec(&view).unwrap()).unwrap();
        assert!(
            matches!(back, ControllerMsg::View { ref streams } if streams.len() == 2 && streams[1].max_short == 720)
        );
        assert_eq!(view.lane(), SessionLane::Control);
        assert_eq!(stricter_short(0, 720), 720);
        assert_eq!(stricter_short(1080, 0), 1080);
        assert_eq!(stricter_short(1080, 720), 720);

        // 麦克风的状态：新版本加的状态留着原值（控制端当作接不了），没有说明的不带 detail
        let mic = HostMsg::Mic {
            state: MicState::Unknown(9),
            detail: None,
        };
        let back: HostMsg = minicbor::decode(&minicbor::to_vec(&mic).unwrap()).unwrap();
        assert!(matches!(
            back,
            HostMsg::Mic {
                state: MicState::Unknown(9),
                detail: None
            }
        ));
        assert_eq!(back.lane(), SessionLane::Control);
        assert_eq!(
            ControllerMsg::Mic { enabled: true }.lane(),
            SessionLane::Control
        );

        // 防窥模式：新版本加的状态留着原值（控制端当作不支持）
        let privacy = HostMsg::Privacy {
            state: PrivacyState::Unknown(42),
            detail: Some("x".into()),
        };
        let back: HostMsg = minicbor::decode(&minicbor::to_vec(&privacy).unwrap()).unwrap();
        assert!(matches!(
            back,
            HostMsg::Privacy { state: PrivacyState::Unknown(42), detail: Some(ref d) } if d == "x"
        ));
        assert_eq!(back.lane(), SessionLane::Control);
        let on = ControllerMsg::Privacy { on: true };
        assert_eq!(on.lane(), SessionLane::Control);
        let back: ControllerMsg = minicbor::decode(&minicbor::to_vec(&on).unwrap()).unwrap();
        assert!(matches!(back, ControllerMsg::Privacy { on: true }));

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
            draw_cursor: true,
        };
        assert_eq!(
            serde_json::to_value(q).unwrap(),
            json!({ "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "av1", "drawCursor": true })
        );
        // 省略光标设置时使用默认值
        let old: QualitySettings = serde_json::from_value(
            json!({ "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "av1" }),
        )
        .unwrap();
        assert!(!old.draw_cursor);
        let events = vec![
            InputEvent::MouseMove {
                x: 0.5,
                y: 0.25,
                stream: 0,
            },
            InputEvent::MouseMove {
                x: 0.0,
                y: 1.0,
                stream: 2,
            },
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
                { "type": "mouseMove", "x": 0.0, "y": 1.0, "stream": 2 },
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
        assert_eq!(serde_json::to_value(Platform::Ios).unwrap(), json!("ios"));
        assert_eq!(
            serde_json::to_value([MicState::NoDevice, MicState::Ready]).unwrap(),
            json!(["noDevice", "ready"])
        );
        assert_eq!(
            serde_json::to_value([PrivacyState::NotAllowed, PrivacyState::On]).unwrap(),
            json!(["notAllowed", "on"])
        );
        assert_eq!(
            serde_json::to_value(StreamView {
                display: 1,
                max_short: 720
            })
            .unwrap(),
            json!({ "display": 1, "maxShort": 720 })
        );
        assert_eq!(
            serde_json::from_value::<StreamView>(json!({ "display": 2 })).unwrap(),
            StreamView::display(2)
        );
    }
}
