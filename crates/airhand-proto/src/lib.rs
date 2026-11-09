//! Airhand 协议定义。
//!
//! 整体结构：
//!
//! ```text
//!  被控端(Host) ──QUIC──┐                 ┌──QUIC── 控制端(Controller)
//!                      │  airhand-server │
//!   信令流(bi #0) ──────┤   信令 + 中转   ├────── 信令流(bi #0)
//!   会话流(bi #n) ══════╪═════ 字节透传 ══╪══════ 会话流(bi #n)
//! ```
//!
//! * 每条 QUIC 连接上客户端打开的**第一条**双向流是"信令流"，承载 [`signal`] 消息。
//! * 每个远控会话双方各打开三条双向流（[`SessionLane`]：控制、画面、声音），先写入 [`SessionHeader`]。
//!   服务器按会话 ID 和流的类型配对，然后**零拷贝透传**，不解析会话内容。
//! * 三条流各自有序，跨流没有顺序保证（音画用采集时间同步），一条流堵住不影响其它两条：
//!
//! | 流   | 方向                | 内容                                              | 编码 |
//! |------|---------------------|---------------------------------------------------|------|
//! | 控制 | 双向                | [`session::HostMsg`] / [`session::ControllerMsg`] | postcard（[`framing`]） |
//! | 画面 | 被控端 → 控制端     | 画面包                                            | 媒体包（[`media`]） |
//! | 声音 | 被控端 → 控制端     | 放不进数据报的声音包（很少见）                    | 媒体包（[`media`]） |
//!
//! 每条流上都是 `u32 LE 长度前缀 | 负载`。媒体包是定长头 + 原始数据，控制端原样交给界面，不重新编码。
//!
//! 声音包平时走 QUIC 数据报（[`datagram`]）：丢了就丢了，不重传，也不用排在丢了的包后面等重传。
//! 服务器按数据报开头的会话 ID 在这个会话的两端之间原样转发。

pub mod framing;
pub mod media;
pub mod session;
pub mod signal;

/// 协议版本，服务器与客户端不一致时拒绝连接（见 [`signal`] 模块的"版本握手"）。
///
/// 任何不兼容的改动（增删字段、调整枚举变体顺序、改媒体包的头……）都要把它加 1。
pub const PROTOCOL_VERSION: u16 = 9;

/// QUIC ALPN 标识。与版本握手一样永远不变，否则旧客户端连 TLS 握手都过不了，无法得知版本不一致。
pub const ALPN: &[u8] = b"airhand/1";

/// 控制码范围：6 位数字，首位不为 0，避免口头报码时产生歧义。
pub const CODE_MIN: u32 = 100_000;
pub const CODE_MAX: u32 = 999_999;

/// 信令流单条消息上限。
pub const MAX_SIGNAL_MSG: usize = 64 * 1024;
/// 会话控制流单条消息上限（最大的是 1 MiB 的剪贴板文本和光标形状）。
pub const MAX_CONTROL_MSG: usize = 2 * 1024 * 1024;

/// 会话流头部魔数。
pub const SESSION_MAGIC: [u8; 4] = *b"AHS1";

/// 连接角色。
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 被控端
    Host,
    /// 控制端
    Controller,
}

/// 会话的三条流（见模块说明）。画面丢包 / 积压不能堵住声音和控制消息。取值是线上的编号，不能改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionLane {
    Control = 0,
    Video = 1,
    Audio = 2,
}
impl SessionLane {
    pub const ALL: [Self; 3] = [Self::Control, Self::Video, Self::Audio];
}

/// 会话流头部（固定 14 字节）：`magic[4] | session_id u64 LE | role u8 | lane u8`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader {
    pub session_id: u64,
    pub role: Role,
    pub lane: SessionLane,
}

impl SessionHeader {
    pub const LEN: usize = 14;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..4].copy_from_slice(&SESSION_MAGIC);
        b[4..12].copy_from_slice(&self.session_id.to_le_bytes());
        b[12] = match self.role {
            Role::Host => 1,
            Role::Controller => 2,
        };
        b[13] = self.lane as u8;
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
        let lane = match b[13] {
            0 => SessionLane::Control,
            1 => SessionLane::Video,
            2 => SessionLane::Audio,
            _ => return None,
        };
        Some(Self {
            session_id,
            role,
            lane,
        })
    }
}

/// QUIC 数据报：`会话 ID（u64 LE）| 媒体包`（没有长度前缀，一个数据报就是一个包）。
///
/// 服务器只看开头的会话 ID：发送方是这个会话的一端，就原样转给另一端，否则丢掉。
/// 目前只有被控端的声音包走数据报（见 [`media`]）。
pub mod datagram {
    /// 开头的会话 ID
    pub const HEADER: usize = 8;
    /// 数据报最多这么大。被控端 → 服务器、服务器 → 控制端是两段路径，MTU 可能不一样：
    /// QUIC 保证任何路径上都放得下一千出头的数据报，按这个下限来，服务器转发时就不会因为太大而丢掉
    pub const MAX: usize = 1000;

    pub fn encode(session_id: u64, packet: &[u8]) -> Vec<u8> {
        let mut d = Vec::with_capacity(HEADER + packet.len());
        d.extend_from_slice(&session_id.to_le_bytes());
        d.extend_from_slice(packet);
        d
    }

    /// 数据报属于哪个会话；不到一个会话 ID 长时为 `None`
    pub fn session(d: &[u8]) -> Option<u64> {
        Some(u64::from_le_bytes(d.get(..HEADER)?.try_into().ok()?))
    }
}

/// 把控制码格式化为 `123 456` 形式，便于阅读。
pub fn format_code(code: u32) -> String {
    let s = format!("{code:06}");
    format!("{} {}", &s[..3], &s[3..])
}

/// 解析用户输入的控制码（允许空格/短横线分隔）。
pub fn parse_code(input: &str) -> Option<u32> {
    let digits: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let code: u32 = digits.parse().ok()?;
    (CODE_MIN..=CODE_MAX).contains(&code).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = SessionHeader {
            session_id: 0xdead_beef_1234,
            role: Role::Controller,
            lane: SessionLane::Video,
        };
        assert_eq!(SessionHeader::decode(&h.encode()), Some(h));
        let mut bad = h.encode();
        bad[0] = b'X';
        assert_eq!(SessionHeader::decode(&bad), None);
        let mut bad = h.encode();
        bad[13] = 3;
        assert_eq!(SessionHeader::decode(&bad), None);
    }

    #[test]
    fn datagram_layout() {
        let d = datagram::encode(0x0102_0304_0506_0708, &[9, 9]);
        assert_eq!(d, [8, 7, 6, 5, 4, 3, 2, 1, 9, 9]);
        assert_eq!(datagram::session(&d), Some(0x0102_0304_0506_0708));
        assert_eq!(datagram::session(&d[..7]), None);
    }

    #[test]
    fn code_parse() {
        assert_eq!(parse_code("123 456"), Some(123456));
        assert_eq!(parse_code("123-456"), Some(123456));
        assert_eq!(parse_code("012345"), None);
        assert_eq!(parse_code("12345"), None);
        assert_eq!(parse_code("12345a"), None);
        assert_eq!(format_code(123456), "123 456");
    }
}
