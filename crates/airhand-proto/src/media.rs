//! 媒体包：画面通道上的帧、声音数据报里的包，以及控制端交给界面的光标。
//!
//! 控制消息不多，用 CBOR 编码（见 [`crate::framing`]）；画面和声音量大、格式固定，
//! 用一个头加原始数据，**同一份字节**从被控端一路传到控制端的界面：
//!
//! ```text
//! 线上（画面通道）：长度 u32 LE | 头（现在 24 字节）| 负载
//! 线上（声音数据报）：会话 ID | 种类 |（加密的）{ 长度 u32 LE | 头 | 负载 } × 1~3（见下面的"声音数据报"）
//! 交给界面（WebView / 浏览器 / Android）：  头（现在 24 字节）| 负载
//! ```
//!
//! 线上的包是加密的（见 [`crate::handshake`] 的"之后的每一帧"）：控制端读到一个包后原地解密，只看一眼头（统计、校验），
//! 不解码、不拷贝，原样交给界面；
//! 界面按同一个格式解析（`client/src/lib/media.ts`、`android/.../controller/Protocol.kt` 里的 `Media`，
//! 三边都用 `crates/airhand-link/tests/protocol.json` 里的样例检查）。
//! 光标（[`HostMsg::Cursor`](crate::session::HostMsg::Cursor) / [`HostMsg::CursorImage`](crate::session::HostMsg::CursorImage)）
//! 由控制端合并成同样格式的光标包（[`cursor_packet`]）交给界面。
//!
//! 头（小端）：
//!
//! | 偏移 | 类型    | 画面（1）            | 光标（2）                         | 声音（3）            |
//! |-----:|---------|----------------------|-----------------------------------|----------------------|
//! | 0    | u8      | 类型                 | 类型                              | 类型                 |
//! | 1    | u8      | 标志：bit0 关键帧    | bit0 可见，bit1 新形状，bit2 旧形状 | bit0 停顿后的第一包  |
//! | 2    | u8      | [`Codec`] 的编号     | 形状的槽位                        | 0                    |
//! | 3    | u8      | **头的长度**（现在是 24） | 同左                         | 同左                 |
//! | 4    | u32     | 帧序号               | 0                      | 包序号               |
//! | 8    | u16 × 2 | 画面宽、高           | 形状宽、高             | 0                    |
//! | 12   | u16 × 2 | 0                    | 热点 x、y              | 0                    |
//! | 16   | u64     | 采集时间（微秒）     | 0                      | 采集时间（微秒）     |
//! | 头的长度 |     | 码流（Annex-B / OBU）| RGBA（非预乘）         | 一个 Opus 包         |
//!
//! # 以后加字段
//!
//! 头的长度在第 3 个字节里（[`header_len`]）：以后要加字段（色彩空间、显示器编号……）就加在第 24 字节之后，
//! 把头变长，负载照样从"头的长度"处开始，旧的界面不认识新字段、跳过它们就行。
//!
//! 24 字节之后是一串扩展，每个是 `类型 u8 | 长度 u8 | 值`（TLV）：每个扩展自己说明有多长，
//! 新的扩展不依赖别的扩展在不在、排在第几，读的一方只取认识的类型。现在还没有扩展。所以读的一方必须：
//!
//! * 从第 3 个字节读头的长度、从那里开始取负载，**不能**写死 24；
//! * 忽略不认识的标志位、为 0 的保留字节（以后可能有值）、24 字节之后不认识的部分；
//! * 头的长度小于 24 或者超出了包的长度时丢掉这个包（[`Kind::of`] 为 `None`）。
//!
//! 现在的三种实现（这里、`client/src/lib/media.ts`、Android 的 `Media`）都按这个规则读。
//!
//! 采集时间是被控端的时钟，画面和声音是同一个时钟，控制端据此做音画同步。
//!
//! 光标的形状按槽位缓存（[`CURSOR_SLOTS`](crate::session::CURSOR_SLOTS)）：带新形状（bit1）的包后面是 RGBA，
//! 界面把它存进字节 2 给出的槽位并换成它；带旧形状（bit2）的包没有数据，换成这个槽位里已有的形状；
//! 都没有就是形状没变。槽位只在一个会话里有效，重连之后界面要清空。
//!
//! 编码格式不认识的画面包（[`VideoHeader::decode`] 返回 `None`）三边都丢掉，不猜成某种格式送进解码器。
//!
//! # 声音数据报
//!
//! 声音走 QUIC 数据报（[`crate::datagram`]）：丢了的不重传，后面的包也不用等它。丢了也要补得回来，所以每个数据报
//! 除了最新的一包，还带着之前一两包的副本（冗余，和 WebRTC 的 RED 一样）。数据报的明文（[`audio_datagram`] /
//! [`audio_packets`]）是一个或几个声音包，每个前面是 `u32 LE` 长度（和画面通道上的帧一样），按采集先后排列，
//! **最后一个是最新的**：
//!
//! * 连着丢了几个数据报，下一个到的时候把缺的那几包一起带来：只比原来晚 20、40 ms（抖动缓冲盖得住），不用等重传；
//!   中转、直连换路时，旧路上还没到的包也这样补上；
//! * 带几包由发的一方决定（放得下才带，见 [`audio_datagram`]），收的一方不管；静音之后的第一包（`start`）
//!   不带之前的：那是上一段声音的，早就过时了；
//! * 控制端按采集时刻（相同时按序号）只收比收过的都新的包：副本、乱序和迟到的包都在这里丢掉，交给界面的
//!   还是一个一个的声音包，界面不知道有冗余；
//! * 里面不是声音包的（以后的版本可能放别的）跳过；长度前缀不对的整个数据报丢掉。
//!
//! 一个包最多 [`MAX_AUDIO_PACKET`]（带上长度前缀、单独一包放得进一个数据报），被控端的 Opus 编码器按
//! [`MAX_OPUS_PACKET`] 限长，放不下的（只可能来自不能限长的编码器）丢掉，由解码器的丢包补偿盖过去。

use crate::session::{Codec, CursorShape};

/// 本版本写的媒体包头的长度，也是头最短的长度（读的时候按第 3 个字节，见 [`header_len`]）
pub const HEADER_LEN: usize = 24;
/// 画面包上限（一个 4K 关键帧也远小于这个值）
pub const MAX_VIDEO_PACKET: usize = 32 * 1024 * 1024;
/// 声音数据报的明文（[`audio_datagram`]）上限：加密之后正好放得进一个数据报（[`crate::datagram::MAX`]）
pub const MAX_AUDIO_DATAGRAM: usize =
    crate::datagram::MAX - crate::datagram::HEADER - crate::handshake::SEAL_OVERHEAD;
/// 声音包（头 + Opus 包）上限：带上长度前缀，单独一包放得进一个声音数据报
pub const MAX_AUDIO_PACKET: usize = MAX_AUDIO_DATAGRAM - crate::framing::PREFIX;
/// 一个 Opus 包最多这么大（被控端的编码器按它限长，见模块说明）
pub const MAX_OPUS_PACKET: usize = MAX_AUDIO_PACKET - HEADER_LEN;

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

/// 包的头有多长（第 3 个字节，见模块说明的"以后加字段"）：负载从这里开始。
/// 小于 [`HEADER_LEN`]、或者比包还长时为 `None`（不是合法的包）
pub fn header_len(packet: &[u8]) -> Option<usize> {
    let n = *packet.get(3)? as usize;
    (HEADER_LEN..=packet.len()).contains(&n).then_some(n)
}

/// 包的负载（头后面的部分）；不是合法的包时为 `None`
pub fn payload(packet: &[u8]) -> Option<&[u8]> {
    Some(&packet[header_len(packet)?..])
}

/// 一个新的头：类型和头的长度填好了，其余是 0
fn new_header(kind: Kind) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0] = kind as u8;
    h[3] = HEADER_LEN as u8;
    h
}

impl Kind {
    /// 包的类型；头不完整（见 [`header_len`]）、或者类型不认识时为 `None`
    pub fn of(packet: &[u8]) -> Option<Kind> {
        header_len(packet)?;
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
const FLAG_CACHED: u8 = 4;

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
    /// 会话内的帧序号，控制端收到后确认（[`ControllerDatagram::Ack`](crate::session::ControllerDatagram::Ack)）
    pub seq: u32,
    pub key: bool,
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub capture_us: u64,
}

impl VideoHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        debug_assert!(self.codec.is_known(), "encoding with an unknown codec");
        let mut h = new_header(Kind::Video);
        h[1] = if self.key { FLAG_KEY } else { 0 };
        h[2] = self.codec.code() as u8;
        h[4..8].copy_from_slice(&self.seq.to_le_bytes());
        h[8..10].copy_from_slice(&self.width.to_le_bytes());
        h[10..12].copy_from_slice(&self.height.to_le_bytes());
        h[16..24].copy_from_slice(&self.capture_us.to_le_bytes());
        h
    }

    /// 解析画面包的头；不是画面包或者编码格式不认识时为 `None`
    pub fn decode(packet: &[u8]) -> Option<Self> {
        Self::decode_any(packet).filter(|h| h.codec.is_known())
    }

    /// 解析公共头，保留未知编码。传输层仍可读取序号并交给界面确认，
    /// 只有实际解码器需要过滤编码格式，不能把未知格式误判成损坏的包。
    pub fn decode_any(packet: &[u8]) -> Option<Self> {
        if Kind::of(packet)? != Kind::Video {
            return None;
        }
        let codec = Codec::from_code(packet[2] as u32);
        Some(Self {
            seq: u32_at(packet, 4),
            key: packet[1] & FLAG_KEY != 0,
            codec,
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
        let mut h = new_header(Kind::Audio);
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

/// 拼一个声音数据报的明文（见模块说明的"声音数据报"）：最新的一包 `latest`，和它之前的几包 `earlier`
/// （副本，从新到旧）里放得下的（从新的开始，放不下就不再往前带）。包都不带长度前缀。
/// `latest` 单独都放不下（超过 [`MAX_AUDIO_PACKET`]）时为 `None`
pub fn audio_datagram<'a>(
    latest: &[u8],
    earlier: impl IntoIterator<Item = &'a [u8]>,
) -> Option<Vec<u8>> {
    let size = |p: &[u8]| crate::framing::PREFIX + p.len();
    let mut room = MAX_AUDIO_DATAGRAM.checked_sub(size(latest))?;
    let mut copies = Vec::new();
    for p in earlier {
        let Some(left) = room.checked_sub(size(p)) else {
            break;
        };
        room = left;
        copies.push(p);
    }
    let mut buf = Vec::with_capacity(MAX_AUDIO_DATAGRAM - room);
    for p in copies.into_iter().rev().chain([latest]) {
        buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
        buf.extend_from_slice(p);
    }
    Some(buf)
}

/// 解开声音数据报的明文（见模块说明的"声音数据报"）：里面的包，按采集先后排列（最后一个是最新的），不带长度前缀。
/// 长度前缀不对（超出了剩下的字节、超过 [`MAX_AUDIO_PACKET`]）、一个包都没有时为 `None`：整个数据报丢掉。
/// 包是什么不管（声音包之外的跳过、迟到的丢掉，由收的一方决定）
pub fn audio_packets(plain: &[u8]) -> Option<Vec<&[u8]>> {
    let mut packets = Vec::with_capacity(3);
    let mut rest = plain;
    while !rest.is_empty() {
        let (p, r) = crate::framing::split(rest, MAX_AUDIO_PACKET).ok()??;
        packets.push(p);
        rest = r;
    }
    (!packets.is_empty()).then_some(packets)
}

/// 交给界面的光标（[`cursor_packet`]）。由控制端合并线上的光标消息得到，不在线上传
#[derive(Debug, Clone, PartialEq)]
pub struct CursorState {
    pub visible: bool,
    /// 换成了哪个形状；`None` 表示形状没变
    pub shape: Option<CursorSlot>,
}

/// 光标换成了界面槽位 `slot` 里的形状（见 [`CURSOR_SLOTS`](crate::session::CURSOR_SLOTS)）
#[derive(Debug, Clone, PartialEq)]
pub struct CursorSlot {
    pub slot: u8,
    /// 新的形状：存进这个槽位（替换掉原来的）。`None` 表示槽位里已经是它了
    pub image: Option<CursorShape>,
}

/// 光标包（控制端交给界面，不在线上传）
pub fn cursor_packet(c: &CursorState) -> Vec<u8> {
    let image = c.shape.as_ref().and_then(|s| s.image.as_ref());
    let mut h = new_header(Kind::Cursor);
    h[1] = if c.visible { FLAG_VISIBLE } else { 0 };
    if let Some(s) = &c.shape {
        h[2] = s.slot;
        h[1] |= if image.is_some() {
            FLAG_SHAPE
        } else {
            FLAG_CACHED
        };
    }
    if let Some(s) = image {
        h[8..10].copy_from_slice(&s.width.to_le_bytes());
        h[10..12].copy_from_slice(&s.height.to_le_bytes());
        h[12..14].copy_from_slice(&s.hot_x.to_le_bytes());
        h[14..16].copy_from_slice(&s.hot_y.to_le_bytes());
    }
    let mut buf = Vec::with_capacity(HEADER_LEN + image.map_or(0, |s| s.rgba.len()));
    buf.extend_from_slice(&h);
    if let Some(s) = image {
        buf.extend_from_slice(&s.rgba);
    }
    buf
}

/// 线上的一个媒体包（加密之前）：长度前缀 + 头 + 负载。负载可以分几段给出（例如参数集 + 关键帧），一次拷贝拼好。
/// 多留了加密开销（标签和计数）的容量：原地加密时不用重新分配（画面包可能有几 MiB）。
/// 声音包也用它（发成数据报时和之前几包的副本拼在一起，见 [`audio_datagram`]），发送队列里画面、声音是同一种东西
pub fn frame(header: &[u8; HEADER_LEN], payload: &[&[u8]]) -> Vec<u8> {
    let len = HEADER_LEN + payload.iter().map(|p| p.len()).sum::<usize>();
    let mut buf = Vec::with_capacity(4 + len + crate::handshake::SEAL_OVERHEAD);
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

    #[test]
    fn unknown_video_codec_preserves_the_header_and_ack_sequence() {
        let mut packet = VideoHeader {
            seq: 42,
            key: true,
            codec: Codec::H264,
            width: 1920,
            height: 1080,
            capture_us: 100,
        }
        .encode()
        .to_vec();
        packet[2] = 99;
        let h = VideoHeader::decode_any(&packet).unwrap();
        assert_eq!(h.seq, 42);
        assert_eq!(h.codec, Codec::Unknown(99));
        assert!(VideoHeader::decode(&packet).is_none());
        assert!(VideoHeader::decode_any(&packet[..20]).is_none());
        packet[3] = 255;
        assert!(VideoHeader::decode_any(&packet).is_none());
    }

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
        assert_eq!(p[3], 24);
        assert_eq!(payload(p), Some(&[7, 7, 7, 8, 8][..]));
        // 编码格式不认识
        let mut bad = p.to_vec();
        bad[2] = 9;
        assert_eq!(VideoHeader::decode(&bad), None);
        assert_eq!(Kind::of(&p[..HEADER_LEN - 1]), None);
    }

    /// 以后的版本把头加长了：旧的读法照样找得到负载，不认识的部分跳过；头的长度不对的包丢掉
    #[test]
    fn longer_headers_from_a_newer_version() {
        let h = AudioHeader {
            seq: 3,
            start: false,
            capture_us: 77,
        };
        let mut p = h.encode().to_vec();
        p[3] = 32;
        p.extend_from_slice(&[0xee; 8]);
        p.extend_from_slice(&[1, 2, 3]);
        assert_eq!(AudioHeader::decode(&p), Some(h));
        assert_eq!(payload(&p), Some(&[1, 2, 3][..]));
        // 头比包还长、比 24 字节短
        p[3] = 200;
        assert_eq!((Kind::of(&p), payload(&p)), (None, None));
        p[3] = 23;
        assert_eq!(Kind::of(&p), None);
        // 保留的字节有了值也不影响
        let mut q = h.encode().to_vec();
        q[13] = 0x55;
        assert_eq!(AudioHeader::decode(&q), Some(h));
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
        assert_eq!(payload(&wire[4..]), Some(&[1, 2, 3][..]));
    }

    fn audio(seq: u32, start: bool, opus: usize) -> Vec<u8> {
        let h = AudioHeader {
            seq,
            start,
            capture_us: seq as u64 * 20_000,
        };
        frame(&h.encode(), &[&vec![seq as u8; opus]])[4..].to_vec()
    }

    fn seqs(plain: &[u8]) -> Vec<u32> {
        audio_packets(plain)
            .unwrap()
            .iter()
            .map(|p| AudioHeader::decode(p).unwrap().seq)
            .collect()
    }

    /// 声音数据报：最新的一包在最后，之前的副本按先后排在前面，放得下几包带几包
    #[test]
    fn audio_datagrams_carry_earlier_packets_that_fit() {
        let (a, b, c) = (
            audio(1, true, 240),
            audio(2, false, 240),
            audio(3, false, 240),
        );
        let only = audio_datagram(&c, []).unwrap();
        assert_eq!(only.len(), 4 + c.len());
        assert_eq!(audio_packets(&only).unwrap(), [&c[..]]);
        let all = audio_datagram(&c, [&b[..], &a[..]]).unwrap();
        assert_eq!(seqs(&all), [1, 2, 3]);
        assert_eq!(audio_packets(&all).unwrap(), [&a[..], &b[..], &c[..]]);
        assert!(all.len() <= MAX_AUDIO_DATAGRAM);

        // 放不下就不再往前带（从新的开始带）
        let big = audio(2, false, 600);
        let some = audio_datagram(&c, [&big[..], &a[..]]).unwrap();
        assert_eq!(seqs(&some), [2, 3]);
        let none = audio_datagram(&big, [&big[..]]).unwrap();
        assert_eq!(seqs(&none), [2]);

        // 最大的包单独正好放得下，再大就不行
        let max = audio(9, false, MAX_OPUS_PACKET);
        assert_eq!(max.len(), MAX_AUDIO_PACKET);
        assert_eq!(
            audio_datagram(&max, [&a[..]]).unwrap().len(),
            MAX_AUDIO_DATAGRAM
        );
        assert!(audio_datagram(&audio(9, false, MAX_OPUS_PACKET + 1), []).is_none());
    }

    /// 长度前缀不对的整个数据报丢掉；什么样的字节都不能让它崩溃
    #[test]
    fn malformed_audio_datagrams_are_dropped() {
        let plain = audio_datagram(&audio(2, false, 100), [&audio(1, false, 100)[..]]).unwrap();
        assert!(audio_packets(&[]).is_none());
        for cut in 1..plain.len() {
            if cut != 4 + audio(1, false, 100).len() {
                assert!(audio_packets(&plain[..cut]).is_none(), "{cut}");
            }
        }
        let mut huge = plain.clone();
        huge[..4].copy_from_slice(&(MAX_AUDIO_PACKET as u32 + 1).to_le_bytes());
        assert!(audio_packets(&huge).is_none());
        // 空的包、不是声音的包照样切出来（收的一方跳过）
        assert_eq!(audio_packets(&[0, 0, 0, 0]).unwrap(), [&[0u8; 0][..]]);
        for i in 0..plain.len() {
            let mut m = plain.clone();
            for b in [0x00, 0x01, 0x7f, 0xff] {
                m[i] = b;
                let _ = audio_packets(&m);
            }
        }
    }

    #[test]
    fn cursor_layout() {
        let p = cursor_packet(&CursorState {
            visible: true,
            shape: Some(CursorSlot {
                slot: 5,
                image: Some(CursorShape {
                    width: 2,
                    height: 1,
                    hot_x: 1,
                    hot_y: 0,
                    rgba: vec![9; 8],
                }),
            }),
        });
        assert_eq!(Kind::of(&p), Some(Kind::Cursor));
        assert_eq!(p[1], FLAG_VISIBLE | FLAG_SHAPE);
        assert_eq!(p[2], 5);
        assert_eq!(p[3], HEADER_LEN as u8);
        assert_eq!((u16_at(&p, 8), u16_at(&p, 10)), (2, 1));
        assert_eq!((u16_at(&p, 12), u16_at(&p, 14)), (1, 0));
        assert_eq!(&p[HEADER_LEN..], &[9; 8]);
        // 槽位里已有的形状：只有头
        let cached = cursor_packet(&CursorState {
            visible: true,
            shape: Some(CursorSlot {
                slot: 31,
                image: None,
            }),
        });
        assert_eq!(cached.len(), HEADER_LEN);
        assert_eq!((cached[1], cached[2]), (FLAG_VISIBLE | FLAG_CACHED, 31));
        let hidden = cursor_packet(&CursorState {
            visible: false,
            shape: None,
        });
        assert_eq!(hidden.len(), HEADER_LEN);
        assert_eq!(hidden[1], 0);
    }
}
