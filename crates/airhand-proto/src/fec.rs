//! 画面的纠错数据报（[`crate::datagram::VIDEO`]，见 [`crate::session`] 的"前向纠错"）的格式。这里只管格式：
//! 冗余怎么算（Reed-Solomon）、怎么拼回来在 `airhand_link::fec`。
//!
//! 被控端把一帧画面加密成一个密封单元（和帧流上的一样，`密文 | 标签 | 计数`，不带长度前缀），切成 `k` 块一样长的
//! （最后一块后面补 0），再算出 `r` 块冗余：这 `k + r` 块里收到任意 `k` 块就拼得回原来的 `k` 块
//! （Reed-Solomon，GF(2^16)，和 `reed-solomon-simd` 的编码一样，冗余的第 `i` 块就是它的第 `i` 个 recovery shard）。
//! 每块一个数据报：
//!
//! ```text
//! 会话 ID u64 | 种类 u8（= 2）| 计数 u64 | 长度 u32 | 第几块 u16 | k u16 | r u16 | 标志 u8 | 这一块（S 字节）
//! ```
//!
//! 都是小端：
//!
//! * 计数：这一帧的密封单元的计数（密封单元末尾也有）：哪些块是同一帧的；
//! * 长度：密封单元有多长（拼回来之后去掉补的 0）；
//! * 第几块：`0..k` 是原来的块，`k..k + r` 是冗余；
//! * 标志：bit0 [`PREV_FEC`]：前一帧（计数减 1）也是这样发的。前一帧的数据报一个都没到时，收的一方据此知道它不会再来了
//!   （不然它可能是一条还在路上的帧流）。别的位以后再分配，不认识的不管；
//! * 块长 S：同一帧的块一样长，就是数据报去掉头之后的长度。发的一方按 [`shard_size`] 定：偶数，不超过 [`MAX_SHARD`]，
//!   长度均分到每一块，最后一块补的 0 不多（一帧只有几百字节时数据报也只有那么长）。
//!
//! 头是明文：中转本来就看得到每一帧多大、计数是多少（帧流上的也一样）。密封单元整个拼回来才解密：
//! 中转改了哪一块，这一帧就解不开，和丢了一样（中转本来就能丢包）。
//!
//! 收的一方先检查（[`Fragment::parse`]）：块长是不超过 [`MAX_SHARD`] 的偶数；`k` 块正好盖住长度（最后一块不空）；
//! 长度不超过 [`MAX_FRAME`]；冗余不超过 [`max_repair`]；第几块在范围里。不对的整个丢掉。

use crate::datagram;

/// 种类后面的头有多长
pub const HEADER: usize = 19;
/// 整个头（会话 ID、种类和 [`HEADER`]）有多长
pub const FULL_HEADER: usize = datagram::HEADER + HEADER;
/// 一块最长多少字节：数据报正好是 [`datagram::MAX`]
pub const MAX_SHARD: usize = datagram::MAX - FULL_HEADER;
/// 密封单元最长多少字节：再大的帧（很少见，多半是关键帧）走帧流
pub const MAX_FRAME: usize = 1 << 20;
/// 一帧最多切成这么多块
pub const MAX_SHARDS: usize = MAX_FRAME.div_ceil(MAX_SHARD);
/// 标志：前一帧也是这样发的（见模块说明）
pub const PREV_FEC: u8 = 1;

const _: () = assert!(
    MAX_SHARD.is_multiple_of(2),
    "shards must have an even length"
);

/// `k` 块原来的块最多配这么多块冗余：一倍，块很少时最多 4 块
pub const fn max_repair(k: usize) -> usize {
    if k > 4 { k } else { 4 }
}

/// `len` 字节的密封单元怎么切：(块长, 块数)。`len` 至少 1
pub fn shard_size(len: usize) -> (usize, usize) {
    let len = len.max(1);
    let k = len.div_ceil(MAX_SHARD);
    let shard = len.div_ceil(k).next_multiple_of(2);
    (shard, len.div_ceil(shard))
}

/// 一个纠错数据报（见模块说明）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment<'a> {
    /// 这一帧的密封单元的计数
    pub counter: u64,
    /// 密封单元的长度
    pub len: u32,
    /// 第几块：`0..k` 是原来的块，之后是冗余
    pub index: u16,
    /// 原来的块有几块
    pub k: u16,
    /// 冗余有几块
    pub r: u16,
    /// [`PREV_FEC`] 等
    pub flags: u8,
    /// 这一块
    pub shard: &'a [u8],
}

impl<'a> Fragment<'a> {
    /// 前一帧也是这样发的（[`PREV_FEC`]）
    pub fn prev_fec(&self) -> bool {
        self.flags & PREV_FEC != 0
    }

    /// 是冗余的一块
    pub fn is_repair(&self) -> bool {
        self.index >= self.k
    }

    /// 写成数据报：头 + 这一块
    pub fn encode(&self, session_id: u64) -> Vec<u8> {
        let mut d = Vec::with_capacity(FULL_HEADER + self.shard.len());
        d.extend_from_slice(&session_id.to_le_bytes());
        d.push(datagram::VIDEO);
        d.extend_from_slice(&self.counter.to_le_bytes());
        d.extend_from_slice(&self.len.to_le_bytes());
        d.extend_from_slice(&self.index.to_le_bytes());
        d.extend_from_slice(&self.k.to_le_bytes());
        d.extend_from_slice(&self.r.to_le_bytes());
        d.push(self.flags);
        d.extend_from_slice(self.shard);
        d
    }

    /// 解析一个纠错数据报（整个数据报，从会话 ID 开始）。不是纠错数据报、格式不对的为 `None`（见模块说明）
    pub fn parse(d: &'a [u8]) -> Option<Self> {
        if datagram::kind(d)? != datagram::VIDEO || d.len() <= FULL_HEADER {
            return None;
        }
        let h = &d[datagram::HEADER..FULL_HEADER];
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        let f = Fragment {
            counter: u64::from_le_bytes(h[..8].try_into().ok()?),
            len: u32::from_le_bytes(h[8..12].try_into().ok()?),
            index: u16_at(12),
            k: u16_at(14),
            r: u16_at(16),
            flags: h[18],
            shard: &d[FULL_HEADER..],
        };
        let (len, k, r, s) = (f.len as usize, f.k as usize, f.r as usize, f.shard.len());
        let fits = s <= MAX_SHARD
            && s.is_multiple_of(2)
            && (1..=MAX_FRAME).contains(&len)
            && (1..=MAX_SHARDS).contains(&k)
            && (k - 1) * s < len
            && len <= k * s
            && r <= max_repair(k)
            && (f.index as usize) < k + r;
        fits.then_some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shards_cover_the_frame_evenly() {
        for len in [1, 2, 3, 100, 971, 972, 973, 1000, 1945, 50_000, MAX_FRAME] {
            let (s, k) = shard_size(len);
            assert!(s.is_multiple_of(2) && s <= MAX_SHARD, "{len}: {s}");
            assert!((k - 1) * s < len && len <= k * s, "{len}: {k} × {s}");
            assert!(k <= MAX_SHARDS);
            // 最后一块补的 0 不到一块的块数分之一（每块多补不到 2 字节）
            assert!(k * s - len < 2 * k, "{len}: {k} × {s}");
        }
        // 小帧的数据报也小
        assert_eq!(shard_size(101), (102, 1));
        assert_eq!(shard_size(MAX_SHARD), (MAX_SHARD, 1));
        assert_eq!(shard_size(MAX_SHARD + 1).1, 2);
    }

    #[test]
    fn fragments_roundtrip_and_bad_ones_are_dropped() {
        let shard = [7u8; 500];
        let f = Fragment {
            counter: 0x0102_0304_0506_0708,
            len: 999,
            index: 2,
            k: 2,
            r: 1,
            flags: PREV_FEC,
            shard: &shard,
        };
        let d = f.encode(42);
        assert_eq!(d.len(), FULL_HEADER + 500);
        assert_eq!(datagram::session(&d), Some(42));
        assert_eq!(datagram::kind(&d), Some(datagram::VIDEO));
        let back = Fragment::parse(&d).unwrap();
        assert_eq!(back, f);
        assert!(back.prev_fec() && back.is_repair());
        // 数据报正好是上限那么长
        let full = [0u8; MAX_SHARD];
        let big = Fragment {
            len: MAX_SHARD as u32,
            k: 1,
            r: 1,
            index: 0,
            shard: &full,
            ..f
        };
        assert_eq!(big.encode(1).len(), datagram::MAX);
        assert!(Fragment::parse(&big.encode(1)).is_some());

        let bad = |g: Fragment| Fragment::parse(&g.encode(1)).is_none();
        // 块长是奇数、太长
        assert!(bad(Fragment {
            shard: &shard[..499],
            ..f
        }));
        let huge = [0u8; MAX_SHARD + 2];
        assert!(bad(Fragment {
            shard: &huge,
            len: 1000,
            k: 1,
            ..f
        }));
        // k 块盖不住长度、最后一块是空的
        assert!(bad(Fragment { len: 1001, ..f }));
        assert!(bad(Fragment { len: 500, ..f }));
        // 冗余太多、第几块超出了范围、长度是 0
        assert!(bad(Fragment { r: 5, ..f }));
        assert!(bad(Fragment { index: 3, ..f }));
        assert!(bad(Fragment { len: 0, k: 0, ..f }));
        // 不是画面的数据报、只有头
        let mut audio = d.clone();
        audio[8] = datagram::AUDIO;
        assert!(Fragment::parse(&audio).is_none());
        assert!(Fragment::parse(&d[..FULL_HEADER]).is_none());
        // 不认识的标志位不管
        let newer = Fragment { flags: 0x80, ..f };
        assert!(!Fragment::parse(&newer.encode(1)).unwrap().prev_fec());
    }
}
