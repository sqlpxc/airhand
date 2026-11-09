//! 媒体包：画面流和声音流上的数据，以及控制端交给界面的光标。
//!
//! 控制流上的消息不多，用 postcard 编码（见 [`crate::framing`]）；画面和声音量大、格式固定，
//! 用一个 24 字节的定长头加原始数据，**同一份字节**从被控端一路传到控制端的界面：
//!
//! ```text
//! 线上（画面流 / 声音流）：长度 u32 LE | 头（24 字节）| 负载
//! 交给界面（WebView / 浏览器 / Android）：  头（24 字节）| 负载
//! ```
//!
//! 控制端读到一个包后只看一眼头（统计、校验），不解码、不拷贝，原样交给界面；
//! 界面按同一个格式解析（`client/src/lib/media.ts`、`android/.../controller/Protocol.kt` 里的 `Media`，
//! 三边都用 `crates/airhand-link/tests/protocol.json` 里的样例检查）。
//! 光标形状走控制流（[`HostMsg::Cursor`](crate::session::HostMsg::Cursor)），控制端把它转成同样格式的光标包。
//!
//! 头（小端）：
//!
//! | 偏移 | 类型    | 画面（1）            | 光标（2）              | 声音（3）            |
//! |-----:|---------|----------------------|------------------------|----------------------|
//! | 0    | u8      | 类型                 | 类型                   | 类型                 |
//! | 1    | u8      | 标志：bit0 关键帧    | bit0 可见，bit1 带形状 | bit0 停顿后的第一包  |
//! | 2    | u8      | [`Codec::id`]        | 0                      | 0                    |
//! | 3    | u8      | 0                    | 0                      | 0                    |
//! | 4    | u32     | 帧序号               | 0                      | 包序号               |
//! | 8    | u16 × 2 | 画面宽、高           | 形状宽、高             | 0                    |
//! | 12   | u16 × 2 | 0                    | 热点 x、y              | 0                    |
//! | 16   | u64     | 采集时间（微秒）     | 0                      | 采集时间（微秒）     |
//! | 24   |         | 码流（Annex-B / OBU）| RGBA（非预乘）         | 一个 Opus 包         |
//!
//! 采集时间是被控端的时钟，画面和声音是同一个时钟，控制端据此做音画同步。
//!
//! 编码格式不认识的画面包（[`VideoHeader::decode`] 返回 `None`）三边都丢掉，不猜成某种格式送进解码器。

use crate::session::{Codec, CursorState};

/// 媒体包头的长度
pub const HEADER_LEN: usize = 24;
/// 画面包上限（一个 4K 关键帧也远小于这个值）
pub const MAX_VIDEO_PACKET: usize = 32 * 1024 * 1024;
/// 声音包上限
pub const MAX_AUDIO_PACKET: usize = 64 * 1024;

/// 声音格式固定为 Opus 48 kHz 立体声，每包 20 ms。
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u8 = 2;
pub const AUDIO_FRAME_MS: u32 = 20;

/// 包的类型（头的第一个字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Video = 1,
    Cursor = 2,
    Audio = 3,
}

impl Kind {
    /// 包的类型；不到一个头长、或者类型不认识时为 `None`
    pub fn of(packet: &[u8]) -> Option<Kind> {
        if packet.len() < HEADER_LEN {
            return None;
        }
        match packet[0] {
            1 => Some(Kind::Video),
            2 => Some(Kind::Cursor),
            3 => Some(Kind::Audio),
            _ => None,
        }
    }
}

const FLAG_KEY: u8 = 1;
const FLAG_START: u8 = 1;
const FLAG_VISIBLE: u8 = 1;
const FLAG_SHAPE: u8 = 2;

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}

/// 画面包的头
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoHeader {
    /// 会话内的帧序号，控制端收到后确认（[`ControllerMsg::Ack`](crate::session::ControllerMsg::Ack)）
    pub seq: u32,
    pub key: bool,
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub capture_us: u64,
}

impl VideoHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = Kind::Video as u8;
        h[1] = if self.key { FLAG_KEY } else { 0 };
        h[2] = self.codec.id();
        h[4..8].copy_from_slice(&self.seq.to_le_bytes());
        h[8..10].copy_from_slice(&self.width.to_le_bytes());
        h[10..12].copy_from_slice(&self.height.to_le_bytes());
        h[16..24].copy_from_slice(&self.capture_us.to_le_bytes());
        h
    }

    /// 解析画面包的头；不是画面包或者编码格式不认识时为 `None`
    pub fn decode(packet: &[u8]) -> Option<Self> {
        if Kind::of(packet)? != Kind::Video {
            return None;
        }
        Some(Self {
            seq: u32_at(packet, 4),
            key: packet[1] & FLAG_KEY != 0,
            codec: Codec::from_id(packet[2])?,
            width: u16_at(packet, 8),
            height: u16_at(packet, 10),
            capture_us: u64_at(packet, 16),
        })
    }
}

/// 声音包的头
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// 包序号。没有声音时被控端不发包，序号也不递增
    pub seq: u32,
    /// 静音之后（或刚开始收听时）的第一包。控制端据此区分"声音停了"和"网络抖动"：前者不需要加大缓冲
    pub start: bool,
    /// 这一包解码后第一个采样的采集时间，已扣除 Opus 编码器的前瞻延迟
    pub capture_us: u64,
}

impl AudioHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = Kind::Audio as u8;
        h[1] = if self.start { FLAG_START } else { 0 };
        h[4..8].copy_from_slice(&self.seq.to_le_bytes());
        h[16..24].copy_from_slice(&self.capture_us.to_le_bytes());
        h
    }

    pub fn decode(packet: &[u8]) -> Option<Self> {
        if Kind::of(packet)? != Kind::Audio {
            return None;
        }
        Some(Self {
            seq: u32_at(packet, 4),
            start: packet[1] & FLAG_START != 0,
            capture_us: u64_at(packet, 16),
        })
    }
}

/// 光标包（控制端交给界面，不在线上传）
pub fn cursor_packet(c: &CursorState) -> Vec<u8> {
    let shape_len = c.shape.as_ref().map_or(0, |s| s.rgba.len());
    let mut h = [0u8; HEADER_LEN];
    h[0] = Kind::Cursor as u8;
    h[1] = if c.visible { FLAG_VISIBLE } else { 0 };
    if let Some(s) = &c.shape {
        h[1] |= FLAG_SHAPE;
        h[8..10].copy_from_slice(&s.width.to_le_bytes());
        h[10..12].copy_from_slice(&s.height.to_le_bytes());
        h[12..14].copy_from_slice(&s.hot_x.to_le_bytes());
        h[14..16].copy_from_slice(&s.hot_y.to_le_bytes());
    }
    let mut buf = Vec::with_capacity(HEADER_LEN + shape_len);
    buf.extend_from_slice(&h);
    if let Some(s) = &c.shape {
        buf.extend_from_slice(&s.rgba);
    }
    buf
}

/// 线上的一个媒体包：长度前缀 + 头 + 负载。负载可以分几段给出（例如参数集 + 关键帧），一次拷贝拼好
pub fn frame(header: &[u8; HEADER_LEN], payload: &[&[u8]]) -> Vec<u8> {
    let len = HEADER_LEN + payload.iter().map(|p| p.len()).sum::<usize>();
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    buf.extend_from_slice(header);
    for p in payload {
        buf.extend_from_slice(p);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::CursorShape;

    #[test]
    fn video_header_roundtrip() {
        let h = VideoHeader {
            seq: 9,
            key: true,
            codec: Codec::AV1,
            width: 1920,
            height: 1080,
            capture_us: 0x0102_0304_0506_0708,
        };
        let wire = frame(&h.encode(), &[&[7; 3], &[8; 2]]);
        assert_eq!(u32::from_le_bytes(wire[..4].try_into().unwrap()), 29);
        let p = &wire[4..];
        assert_eq!(Kind::of(p), Some(Kind::Video));
        assert_eq!(VideoHeader::decode(p), Some(h));
        assert_eq!(AudioHeader::decode(p), None);
        assert_eq!(&p[HEADER_LEN..], &[7, 7, 7, 8, 8]);
        // 编码格式不认识
        let mut bad = p.to_vec();
        bad[2] = 9;
        assert_eq!(VideoHeader::decode(&bad), None);
        assert_eq!(Kind::of(&p[..HEADER_LEN - 1]), None);
    }

    #[test]
    fn audio_header_roundtrip() {
        let h = AudioHeader {
            seq: 3,
            start: true,
            capture_us: 1 << 40,
        };
        let wire = frame(&h.encode(), &[&[1, 2, 3]]);
        assert_eq!(AudioHeader::decode(&wire[4..]), Some(h));
        assert_eq!(&wire[4 + HEADER_LEN..], &[1, 2, 3]);
    }

    #[test]
    fn cursor_layout() {
        let p = cursor_packet(&CursorState {
            visible: true,
            shape: Some(CursorShape {
                width: 2,
                height: 1,
                hot_x: 1,
                hot_y: 0,
                rgba: vec![9; 8],
            }),
        });
        assert_eq!(Kind::of(&p), Some(Kind::Cursor));
        assert_eq!(p[1], FLAG_VISIBLE | FLAG_SHAPE);
        assert_eq!((u16_at(&p, 8), u16_at(&p, 10)), (2, 1));
        assert_eq!((u16_at(&p, 12), u16_at(&p, 14)), (1, 0));
        assert_eq!(&p[HEADER_LEN..], &[9; 8]);
        let hidden = cursor_packet(&CursorState {
            visible: false,
            shape: None,
        });
        assert_eq!(hidden.len(), HEADER_LEN);
        assert_eq!(hidden[1], 0);
    }
}
