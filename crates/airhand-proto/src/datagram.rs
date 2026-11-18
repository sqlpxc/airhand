//! QUIC 数据报：`会话 ID（u64 LE）| 种类 u8 | 密封单元`，没有流上的长度前缀。
//!
//! 中转只看会话 ID：发送方是这个会话的一端就原样转给另一端，否则丢掉。
//! 两条路使用同一格式，种类决定使用哪把端到端加密密钥（见 [`crate::handshake`]）。

/// 开头的会话 ID 和种类。
pub const HEADER: usize = 9;
/// 声音，至多包含三包 Opus（见 [`crate::media`]）。
pub const AUDIO: u8 = 0;
/// 控制数据报（Ping、Pong、画面确认、探测等）。
pub const CONTROL: u8 = 1;
/// 两段路径的 MTU 可能不一样，转发按同一个保守上限。
pub const MAX: usize = 1000;

pub fn header(session_id: u64, kind: u8) -> [u8; HEADER] {
    let mut h = [0; HEADER];
    h[..8].copy_from_slice(&session_id.to_le_bytes());
    h[8] = kind;
    h
}

pub fn session(d: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(*d.first_chunk::<8>()?))
}

pub fn kind(d: &[u8]) -> Option<u8> {
    d.get(8).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_layout() {
        let d = [8, 7, 6, 5, 4, 3, 2, 1, CONTROL, 9];
        assert_eq!(header(0x0102_0304_0506_0708, CONTROL), d[..HEADER]);
        assert_eq!(session(&d), Some(0x0102_0304_0506_0708));
        assert_eq!(kind(&d), Some(CONTROL));
        assert_eq!(session(&d[..7]), None);
        assert_eq!(kind(&d[..8]), None);
    }
}
