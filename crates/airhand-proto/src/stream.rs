//! 会话通道的流头、编号和传输策略。中转和直连使用同一格式，负载由会话端到端加密。

use crate::{MAX_BULK_MSG, MAX_CONTROL_MSG, files, handshake, media};

/// 一个会话最多同时有这么多路画面（每个显示器一路，见 [`crate::session`]）。
pub const MAX_STREAMS: u8 = 4;
/// 一个会话每端同时打开的视频帧流上限。
pub const MAX_FRAME_STREAMS: u16 = 256;
/// 一个会话每端同时打开的文件流上限。
pub const MAX_FILE_STREAMS: u16 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionLane {
    /// 一条常驻流，优先发送输入等小消息。
    Control,
    /// 一条常驻流，发送剪贴板等大消息。
    Bulk,
    /// 每帧一条流，第几路小于 [`MAX_STREAMS`]。
    Video(u8),
    /// 每次文件取用一条流。
    File,
}

impl SessionLane {
    const VIDEO_START: u8 = 2;
    const FILE: u8 = Self::VIDEO_START + MAX_STREAMS;
    pub const COUNT: usize = Self::FILE as usize + 1;

    pub const fn number(self) -> u8 {
        match self {
            Self::Control => 0,
            Self::Bulk => 1,
            Self::Video(stream) => {
                assert!(stream < MAX_STREAMS, "no such video stream");
                Self::VIDEO_START + stream
            }
            Self::File => Self::FILE,
        }
    }

    pub const fn from_u8(lane: u8) -> Option<Self> {
        match lane {
            0 => Some(Self::Control),
            1 => Some(Self::Bulk),
            Self::FILE => Some(Self::File),
            n if n < Self::FILE => Some(Self::Video(n - Self::VIDEO_START)),
            _ => None,
        }
    }

    /// 一帧负载的上限，含加密开销，不含长度前缀。
    pub const fn max_frame_size(self) -> usize {
        match self {
            Self::Control => MAX_CONTROL_MSG,
            Self::Bulk => MAX_BULK_MSG,
            Self::Video(_) => media::MAX_VIDEO_PACKET + handshake::SEAL_OVERHEAD,
            Self::File => files::CHUNK + handshake::SEAL_OVERHEAD,
        }
    }

    /// QUIC 发送优先级：控制不等画面，文件使用剩余带宽。
    pub const fn priority(self) -> i32 {
        match self {
            Self::Control => 10,
            Self::Bulk | Self::Video(_) => 1,
            Self::File => 0,
        }
    }
}

/// 单向流开头的 9 字节：`session_id u64 LE | lane u8`。
/// 流类型和发送优先级均由 [`SessionLane`] 决定，不在线上重复传输。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader {
    pub session_id: u64,
    pub lane: SessionLane,
}

impl SessionHeader {
    pub const LEN: usize = 9;

    pub const fn new(session_id: u64, lane: SessionLane) -> Self {
        Self { session_id, lane }
    }

    pub fn encode(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[..8].copy_from_slice(&self.session_id.to_le_bytes());
        b[8] = self.lane.number();
        b
    }

    pub fn decode(b: &[u8; Self::LEN]) -> Option<Self> {
        let session_id = u64::from_le_bytes(b[..8].try_into().unwrap());
        Some(Self::new(session_id, SessionLane::from_u8(b[8])?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_roundtrip_for_every_lane() {
        let id = 0x0102_0304_0506_0708;
        let mut b = [8, 7, 6, 5, 4, 3, 2, 1, 0];
        for n in 0..=u8::MAX {
            b[8] = n;
            let h = SessionHeader::decode(&b);
            if (n as usize) < SessionLane::COUNT {
                let h = h.unwrap();
                assert_eq!(h.session_id, id);
                assert_eq!(h.lane.number(), n);
                assert_eq!(h.encode(), b);
            } else {
                assert!(h.is_none());
            }
        }
    }
}
