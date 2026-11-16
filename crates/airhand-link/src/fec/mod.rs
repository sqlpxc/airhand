//! 画面的前向纠错（见 `airhand_proto::session` 的"前向纠错"，数据报的格式见 `airhand_proto::fec`）：
//!
//! * [`fragments`]：被控端把一帧的密封单元切开、算出冗余（Reed-Solomon，`reed-solomon-simd`），变成一个个数据报；
//!   配几块冗余由 [`repair_count`] 按丢包率算；
//! * [`Reassembler`]：控制端把收到的数据报拼回密封单元，拼不回来的（到不齐了）报告出去，好马上请被控端恢复；
//! * [`Chooser`]：被控端决定每一帧走帧流还是纠错（自动时按控制端的报告切换）、配几块冗余。

mod choose;
mod reassemble;

use airhand_proto::fec::{Fragment, MAX_FRAME, PREV_FEC, max_repair, shard_size};
use anyhow::{Context, Result};
use bytes::Bytes;

use crate::secure::counter_of;

pub(crate) use choose::Chooser;
pub(crate) use reassemble::Reassembler;

/// 一帧补不回来的机会最多这么大（配冗余时的目标）
const TARGET: f64 = 0.005;
/// 丢包常常是一串一串的：按量到的丢包率的这么多倍配冗余
const MARGIN: f64 = 1.5;
/// 至少按这么大的丢包率配（还没量到丢包时也能补上偶尔丢的一块：小帧配一块冗余）
const MIN_LOSS: f64 = 0.005;
/// 最多按这么大的丢包率配
const MAX_LOSS: f64 = 0.5;

/// 把一帧的密封单元（`密文 | 标签 | 计数`，不带长度前缀）切开、配上 `r` 块冗余，变成数据报（见 `airhand_proto::fec`）。
/// `stream`：第几路画面；`prev_fec`：前一帧也是这样发的。冗余多于 [`max_repair`] 的按上限算
pub(crate) fn fragments(
    session_id: u64,
    stream: u8,
    sealed: &[u8],
    prev_fec: bool,
    r: usize,
) -> Result<Vec<Bytes>> {
    anyhow::ensure!(
        (1..=MAX_FRAME).contains(&sealed.len()),
        "a frame of {} bytes can't be sent as datagrams",
        sealed.len()
    );
    let counter = counter_of(sealed).context("not a sealed unit")?;
    let (size, k) = shard_size(sealed.len());
    let r = r.min(max_repair(k));
    // 最后一块补 0，块都一样长
    let mut padded = Vec::with_capacity(k * size);
    padded.extend_from_slice(sealed);
    padded.resize(k * size, 0);
    let originals: Vec<&[u8]> = padded.chunks(size).collect();
    let recovery = if r > 0 {
        reed_solomon_simd::encode(k, r, &originals)?
    } else {
        Vec::new()
    };
    let shards = originals
        .iter()
        .copied()
        .chain(recovery.iter().map(Vec::as_slice));
    Ok(shards
        .enumerate()
        .map(|(i, shard)| {
            let f = Fragment {
                counter,
                len: sealed.len() as u32,
                index: i as u16,
                k: k as u16,
                r: r as u16,
                flags: if prev_fec { PREV_FEC } else { 0 },
                stream,
                shard,
            };
            Bytes::from(f.encode(session_id))
        })
        .collect())
}

/// `k` 块原来的块、丢包率 `loss` 时配几块冗余：最少几块，`k + r` 块里丢了 `r` 块以上的机会不超过 [`TARGET`]
/// （丢包率按 [`MARGIN`] 倍、至少 [`MIN_LOSS`] 算）。至少 1 块，至多 [`max_repair`] 块
pub(crate) fn repair_count(k: usize, loss: f64) -> usize {
    let p = (loss * MARGIN).clamp(MIN_LOSS, MAX_LOSS);
    let fails = |r: usize| binomial_tail(k + r, r, p) > TARGET;
    let (mut lo, mut hi) = (1, max_repair(k));
    if fails(hi) {
        return hi;
    }
    // 冗余越多越不容易补不回来：找最少的够用的
    while lo < hi {
        let mid = (lo + hi) / 2;
        if fails(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// 二项分布 B(n, p) 大于 `r` 的概率。在对数里递推每一项（大的 `n` 不会在第一项就下溢成 0，
/// 太小的项下溢了也不影响结果）
fn binomial_tail(n: usize, r: usize, p: f64) -> f64 {
    if r >= n {
        return 0.0;
    }
    let (ln_p, ln_q) = (p.ln(), (1.0 - p).ln());
    let mut ln_pmf = n as f64 * ln_q;
    let mut cdf = ln_pmf.exp();
    for i in 0..r {
        ln_pmf += ((n - i) as f64 / (i + 1) as f64).ln() + ln_p - ln_q;
        cdf += ln_pmf.exp();
    }
    (1.0 - cdf).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::fec::MAX_SHARD;

    #[test]
    fn the_binomial_tail_is_right() {
        // 丢一块的机会：1 - (1 - p)^n
        let one = binomial_tail(10, 0, 0.1);
        assert!((one - (1.0 - 0.9f64.powi(10))).abs() < 1e-12);
        assert_eq!(binomial_tail(3, 3, 0.5), 0.0);
        // B(4, 0.5) > 2：1/16 + 4/16
        assert!((binomial_tail(4, 2, 0.5) - 5.0 / 16.0).abs() < 1e-12);
        // 很大的 n：第一项下溢也算得出来
        let big = binomial_tail(2000, 700, 0.3);
        assert!(big > 0.0 && big < 0.01, "{big}");
    }

    /// 冗余随块数、丢包率增加，不超过上限；配出来的真的够（按配冗余时的丢包率算）
    #[test]
    fn repairs_grow_with_loss_and_size() {
        assert_eq!(repair_count(1, 0.0), 1);
        assert_eq!(repair_count(10, 0.0), 1);
        assert_eq!(repair_count(50, 0.0), 2);
        // 大帧不丢包时也多配几块（冗余不到 3%）
        assert!(repair_count(200, 0.0) * 100 <= 200 * 3);
        let mut last = 0;
        for loss in [0.0, 0.01, 0.03, 0.1, 0.3] {
            let r = repair_count(50, loss);
            assert!(r >= last, "{loss}: {r}");
            last = r;
            let p = (loss * MARGIN).clamp(MIN_LOSS, MAX_LOSS);
            if r < max_repair(50) {
                assert!(binomial_tail(50 + r, r, p) <= TARGET);
                assert!(r == 1 || binomial_tail(50 + r - 1, r - 1, p) > TARGET);
            }
        }
        assert!(repair_count(100, 0.05) > repair_count(10, 0.05));
        // 丢得太多了：上限
        assert_eq!(repair_count(10, 0.9), max_repair(10));
        assert_eq!(repair_count(2, 0.9), 4);
    }

    /// 切开的数据报：一样长，原来的块拼起来就是密封单元（加上补的 0）；块数按 airhand_proto::fec::shard_size
    #[test]
    fn frames_become_fragments() {
        let (mut sealer, _) = crate::secure::test_lane(false);
        let sealed = sealer.seal(&vec![9u8; 2000]).unwrap();
        let d = fragments(7, 1, &sealed, true, 2).unwrap();
        let (size, k) = shard_size(sealed.len());
        assert_eq!(d.len(), k + 2);
        let parsed: Vec<_> = d.iter().map(|d| Fragment::parse(d).unwrap()).collect();
        assert!(
            parsed
                .iter()
                .all(|f| f.shard.len() == size && f.prev_fec() && f.stream == 1)
        );
        assert_eq!(parsed[0].counter, counter_of(&sealed).unwrap());
        let joined: Vec<u8> = parsed[..k].iter().flat_map(|f| f.shard).copied().collect();
        assert_eq!(&joined[..sealed.len()], &sealed[..]);
        assert!(joined[sealed.len()..].iter().all(|b| *b == 0));
        assert!(d.iter().all(|d| d.len() <= airhand_proto::datagram::MAX));
        // 太大的帧不行
        let huge = sealer.seal(&vec![0u8; MAX_FRAME]).unwrap();
        assert!(fragments(7, 0, &huge, false, 1).is_err());
        // 一个数据报装得下将近 1 KB
        assert!(parsed.iter().all(|f| f.shard.len() <= MAX_SHARD));
    }
}
