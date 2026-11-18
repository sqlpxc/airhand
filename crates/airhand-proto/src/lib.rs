//! Airhand v10 协议定义，只做编解码，不依赖异步运行时。
//!
//! # 两层协议
//!
//! | 层 | 谁解析 | 内容 | 版本 |
//! |----|--------|------|------|
//! | 信令 / 中转 | 中转服务器和客户端 | ALPN、[`signal`]、[`SessionHeader`]、数据报的会话 ID（[`datagram`]） | [`SIGNAL_VERSION`] |
//! | 会话 | 只有会话两端 | 端到端握手（[`handshake`]）、[`session`] 消息、媒体包（[`media`]） | [`SESSION_VERSION`] |
//!
//! 版本必须精确匹配，不协商区间或协议能力位。会话版本在端到端握手里校验；服务器不解析密文。
//!
//! # 传输
//!
//! * 客户端打开的第一条双向流是信令流，见 [`signal`]。发起会话时把第一条端到端握手消息一起转交给被控端。
//! * 会话的流都是单向流，开头写 [`SessionHeader`]。直连时直接发送；中转服务器只读会话 ID 和通道编号，查找另一端、
//!   设置发送优先级，并原样转发后续字节。任一端的控制通道结束，整个中转会话结束。
//! * 控制和大块通道各用一条常驻流；每帧画面、每次取用文件各开一条流。视频按路分配通道，见 [`SessionLane`]。
//!   遇到丢包时，一帧不会卡住后续帧；发送方可以 reset 过时帧而不再重传，见 [`session`] 的“帧流”。
//! * 流上每条消息是 `u32 LE 长度 | 负载`（[`framing`]）。端到端握手完成后，负载为 `密文 | 标签 | 计数`（[`handshake`]）。
//! * 声音和只关心最新值的控制消息（Ping、Pong、画面确认、探测）使用 QUIC 数据报，不重传（[`datagram`]）。
//!
//! # 直连
//!
//! 服务器把自己看到的公网地址分别告诉会话两端（[`signal::SessionGrant::observed`]、[`signal::ServerMsg::Incoming`]）。
//! 两端通过加密控制通道交换候选地址和证书指纹（[`session::DirectOffer`]），用同一 UDP socket 打洞，
//! 建立相互认证的 QUIC 连接（[`DIRECT_ALPN`]）。
//!
//! 控制帧和大部分控制数据报在中转与直连上发送同一份密文，接收端按计数去重。画面、声音走被控端选择的一条路；
//! 大块消息只走中转。探测和回声在同一条路上往返，为选路提供数据。

pub mod cbor;
pub mod datagram;
pub mod files;
pub mod framing;
pub mod handshake;
pub mod media;
pub mod session;
pub mod signal;
mod stream;

pub use minicbor;
pub use stream::{MAX_FILE_STREAMS, MAX_FRAME_STREAMS, MAX_STREAMS, SessionHeader, SessionLane};

use cbor::open_enum;

/// 信令及中转格式版本（客户端 ⇄ 服务器）。
pub const SIGNAL_VERSION: u16 = 10;
/// 端到端会话格式版本（被控端 ⇄ 控制端）。
pub const SESSION_VERSION: u16 = 1;

/// 信令 QUIC ALPN，具体版本由 Hello 校验。
pub const ALPN: &[u8] = b"airhand/1";
/// 会话内协商的直连 QUIC ALPN。
pub const DIRECT_ALPN: &[u8] = b"airhand-direct/1";

/// 控制码范围：6 位数字，首位不为 0。
pub const CODE_MIN: u32 = 100_000;
pub const CODE_MAX: u32 = 999_999;
/// 配对码位数，由被控端本机生成（见 [`handshake`]）。
pub const PAIRING_DIGITS: usize = 6;
/// 信令流单条消息上限。
pub const MAX_SIGNAL_MSG: usize = 64 * 1024;
/// 控制通道单帧上限，含加密开销。长输入分批，大消息走大块通道。
pub const MAX_CONTROL_MSG: usize = 64 * 1024;
/// 大块通道单帧上限，含加密开销。
pub const MAX_BULK_MSG: usize = 2 * 1024 * 1024;

open_enum! {
    /// 连接角色
    pub enum Role {
        Host = 0 => "host",
        Controller = 1 => "controller",
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

/// 解析用户输入的配对码：[`PAIRING_DIGITS`] 位数字（允许空格/短横线分隔），返回去掉分隔之后的数字。
pub fn parse_pairing(input: &str) -> Option<String> {
    let digits = digits_of(input)?;
    (digits.len() == PAIRING_DIGITS).then_some(digits)
}

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
