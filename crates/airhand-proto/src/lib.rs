//! Airhand 协议定义。
//!
//! ```text
//!  被控端(Host) ──QUIC──┐                 ┌──QUIC── 控制端(Controller)
//!                      │  airhand-server │
//!   信令流(bi #0) ──────┤   信令 + 中转   ├────── 信令流(bi #0)
//!   通道(uni) ═════════╪═════ 字节透传 ══╪══════ 通道(uni)
//!                      │   （只看会话头）  │
//!               └──────── 端到端加密（Noise）────────┘
//! ```
//!
//! # 两层协议，各有各的版本
//!
//! | 层 | 谁解析 | 内容 | 版本 |
//! |----|--------|------|------|
//! | 信令 / 中转 | 服务器 | ALPN、版本握手、[`signal`] 消息、[`SessionHeader`]、数据报开头的会话 ID（[`datagram`]） | [`SIGNAL_VERSION`] |
//! | 会话 | 只有两端 | 端到端握手（[`handshake`]）、[`session`] 消息、媒体包（[`media`]） | [`SESSION_VERSION`] |
//!
//! 服务器只检查信令版本（区间 [`SIGNAL_MIN`]..=[`SIGNAL_VERSION`]），从不看会话内容：会话层怎么改都不用升级服务器。
//! 会话版本由两端在端到端握手里协商（加密、认证过，服务器改不了，也就没法让两端降级）。
//!
//! 版本号只在**没法兼容**的改动时加 1。加功能用能力位（[`Features`]）：双方都支持才用。消息是带编号的 CBOR
//! （见 [`framing`]）：不认识的消息、字段跳过，不认识的枚举值留着原值，不断开。端到端握手没有版本号：要改就加一个新的模式
//! （[`handshake::Pattern`]）。
//!
//! # 会话
//!
//! * 每条 QUIC 连接上客户端打开的第一条双向流是"信令流"，承载 [`signal`] 消息。控制端发起会话时把端到端握手的第一条消息
//!   一起交给服务器（[`signal::ClientMsg::Connect`]），服务器原样转给被控端（[`signal::ServerMsg::Incoming`]）。
//! * 会话的每条"通道"（[`SessionLane`]）是**单向流**：每端只为自己要发的通道开流，开头写 [`SessionHeader`]。
//!   服务器收到之后向另一端开一条单向流、写同样的头，然后**零拷贝透传**；它不配对、不管会话有几条通道、各是什么
//!   （只限制数量 [`MAX_LANES`]），以后加通道不用改服务器。任何一端的控制通道结束，整个会话结束。
//! * 信令 v10 起还有**帧流**（会话头带 [`SessionHeader::FRAME`]）：一条短的单向流只装一个单元（一帧画面），
//!   同一条通道可以开很多条，服务器只限制同时开着的条数（[`MAX_FRAME_STREAMS`]）。一帧丢了包只卡住它自己那一条流，
//!   后面的帧照样到；等不到的帧发送方可以整条放弃（`RESET_STREAM`），不再重传过时的数据（见 [`session`] 的"帧流"）。
//! * 端到端握手在控制通道上做完（[`handshake`]，Noise），之后每条通道上的每一帧、每个数据报都是加密的：
//!   服务器看不到、也没法篡改或插入。
//! * 每条流各自有序，跨流没有顺序保证（音画用采集时间同步），一条流堵住不影响其它的：
//!
//! | 通道 | 方向 | 内容 | 编码 |
//! |------|------|------|------|
//! | 控制 | 双向 | 小而急的 [`session::HostMsg`] / [`session::ControllerMsg`]：输入、设置、提示…… | CBOR（[`framing`]） |
//! | 画面 | 被控端 → 控制端 | 画面包（双方都支持 [`features::session::FRAMES`] 时每帧一条帧流）。多屏同时显示时每个显示器一路，
//! 各是一条通道（[`SessionLane::Video`]，见 [`session`] 的"多屏"） | 媒体包（[`media`]） |
//! | 大块 | 双向 | 大的消息：剪贴板、光标图像（见 [`session`] 的"走哪条通道"） | CBOR（[`framing`]） |
//!
//! 每条流上都是 `u32 LE 长度前缀 | 负载`，握手之后负载是密封单元 `密文 | 标签 | 计数`（见 [`handshake`] 的
//! "之后的每一帧"）。声音包，以及 Ping / Pong、画面确认、探测这些只看最新值的控制消息走 QUIC 数据报（[`datagram`]）：
//! 丢了就丢了，不重传，也不用排在丢了的包、流里别的消息后面等。画面的增量帧也可以走数据报，带着前向纠错的冗余
//! （[`fec`]，见 [`session`] 的"前向纠错"）。
//!
//! # 直连
//!
//! 会话的加密和传输无关，所以两端在中转之外再打通一条**直连**，同样的密文帧原样从直连发：
//!
//! * 服务器告诉每一端它看到的这一端自己的公网地址（[`signal::SessionGrant::observed`]、
//!   [`signal::ServerMsg::Incoming`] 的 `observed`）；
//! * 会话开始之后（被控端的用户同意了之后），两端在加密的控制通道上交换候选地址和各自直连证书的指纹
//!   （[`session::DirectOffer`]）：中转看不到局域网地址，还没被放进来的控制端拿不到被控端的地址；
//! * 两端用连中转的那个 UDP socket 打洞，控制端发起 QUIC 连接（ALPN [`DIRECT_ALPN`]）。TLS 双向认证只认对方在
//!   控制通道上给的证书指纹（和 WebRTC 一样），握手完成时就知道对面是谁，中间人冒充不了任何一边；
//! * 直连上每条通道也是一条单向流，开头同样是 [`SessionHeader`]；数据报的格式也和中转上一样。
//!
//! 两条路怎么分工：**控制通道和控制数据报两条路都发**（每一帧都写进中转，直连通着的话再从直连发一份，接收端按计数合并：
//! 输入、确认、Ping 总是走更快的那条，直连突然断了也不丢）；**画面、声音只走一条路**，被控端按两条路上探测到的往返时延
//! 选（直连可能比中转还慢，这时就留在中转上）；大块通道只走中转。探测（[`session::HostDatagram::Probe`]）和它的回声
//! 是只发在一条路上的控制数据报：每条路各探测一次，控制端从收到的那条路弹回去。
//!
//! 候选地址两种地址族都有：连中转的 socket 是双栈的，中转走 IPv4 时也能用两端的公网 IPv6 直连
//! （IPv6 一般没有 NAT，IPv4 的对称 NAT、运营商级 NAT 打不通时它往往通）。

pub mod cbor;
pub mod fec;
pub mod framing;

/// 消息的编码（带编号的 CBOR，见 [`framing`]）
pub use minicbor;
pub mod handshake;
pub mod media;
pub mod session;
pub mod signal;

use cbor::open_enum;

/// 信令协议版本（客户端 ⇄ 服务器，包括 [`SessionHeader`] 和数据报的会话 ID）。只在没法兼容的改动时加 1。
///
/// * 9：会话的每条通道每端开一次；
/// * 10：另有帧流（[`SessionHeader::FRAME`]）：同一条通道可以开很多条，每条装一帧。服务器说 10 的时候才转发帧流，
///   所以被控端只在和服务器商定了 10 的时候才用（9 的服务器会把同一条通道的第二条流拒掉）。
///   9 的客户端照样连得上 10 的服务器，什么都不用变
pub const SIGNAL_VERSION: u16 = 10;
/// 本端还能说的最老的信令版本
pub const SIGNAL_MIN: u16 = 9;
/// 从这个信令版本起服务器转发帧流（[`SessionHeader::FRAME`]）
pub const SIGNAL_FRAMES: u16 = 10;

/// 会话协议版本（被控端 ⇄ 控制端，服务器不看）。两端在端到端握手里取双方都支持的最高版本
pub const SESSION_VERSION: u16 = 1;
/// 本端还能说的最老的会话版本
pub const SESSION_MIN: u16 = 1;

/// QUIC ALPN 标识。和版本握手（见 [`signal`]）一样永远不变，否则不同版本连 TLS 握手都过不了，无法得知版本不一致
pub const ALPN: &[u8] = b"airhand/1";

/// 两端之间直连的 QUIC ALPN。直连只在会话里协商好之后才建，不用像 [`ALPN`] 那样永远不变
pub const DIRECT_ALPN: &[u8] = b"airhand-direct/1";

/// 控制码范围：6 位数字，首位不为 0，避免口头报码时产生歧义。
pub const CODE_MIN: u32 = 100_000;
pub const CODE_MAX: u32 = 999_999;

/// 配对码的位数（被控端本机生成，从不经过服务器，见 [`handshake`] 的"配对"）
pub const PAIRING_DIGITS: usize = 6;

/// 信令流单条消息上限。
pub const MAX_SIGNAL_MSG: usize = 64 * 1024;
/// 会话控制通道单条消息上限，含加密开销（[`handshake::SEAL_OVERHEAD`]）。长输入分批，大消息走大块通道
pub const MAX_CONTROL_MSG: usize = 64 * 1024;
/// 大块通道单条消息上限（1 MiB 的剪贴板文本、光标图像），含加密开销。
pub const MAX_BULK_MSG: usize = 2 * 1024 * 1024;

/// 会话头的魔数
pub const SESSION_MAGIC: [u8; 4] = *b"AHS9";

/// 一个会话每端最多这么多条通道（服务器的资源上限，与通道是什么无关）
pub const MAX_LANES: u8 = 8;

/// 一个会话每端最多同时开着这么多条帧流（[`SessionHeader::FRAME`]，服务器的资源上限）。
/// 被控端每一路画面在途的最多几十帧（见 `airhand_link::flow`），最多 [`MAX_STREAMS`] 路；放弃了的帧流随即关掉，
/// 用不到这么多
pub const MAX_FRAME_STREAMS: u16 = 256;

/// 一个会话最多同时有这么多路画面（多屏同时显示时每个显示器一路，见 [`session`] 的"多屏"）
pub const MAX_STREAMS: u8 = 4;

/// 会话头里的优先级最高这么多（控制通道的）：更大的值解码时压到它。服务器在端到端握手之前就转发通道，
/// 不压的话任何知道控制码的一端都能把自己的流标得比别的会话的控制通道还高
pub const MAX_PRIORITY: u8 = SessionLane::Control.priority();

open_enum! {
    /// 连接角色
    pub enum Role {
        /// 被控端
        Host = 0 => "host",
        /// 控制端
        Controller = 1 => "controller",
    }
}

/// 会话里已知的通道（见模块说明）。服务器不认识它们：线上只是一个编号（[`SessionHeader::lane`]，[`SessionLane::number`]），
/// 编号不能改
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionLane {
    /// 0 号
    Control,
    /// 画面的第几路（小于 [`MAX_STREAMS`]；只看一个显示器时只有第 0 路，见 [`session`] 的"多屏"）。
    /// 第 0 路是 1 号通道，之后的从 3 号起（第 1 路是 3 号）。每一路的密钥、计数各是各的
    Video(u8),
    /// 2 号：大的消息（剪贴板、光标图像）：不挡控制通道上的输入、确认（见 [`session`] 的"走哪条通道"）
    Bulk,
}

const _: () = assert!(
    SessionLane::COUNT <= MAX_LANES as usize,
    "the video streams don't fit in the lanes the server forwards"
);

impl SessionLane {
    /// 认识的通道有几条（编号是 `0..COUNT`）
    pub const COUNT: usize = 2 + MAX_STREAMS as usize;

    /// 线上的编号（`stream` 不小于 [`MAX_STREAMS`] 的画面没有编号：调用的一方不能这样用）
    pub const fn number(self) -> u8 {
        match self {
            Self::Control => 0,
            Self::Video(0) => 1,
            Self::Bulk => 2,
            Self::Video(stream) => {
                assert!(stream < MAX_STREAMS, "no such video stream");
                2 + stream
            }
        }
    }

    pub const fn from_u8(lane: u8) -> Option<Self> {
        match lane {
            0 => Some(Self::Control),
            1 => Some(Self::Video(0)),
            2 => Some(Self::Bulk),
            n if n < 2 + MAX_STREAMS => Some(Self::Video(n - 2)),
            _ => None,
        }
    }

    /// 认识的所有通道（按编号）
    pub fn all() -> impl Iterator<Item = Self> {
        (0..Self::COUNT as u8).filter_map(Self::from_u8)
    }

    /// 一帧负载的资源上限（含加密开销，不含长度前缀），发送和接收共用
    pub const fn max_frame_size(self) -> usize {
        match self {
            Self::Control => MAX_CONTROL_MSG,
            Self::Bulk => MAX_BULK_MSG,
            Self::Video(_) => media::MAX_VIDEO_PACKET + handshake::SEAL_OVERHEAD,
        }
    }

    /// 这条通道的流的优先级（越大越先发，服务器转发时也用它）：控制消息不能排在画面后面。
    /// 大块和画面一样：QUIC 轮流发，1 MiB 的剪贴板不会把画面完全挡住，光标图像也不会排在一串画面后面
    pub const fn priority(self) -> u8 {
        match self {
            Self::Control => 10,
            Self::Video(_) | Self::Bulk => 0,
        }
    }
}

/// 通道流的开头（固定 16 字节）：`magic[4] | session_id u64 LE | lane u8 | priority u8 | flags u8 | 0`。
///
/// 服务器按会话 ID 找到会话的另一端，向它开一条单向流、写同样的头，然后转发；往那一端写数据时用这里给的优先级
/// （不超过 [`MAX_PRIORITY`]）。直连上的通道流开头也是它（会话 ID 必须是这条直连的会话）。
///
/// `flags`（信令 v10 起，v9 写 0、不看）：[`SessionHeader::FRAME`] 表示这是一条帧流。别的位以后再分配，
/// 不认识的原样转发
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader {
    pub session_id: u64,
    /// 通道编号，小于 [`MAX_LANES`]
    pub lane: u8,
    /// 不超过 [`MAX_PRIORITY`]
    pub priority: u8,
    pub flags: u8,
}

impl SessionHeader {
    pub const LEN: usize = 16;

    /// 帧流：这条流只装一个单元（一帧画面），同一条通道可以开很多条（信令 v10，见 crate 说明）。
    /// 它结束了不代表通道结束了；控制通道不能是帧流
    pub const FRAME: u8 = 1;

    pub fn new(session_id: u64, lane: SessionLane) -> Self {
        Self {
            session_id,
            lane: lane.number(),
            priority: lane.priority(),
            flags: 0,
        }
    }

    /// 一条帧流的头（见 [`SessionHeader::FRAME`]）
    pub fn frame(session_id: u64, lane: SessionLane) -> Self {
        Self {
            flags: Self::FRAME,
            ..Self::new(session_id, lane)
        }
    }

    /// 是一条帧流
    pub fn is_frame(&self) -> bool {
        self.flags & Self::FRAME != 0
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..4].copy_from_slice(&SESSION_MAGIC);
        b[4..12].copy_from_slice(&self.session_id.to_le_bytes());
        b[12] = self.lane;
        b[13] = self.priority;
        b[14] = self.flags;
        b
    }

    pub fn decode(b: &[u8; Self::LEN]) -> Option<Self> {
        if b[..4] != SESSION_MAGIC || b[12] >= MAX_LANES {
            return None;
        }
        Some(Self {
            session_id: u64::from_le_bytes(b[4..12].try_into().ok()?),
            lane: b[12],
            priority: b[13].min(MAX_PRIORITY),
            flags: b[14],
        })
    }
}

/// QUIC 数据报：`会话 ID（u64 LE）| 种类 u8 | 密封单元`（没有长度前缀，一个数据报就是一个密封单元）。
///
/// 服务器只看开头的会话 ID：发送方是这个会话的一端，就原样转给另一端，否则丢掉。
/// 后面是端到端加密的内容（见 [`handshake`] 的"之后的每一帧"），种类决定用哪把密钥：
///
/// * [`AUDIO`]：声音：最新的一包和之前一两包的副本（见 [`media`] 的"声音数据报"）。被控端发的是它的系统声音，
///   控制端发的是它的麦克风（双方都支持 [`features::session::MIC`]、控制端打开了麦克风时，见 [`session`] 的"麦克风"）；
/// * [`CONTROL`]：控制数据报（[`session::ControllerDatagram`] / [`session::HostDatagram`]：Ping、Pong、画面确认、
///   量两条路往返时延的探测和回声），CBOR，和控制通道上的消息一样不认识的跳过；
/// * [`VIDEO`]：画面的一帧切出来的一块，或者它的冗余（见 [`fec`]）。和别的不一样，种类后面不是一个密封单元：
///   被控端先把整帧加密成一个密封单元（和帧流上的一样），再切开、算冗余，收的一方拼回密封单元再解密。
///   明文的头里有它是第几路画面（[`fec::Fragment::stream`]，多屏同时显示时）。
///
/// 直连上的数据报格式一样（会话 ID 用不着，但两条路上的包可以用同一段代码处理）。服务器不管种类，以后加种类不用升级服务器。
pub mod datagram {
    /// 开头的会话 ID 和种类
    pub const HEADER: usize = 9;
    /// 声音包
    pub const AUDIO: u8 = 0;
    /// 控制数据报
    pub const CONTROL: u8 = 1;
    /// 画面：纠错的一块（见 [`crate::fec`]）
    pub const VIDEO: u8 = 2;
    /// 数据报最多这么大。被控端 → 服务器、服务器 → 控制端是两段路径，MTU 可能不一样：
    /// QUIC 保证任何路径上都放得下一千出头的数据报，按这个下限来，服务器转发时就不会因为太大而丢掉
    pub const MAX: usize = 1000;

    /// 数据报属于哪个会话；不到一个会话 ID 长时为 `None`
    pub fn session(d: &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(d.get(..8)?.try_into().ok()?))
    }

    /// 数据报的种类（[`AUDIO`]、[`CONTROL`]、[`VIDEO`]）；不到一个头长时为 `None`
    pub fn kind(d: &[u8]) -> Option<u8> {
        d.get(8).copied()
    }
}

/// 能力位：一端支持哪些可选的功能。
///
/// 信令层（客户端 ⇄ 服务器，在版本握手里交换）和会话层（被控端 ⇄ 控制端，在端到端握手里交换）各有一套编号
/// （[`features::signal`]、[`features::session`]），用同一个类型。规则：
///
/// * 实际能用的 = 自己的 ∩ 对方的（[`Features::common`]）；
/// * 不认识的位原样保留、不用管；位永远不复用；
/// * 只放**协议**的能力（懂不懂某条消息、某种格式）。设备的能力（有没有触摸屏、有哪些编码器）是会话消息里的数据
///   （例如 [`session::HostHello`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Features(u64);

impl Features {
    pub const NONE: Features = Features(0);

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn with(self, other: Features) -> Self {
        Self(self.0 | other.0)
    }

    /// 包含 `f` 的所有位
    pub const fn has(self, f: Features) -> bool {
        self.0 & f.0 == f.0
    }

    /// 双方都支持的
    pub const fn common(self, peer: Features) -> Self {
        Self(self.0 & peer.0)
    }
}

impl<C> minicbor::Encode<C> for Features {
    fn encode<W: minicbor::encode::Write>(
        &self,
        e: &mut minicbor::Encoder<W>,
        _: &mut C,
    ) -> Result<(), minicbor::encode::Error<W::Error>> {
        e.u64(self.0)?;
        Ok(())
    }
}

impl<'b, C> minicbor::Decode<'b, C> for Features {
    fn decode(d: &mut minicbor::Decoder<'b>, _: &mut C) -> Result<Self, minicbor::decode::Error> {
        Ok(Self(d.u64()?))
    }
}

/// 能力位的编号（见 [`Features`]）。v9 的功能都是必备的，不占位；之后的可选功能从 0 号位开始分配
pub mod features {
    /// 信令层：客户端 ⇄ 服务器
    pub mod signal {
        use crate::Features;

        /// 本端支持的
        pub const SUPPORTED: Features = Features::NONE;
    }

    /// 会话层：被控端 ⇄ 控制端
    pub mod session {
        use crate::Features;

        /// 0 号位：控制端的麦克风（见 [`crate::session`] 的"麦克风"）。懂 [`crate::session::HostMsg::Mic`]、
        /// [`crate::session::ControllerMsg::Mic`] 和控制端发的声音数据报。被控端能不能真的接（有没有虚拟麦克风）
        /// 是 `HostMsg::Mic` 里的状态，不是能力位
        pub const MIC: Features = Features::from_bits(1 << 0);

        /// 1 号位：画面的帧流（见 [`crate::session`] 的"帧流"）。控制端收得了一帧一条的帧流、看得懂画面包头里的
        /// 参考帧（[`crate::media`] 的"参考帧"），会发 [`crate::session::ControllerDatagram::Lost`]；
        /// 被控端收到 `Lost` 会放弃在途的帧、从控制端有的帧恢复。被控端在中转上用帧流还要服务器说信令 v10
        /// （[`crate::SIGNAL_FRAMES`]）
        pub const FRAMES: Features = Features::from_bits(1 << 1);

        /// 2 号位：画面的前向纠错（见 [`crate::session`] 的"前向纠错"）。控制端收得了画面的纠错数据报
        /// （[`crate::datagram::VIDEO`]、[`crate::fec`]），会发 [`crate::session::ControllerDatagram::Report`]；
        /// 被控端看得懂 `Report`、[`crate::session::QualitySettings::transport`]。被控端只在画面用着帧流时用它
        /// （恢复要靠 [`FRAMES`] 的那一套）
        pub const FEC: Features = Features::from_bits(1 << 2);

        /// 本端支持的
        pub const SUPPORTED: Features = MIC.with(FRAMES).with(FEC);
    }
}

/// 把控制码格式化为 `123 456` 形式，便于阅读。
pub fn format_code(code: u32) -> String {
    let s = format!("{code:06}");
    format!("{} {}", &s[..3], &s[3..])
}

/// 解析用户输入的控制码（允许空格/短横线分隔）。
pub fn parse_code(input: &str) -> Option<u32> {
    let digits = digits_of(input)?;
    if digits.len() != 6 {
        return None;
    }
    let code: u32 = digits.parse().ok()?;
    (CODE_MIN..=CODE_MAX).contains(&code).then_some(code)
}

/// 解析用户输入的配对码：[`PAIRING_DIGITS`] 位数字（允许空格/短横线分隔），返回去掉分隔之后的数字
pub fn parse_pairing(input: &str) -> Option<String> {
    let digits = digits_of(input)?;
    (digits.len() == PAIRING_DIGITS).then_some(digits)
}

/// 去掉空格 / 短横线；还有别的字符时为 `None`
fn digits_of(input: &str) -> Option<String> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    digits.chars().all(|c| c.is_ascii_digit()).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = SessionHeader::new(0xdead_beef_1234, SessionLane::Video(0));
        assert_eq!(h.priority, 0);
        assert_eq!(SessionHeader::decode(&h.encode()), Some(h));
        let mut bad = h.encode();
        bad[0] = b'X';
        assert_eq!(SessionHeader::decode(&bad), None);
        // 服务器不认识的通道照样转发（以后加的通道），只要不超过上限
        let mut other = h.encode();
        other[12] = 5;
        assert_eq!(SessionHeader::decode(&other).map(|h| h.lane), Some(5));
        other[12] = MAX_LANES;
        assert_eq!(SessionHeader::decode(&other), None);
        let c = SessionHeader::new(1, SessionLane::Control);
        assert_eq!(c.encode()[12..], [0, 10, 0, 0]);
        // 比控制通道还高的优先级压到控制通道的
        let mut greedy = SessionHeader::new(1, SessionLane::Bulk).encode();
        greedy[13] = 255;
        assert_eq!(
            SessionHeader::decode(&greedy).map(|h| h.priority),
            Some(MAX_PRIORITY)
        );
        let b = SessionHeader::new(1, SessionLane::Bulk);
        assert_eq!(b.encode()[12..], [2, 0, 0, 0]);
        // 帧流：标志在第 14 个字节；不认识的标志原样留着（服务器照样转发）
        let f = SessionHeader::frame(1, SessionLane::Video(0));
        assert!(f.is_frame() && !b.is_frame());
        assert_eq!(f.encode()[12..], [1, 0, 1, 0]);
        assert_eq!(SessionHeader::decode(&f.encode()), Some(f));
        let mut newer = f.encode();
        newer[14] = 0x81;
        let n = SessionHeader::decode(&newer).unwrap();
        assert!(n.is_frame());
        assert_eq!(n.encode(), newer);
        // v9 写的头（第 14 个字节是 0）不是帧流
        assert!(!SessionHeader::decode(&h.encode()).unwrap().is_frame());
        for lane in SessionLane::all() {
            assert!(lane.number() < MAX_LANES);
            assert_eq!(SessionLane::from_u8(lane.number()), Some(lane));
        }
        assert_eq!(SessionLane::all().count(), SessionLane::COUNT);
        assert_eq!(SessionLane::from_u8(7), None);
        // 画面的第 0 路还是 1 号通道，之后的从 3 号起
        assert_eq!(SessionLane::Video(0).number(), 1);
        assert_eq!(SessionLane::Video(1).number(), 3);
        assert_eq!(
            SessionLane::from_u8(2 + MAX_STREAMS - 1),
            Some(SessionLane::Video(MAX_STREAMS - 1))
        );
        assert_eq!(SessionLane::from_u8(2 + MAX_STREAMS), None);
        assert_eq!(
            SessionHeader::new(1, SessionLane::Video(2)).encode()[12..],
            [4, 0, 0, 0]
        );
    }

    #[test]
    fn datagram_layout() {
        let d = [8, 7, 6, 5, 4, 3, 2, 1, datagram::CONTROL, 9];
        assert_eq!(datagram::session(&d), Some(0x0102_0304_0506_0708));
        assert_eq!(datagram::kind(&d), Some(datagram::CONTROL));
        assert_eq!(datagram::session(&d[..7]), None);
        assert_eq!(datagram::kind(&d[..8]), None);
    }

    #[test]
    fn features_rules() {
        let mine = Features::from_bits(0b1011);
        let peer = Features::from_bits(0b1110 | 1 << 40);
        assert_eq!(mine.common(peer), Features::from_bits(0b1010));
        assert!(mine.has(Features::from_bits(0b11)));
        assert!(!mine.has(Features::from_bits(0b100)));
        // 不认识的位原样保留
        let wire = minicbor::to_vec(peer).unwrap();
        assert_eq!(minicbor::decode::<Features>(&wire).unwrap(), peer);
    }

    #[test]
    fn code_parse() {
        assert_eq!(parse_code("123 456"), Some(123456));
        assert_eq!(parse_code("123-456"), Some(123456));
        assert_eq!(parse_code("012345"), None);
        assert_eq!(parse_code("12345"), None);
        assert_eq!(parse_code("12345a"), None);
        assert_eq!(format_code(123456), "123 456");
        assert_eq!(parse_pairing("012 345").as_deref(), Some("012345"));
        assert_eq!(parse_pairing("01234"), None);
        assert_eq!(parse_pairing("01234x"), None);
    }
}
