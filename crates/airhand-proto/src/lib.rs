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
//! * 端到端握手在控制通道上做完（[`handshake`]，Noise），之后每条通道上的每一帧、每个数据报都是加密的：
//!   服务器看不到、也没法篡改或插入。
//! * 每条流各自有序，跨流没有顺序保证（音画用采集时间同步），一条流堵住不影响其它的：
//!
//! | 通道 | 方向 | 内容 | 编码 |
//! |------|------|------|------|
//! | 控制 | 双向 | 小而急的 [`session::HostMsg`] / [`session::ControllerMsg`]：输入、确认、Ping…… | CBOR（[`framing`]） |
//! | 画面 | 被控端 → 控制端 | 画面包 | 媒体包（[`media`]） |
//! | 大块 | 双向 | 大的消息：剪贴板、光标图像（见 [`session`] 的"走哪条通道"） | CBOR（[`framing`]） |
//!
//! 每条流上都是 `u32 LE 长度前缀 | 负载`，握手之后负载是密封单元 `密文 | 标签 | 计数`（见 [`handshake`] 的
//! "之后的每一帧"）。声音包走 QUIC 数据报（[`datagram`]）：丢了就丢了，不重传，也不用排在丢了的包后面等。
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
//! 两条路怎么分工：**控制通道两条路都发**（每一帧都写进中转，直连通着的话再从直连发一份，接收端按计数合并：
//! 输入、确认、Ping 总是走更快的那条，直连突然断了也不丢）；**画面、声音只走一条路**，被控端按两条路上探测到的往返时延
//! 选（直连可能比中转还慢，这时就留在中转上）；大块通道只走中转。

pub mod cbor;
pub mod framing;

/// 消息的编码（带编号的 CBOR，见 [`framing`]）
pub use minicbor;
pub mod handshake;
pub mod media;
pub mod session;
pub mod signal;

use cbor::open_enum;

/// 信令协议版本（客户端 ⇄ 服务器，包括 [`SessionHeader`] 和数据报的会话 ID）。只在没法兼容的改动时加 1
pub const SIGNAL_VERSION: u16 = 9;
/// 本端还能说的最老的信令版本
pub const SIGNAL_MIN: u16 = 9;

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

open_enum! {
    /// 连接角色
    pub enum Role {
        /// 被控端
        Host = 0 => "host",
        /// 控制端
        Controller = 1 => "controller",
    }
}

/// 会话里已知的通道（见模块说明）。服务器不认识它们：线上只是一个编号（[`SessionHeader::lane`]），取值不能改
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SessionLane {
    Control = 0,
    Video = 1,
    /// 大的消息（剪贴板、光标图像）：不挡控制通道上的输入、确认（见 [`session`] 的"走哪条通道"）
    Bulk = 2,
}

impl SessionLane {
    pub const ALL: [Self; 3] = [Self::Control, Self::Video, Self::Bulk];

    pub const fn from_u8(lane: u8) -> Option<Self> {
        match lane {
            0 => Some(Self::Control),
            1 => Some(Self::Video),
            2 => Some(Self::Bulk),
            _ => None,
        }
    }

    /// 一帧负载的资源上限（含加密开销，不含长度前缀），发送和接收共用
    pub const fn max_frame_size(self) -> usize {
        match self {
            Self::Control => MAX_CONTROL_MSG,
            Self::Bulk => MAX_BULK_MSG,
            Self::Video => media::MAX_VIDEO_PACKET + handshake::SEAL_OVERHEAD,
        }
    }

    /// 这条通道的流的优先级（越大越先发，服务器转发时也用它）：控制消息不能排在画面后面。
    /// 大块和画面一样：QUIC 轮流发，1 MiB 的剪贴板不会把画面完全挡住，光标图像也不会排在一串画面后面
    pub const fn priority(self) -> u8 {
        match self {
            Self::Control => 10,
            Self::Video | Self::Bulk => 0,
        }
    }
}

/// 通道流的开头（固定 16 字节）：`magic[4] | session_id u64 LE | lane u8 | priority u8 | 0 | 0`。
///
/// 服务器按会话 ID 找到会话的另一端，向它开一条单向流、写同样的头，然后转发；往那一端写数据时用这里给的优先级。
/// 直连上的通道流开头也是它（会话 ID 必须是这条直连的会话）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader {
    pub session_id: u64,
    /// 通道编号，小于 [`MAX_LANES`]
    pub lane: u8,
    pub priority: u8,
}

impl SessionHeader {
    pub const LEN: usize = 16;

    pub fn new(session_id: u64, lane: SessionLane) -> Self {
        Self {
            session_id,
            lane: lane as u8,
            priority: lane.priority(),
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..4].copy_from_slice(&SESSION_MAGIC);
        b[4..12].copy_from_slice(&self.session_id.to_le_bytes());
        b[12] = self.lane;
        b[13] = self.priority;
        b
    }

    pub fn decode(b: &[u8; Self::LEN]) -> Option<Self> {
        if b[..4] != SESSION_MAGIC || b[12] >= MAX_LANES {
            return None;
        }
        Some(Self {
            session_id: u64::from_le_bytes(b[4..12].try_into().ok()?),
            lane: b[12],
            priority: b[13],
        })
    }
}

/// QUIC 数据报：`会话 ID（u64 LE）| 种类 u8 | 密封单元`（没有长度前缀，一个数据报就是一个包）。
///
/// 服务器只看开头的会话 ID：发送方是这个会话的一端，就原样转给另一端，否则丢掉。
/// 后面是端到端加密的内容（见 [`handshake`] 的"之后的每一帧"），种类决定用哪把密钥：
///
/// * [`AUDIO`]：被控端的声音包（见 [`media`]）；
/// * [`PROBE`]：量两条路（中转、直连）的往返时延的探测包（见模块说明的"直连"），两个方向各一把密钥。
///
/// 直连上的数据报格式一样（会话 ID 用不着，但两条路上的包可以用同一段代码处理）。
pub mod datagram {
    /// 开头的会话 ID 和种类
    pub const HEADER: usize = 9;
    /// 声音包
    pub const AUDIO: u8 = 0;
    /// 探测包
    pub const PROBE: u8 = 1;
    /// 数据报最多这么大。被控端 → 服务器、服务器 → 控制端是两段路径，MTU 可能不一样：
    /// QUIC 保证任何路径上都放得下一千出头的数据报，按这个下限来，服务器转发时就不会因为太大而丢掉
    pub const MAX: usize = 1000;

    /// 数据报属于哪个会话；不到一个会话 ID 长时为 `None`
    pub fn session(d: &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(d.get(..8)?.try_into().ok()?))
    }

    /// 数据报的种类（[`AUDIO`]、[`PROBE`]……）；不到一个头长时为 `None`
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

/// 能力位的编号（见 [`Features`]）。v9 的功能都是必备的，不占位；以后的可选功能从 0 号位开始分配
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

        /// 本端支持的
        pub const SUPPORTED: Features = Features::NONE;
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
        let h = SessionHeader::new(0xdead_beef_1234, SessionLane::Video);
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
        let b = SessionHeader::new(1, SessionLane::Bulk);
        assert_eq!(b.encode()[12..], [2, 0, 0, 0]);
        for lane in SessionLane::ALL {
            assert!((lane as u8) < MAX_LANES);
            assert_eq!(SessionLane::from_u8(lane as u8), Some(lane));
        }
        assert_eq!(SessionLane::from_u8(7), None);
    }

    #[test]
    fn datagram_layout() {
        let d = [8, 7, 6, 5, 4, 3, 2, 1, datagram::PROBE, 9];
        assert_eq!(datagram::session(&d), Some(0x0102_0304_0506_0708));
        assert_eq!(datagram::kind(&d), Some(datagram::PROBE));
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
