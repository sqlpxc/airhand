//! Airhand 协议定义。
//!
//! 整体结构：
//!
//! ```text
//!  被控端(Host) ──QUIC──┐                 ┌──QUIC── 控制端(Controller)
//!                      │  airhand-server │
//!   信令流(bi #0) ──────┤   信令 + 中转   ├────── 信令流(bi #0)
//!   会话流(bi #n) ══════╪═════ 字节透传 ══╪══════ 会话流(bi #n)
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
//! 服务器只检查信令版本（而且是区间：[`SIGNAL_MIN`]..=[`SIGNAL_VERSION`]），从不看会话内容：会话层怎么改都不用升级服务器。
//! 会话版本由两端在端到端握手里协商（握手是加密、认证过的，服务器改不了，也就没法让两端降级）。
//!
//! 版本号只在**没法兼容**的改动时加 1。加功能用能力位（[`Features`]）：两端（或者客户端和服务器）各自报上自己支持的，
//! 双方都支持才用。消息是带编号的 CBOR（见 [`framing`]）：不认识的消息、字段跳过，不认识的枚举值留着原值，不断开。
//! 端到端握手本身没有版本号：握手要改就加一个新的模式（[`handshake::Pattern`]）。
//!
//! # 会话
//!
//! * 每条 QUIC 连接上客户端打开的**第一条**双向流是"信令流"，承载 [`signal`] 消息。
//! * 每个远控会话双方各打开几条双向流（"通道"，[`SessionLane`]：控制、画面、声音、大块），先写入 [`SessionHeader`]。
//!   服务器按会话 ID 和通道编号配对，然后**零拷贝透传**。服务器不管会话有几条通道、各是什么：
//!   以后加通道（例如传文件）不用改服务器。
//! * 控制通道上先做端到端握手（[`handshake`]，Noise），之后每条通道上的每一帧、每个数据报都是加密的：
//!   服务器看不到画面、声音、键盘输入和剪贴板，也没法篡改、插入。
//! * 每条流各自有序，跨流没有顺序保证（音画用采集时间同步），一条流堵住不影响其它的：
//!
//! | 通道 | 方向                | 内容                                              | 编码 |
//! |------|---------------------|---------------------------------------------------|------|
//! | 控制 | 双向                | 小而急的 [`session::HostMsg`] / [`session::ControllerMsg`]：输入、确认、Ping…… | CBOR（[`framing`]） |
//! | 画面 | 被控端 → 控制端     | 画面包                                            | 媒体包（[`media`]） |
//! | 声音 | 被控端 → 控制端     | 放不进数据报的声音包（很少见）                    | 媒体包（[`media`]） |
//! | 大块 | 双向                | 大的消息：剪贴板、光标（见 [`session`] 的"走哪条通道"） | CBOR（[`framing`]） |
//!
//! 每条流上都是 `u32 LE 长度前缀 | 负载`，握手之后负载是密封单元 `密文 | 标签 | 计数`（见 [`handshake`] 的
//! "之后的每一帧"）。媒体包是定长头 + 原始数据，控制端解密后原样交给界面，不重新编码。
//!
//! 声音包平时走 QUIC 数据报（[`datagram`]）：丢了就丢了，不重传，也不用排在丢了的包后面等重传。
//! 服务器按数据报开头的会话 ID 在这个会话的两端之间原样转发。

pub mod cbor;
pub mod framing;

/// 消息的编码（带编号的 CBOR，见 [`framing`]）
pub use minicbor;
pub mod handshake;
pub mod media;
pub mod session;
pub mod signal;

/// 信令协议版本（客户端 ⇄ 服务器，包括 [`SessionHeader`] 和数据报的会话 ID）。
///
/// 只在没法兼容的改动时加 1（加功能用 [`features::signal`] 里的能力位）。服务器接受
/// [`SIGNAL_MIN`]..=[`SIGNAL_VERSION`] 之间的客户端（见 [`signal`] 模块的"版本握手"）。
pub const SIGNAL_VERSION: u16 = 9;
/// 本端还能说的最老的信令版本
pub const SIGNAL_MIN: u16 = 9;

/// 会话协议版本（被控端 ⇄ 控制端，服务器不看）。两端在端到端握手里取双方都支持的最高版本
/// （见 [`handshake`]）。只在没法兼容的改动时加 1（加功能用 [`features::session`] 里的能力位）。
pub const SESSION_VERSION: u16 = 1;
/// 本端还能说的最老的会话版本
pub const SESSION_MIN: u16 = 1;

/// QUIC ALPN 标识。与版本握手一样永远不变，否则旧客户端连 TLS 握手都过不了，无法得知版本不一致。
pub const ALPN: &[u8] = b"airhand/1";

/// 控制码范围：6 位数字，首位不为 0，避免口头报码时产生歧义。
pub const CODE_MIN: u32 = 100_000;
pub const CODE_MAX: u32 = 999_999;

/// 配对码的位数（被控端本机生成，从不经过服务器，见 [`handshake`] 的"配对"）
pub const PAIRING_DIGITS: usize = 6;

/// 信令流单条消息上限。
pub const MAX_SIGNAL_MSG: usize = 64 * 1024;
/// 会话控制通道单条消息上限，含加密开销（[`handshake::SEAL_OVERHEAD`]）。长输入分批，大消息走 Bulk。
pub const MAX_CONTROL_MSG: usize = 64 * 1024;
/// 大块通道单条消息上限（1 MiB 的剪贴板文本、光标图像），含加密开销。
pub const MAX_BULK_MSG: usize = 2 * 1024 * 1024;

/// 会话流头部魔数（"AHS1" 是 v9 之前的格式：固定三条通道）。
pub const SESSION_MAGIC: [u8; 4] = *b"AHS2";

/// 一个会话最多这么多条通道（服务器的资源上限，与通道是什么无关）
pub const MAX_LANES: u8 = 8;

/// 连接角色。
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 被控端
    Host,
    /// 控制端
    Controller,
}

/// 会话里已知的通道（见模块说明）。服务器不认识它们：线上只是一个编号（[`SessionHeader::lane`]），取值不能改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionLane {
    Control = 0,
    Video = 1,
    Audio = 2,
    /// 大的消息（剪贴板、光标）：不挡控制通道上的输入、确认（见 [`session`] 的"走哪条通道"）
    Bulk = 3,
}

impl SessionLane {
    pub const ALL: [Self; 4] = [Self::Control, Self::Video, Self::Audio, Self::Bulk];

    /// 一帧负载的资源上限（含加密开销，不含长度前缀），发送和接收共用。
    pub const fn max_frame_size(self) -> usize {
        match self {
            Self::Control => MAX_CONTROL_MSG,
            Self::Bulk => MAX_BULK_MSG,
            Self::Video => media::MAX_VIDEO_PACKET + handshake::SEAL_OVERHEAD,
            Self::Audio => media::MAX_AUDIO_PACKET + handshake::SEAL_OVERHEAD,
        }
    }

    /// 服务器往这条通道写数据时的优先级（越大越先发）：控制消息不能排在画面后面。
    /// 大块和画面一样：一样的优先级 QUIC 轮流发，1 MiB 的剪贴板不会把画面完全挡住，光标也不会排在一串画面后面
    pub const fn priority(self) -> u8 {
        match self {
            SessionLane::Control => 10,
            SessionLane::Audio => 5,
            SessionLane::Video | SessionLane::Bulk => 0,
        }
    }
}

/// 会话流头部（固定 16 字节）：`magic[4] | session_id u64 LE | role u8 | lane u8 | priority u8 | 0`。
/// 服务器不管会话有几条通道（最多 [`MAX_LANES`]），加通道不用升级服务器。
///
/// 服务器按 `(session_id, lane)` 把两端的流配对，往这一端写数据时用这里给的优先级。
/// 通道 0（控制）结束时整个会话结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader {
    pub session_id: u64,
    pub role: Role,
    /// 通道编号，小于 [`MAX_LANES`]
    pub lane: u8,
    /// 服务器往这一端写数据时的优先级（越大越先发）
    pub priority: u8,
}

impl SessionHeader {
    pub const LEN: usize = 16;

    pub fn new(session_id: u64, role: Role, lane: SessionLane) -> Self {
        Self {
            session_id,
            role,
            lane: lane as u8,
            priority: lane.priority(),
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..4].copy_from_slice(&SESSION_MAGIC);
        b[4..12].copy_from_slice(&self.session_id.to_le_bytes());
        b[12] = match self.role {
            Role::Host => 1,
            Role::Controller => 2,
        };
        b[13] = self.lane;
        b[14] = self.priority;
        b
    }

    pub fn decode(b: &[u8; Self::LEN]) -> Option<Self> {
        if b[..4] != SESSION_MAGIC {
            return None;
        }
        let session_id = u64::from_le_bytes(b[4..12].try_into().ok()?);
        let role = match b[12] {
            1 => Role::Host,
            2 => Role::Controller,
            _ => return None,
        };
        let lane = b[13];
        if lane >= MAX_LANES {
            return None;
        }
        Some(Self {
            session_id,
            role,
            lane,
            priority: b[14],
        })
    }
}

/// QUIC 数据报：`会话 ID（u64 LE）| 密封单元`（没有长度前缀，一个数据报就是一个包）。
///
/// 服务器只看开头的会话 ID：发送方是这个会话的一端，就原样转给另一端，否则丢掉。
/// 后面是端到端加密的媒体包，和流上的一帧一样是 `密文 | 标签 | 计数`（见 [`handshake`] 的"之后的每一帧"）。
/// 目前只有被控端的声音包走数据报（见 [`media`]），和声音流用同一把密钥、同一串计数。
pub mod datagram {
    /// 开头的会话 ID
    pub const HEADER: usize = 8;
    /// 数据报最多这么大。被控端 → 服务器、服务器 → 控制端是两段路径，MTU 可能不一样：
    /// QUIC 保证任何路径上都放得下一千出头的数据报，按这个下限来，服务器转发时就不会因为太大而丢掉
    pub const MAX: usize = 1000;

    /// 数据报属于哪个会话；不到一个会话 ID 长时为 `None`
    pub fn session(d: &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(d.get(..HEADER)?.try_into().ok()?))
    }
}

/// 能力位：一端支持哪些可选的功能。
///
/// 信令层（客户端 ⇄ 服务器，在版本握手里交换）和会话层（被控端 ⇄ 控制端，在端到端握手里交换）各有一套编号
/// （[`features::signal`]、[`features::session`]），用同一个类型。规则：
///
/// * 实际能用的 = 自己的 ∩ 对方的（[`Features::common`]）：只用双方都支持的；
/// * 不认识的位原样保留、不用管；
/// * 位永远不复用。某个功能在一次版本升级之后成了必备的，它的位就标成"保留"，不再分给别的功能；
/// * 只放**协议**的能力（懂不懂某条消息、某种格式）。设备的能力（有没有触摸屏、有哪些编码器）会变，
///   是会话消息里的数据（例如 [`session::HostHello`]），不是能力位。
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

/// 能力位的编号（见 [`Features`]）。v9 已有的功能都是必备的，不占位；以后的可选功能从这里开始分配。
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
        let h = SessionHeader::new(0xdead_beef_1234, Role::Controller, SessionLane::Video);
        assert_eq!(h.priority, 0);
        assert_eq!(SessionHeader::decode(&h.encode()), Some(h));
        let mut bad = h.encode();
        bad[0] = b'X';
        assert_eq!(SessionHeader::decode(&bad), None);
        // 服务器不认识的通道照样配对（以后加的通道），只要不超过上限
        let mut other = h.encode();
        other[13] = 5;
        assert_eq!(SessionHeader::decode(&other).map(|h| h.lane), Some(5));
        other[13] = MAX_LANES;
        assert_eq!(SessionHeader::decode(&other), None);
        let c = SessionHeader::new(1, Role::Host, SessionLane::Control);
        assert_eq!(c.encode()[12..], [1, 0, 10, 0]);
        let b = SessionHeader::new(1, Role::Controller, SessionLane::Bulk);
        assert_eq!(b.encode()[12..], [2, 3, 0, 0]);
        assert!(SessionLane::ALL.iter().all(|l| (*l as u8) < MAX_LANES));
    }

    #[test]
    fn datagram_layout() {
        let d = [8, 7, 6, 5, 4, 3, 2, 1, 9, 9];
        assert_eq!(datagram::session(&d), Some(0x0102_0304_0506_0708));
        assert_eq!(datagram::session(&d[..7]), None);
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
