//! 控制端把画面的纠错数据报拼回密封单元（见 `airhand_proto::fec`）。
//!
//! 每一帧收到 `k` 块（原来的、冗余的随便哪几块）就拼得回来：原来的块都到了直接接起来，不然用 Reed-Solomon 补上缺的。
//! 拼好的密封单元和帧流上读到的一样，交给画面的合并（`crate::merge`）解密、排序。
//!
//! 数据报不重传，所以拼不回来的帧很快就能断定（[`Reassembler::poll`]）：被控端一帧一帧按顺序发，后一帧的数据报都到了，
//! 这一帧还缺着、又 [`GONE_AFTER`] 没有新的，它就到不齐了（留一点余量给路上的乱序）。后一帧走的是另一条路（换路了，
//! 旧路慢一点）、或者后面没有帧了（画面停了）时等 [`IDLE_GONE`]。前一帧一个数据报都没到的话，后一帧的标志
//! （`airhand_proto::fec::PREV_FEC`）说明它也是纠错发的，同样等着、同样断定。断定拼不回来的交给合并，
//! 它马上请被控端恢复，不用像帧流那样等一个往返看重传会不会来。
//!
//! 了结的帧（拼好了、拼不回来了）记下这一帧该到几块、到了几块（[`Tally`]）：控制端报告给被控端（`VideoReport`），
//! 被控端据此配冗余。拼好之后又到的块也算到了。

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use airhand_proto::fec::{FULL_HEADER, Fragment};
use bytes::Bytes;
use tokio::time::Instant;
use tracing::debug;

use crate::clock::now_us;
use crate::lanes::Via;
use crate::secure::counter_of;

/// 后一帧的数据报到了、这一帧又这么久没有新的：它到不齐了（见模块说明）
pub(crate) const GONE_AFTER: Duration = Duration::from_millis(10);
/// 后面没有帧了（画面停了）、后一帧走的是另一条路：这一帧这么久没有新的数据报才算到不齐了
pub(crate) const IDLE_GONE: Duration = Duration::from_millis(150);
/// 同时最多拼着这么多帧，再多就把最老的了结掉
const MAX_PENDING: usize = 64;
/// 记着最近了结的这么多帧：之后迟到的数据报不要了
const FINISHED: usize = 256;

/// 拼好的一帧
#[derive(Debug)]
pub(crate) struct Assembled {
    /// 密封单元（和帧流上读到的一样，不带长度前缀）
    pub data: Vec<u8>,
    /// 第一块到达的时刻（控制端的时钟，微秒，同 `crate::wire::read_packet_timed`）
    pub arrived: u64,
    /// 第一块是从直连来的
    pub direct: bool,
}

/// 了结了的帧的数据报到了多少（报告给被控端，见 `airhand_proto::session::VideoReport`）
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tally {
    /// 拼好了的帧
    pub frames: u32,
    /// 该到的块（原来的加冗余的）
    pub fragments: u32,
    /// 其中没到的
    pub missing: u32,
    /// 拼不回来的帧
    pub unrecovered: u32,
}

/// 一帧的样子（每一块的头里都有）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    len: usize,
    k: usize,
    r: usize,
    shard: usize,
}

struct Pending {
    /// `None`：一块都还没到，只是从后一帧的标志知道它也是纠错发的
    shape: Option<Shape>,
    /// 到了的块（`k + r` 个位置）；拼好之后不再留着数据，只记到没到
    shards: Vec<Option<Bytes>>,
    seen: Vec<bool>,
    got: usize,
    /// 第一块到达的时刻（控制端的时钟，微秒）
    arrived: u64,
    /// 最后一块到达的时刻
    last: Instant,
    /// 第一块从哪条路来
    via: Via,
    /// 拼好了（交出去了）
    done: bool,
}

/// 控制端拼纠错的帧（见模块说明）
pub(crate) struct Reassembler {
    pending: BTreeMap<u64, Pending>,
    /// 最近了结的帧（计数）
    finished: VecDeque<u64>,
    /// 比它小的计数都不要了
    floor: u64,
    /// 到过数据报的最新一帧和它从哪条路来
    newest: Option<(u64, Via)>,
    /// 断定拼不回来、还没交出去的
    gone: Vec<u64>,
    tally: Tally,
    /// 最近每帧平均该到几块（一块都没到的帧按它算）
    avg_total: f64,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
            finished: VecDeque::new(),
            floor: 0,
            newest: None,
            gone: Vec::new(),
            tally: Tally::default(),
            avg_total: 0.0,
        }
    }
}

impl Reassembler {
    /// 收到一个纠错数据报（整个数据报）。这一帧拼好了就交出来；格式不对、迟到的、重复的什么也不做
    pub(crate) fn fragment(&mut self, d: Bytes, via: Via, now: Instant) -> Option<Assembled> {
        let f = Fragment::parse(&d)?;
        let c = f.counter;
        if c < self.floor || self.finished.contains(&c) {
            return None;
        }
        let shape = Shape {
            len: f.len as usize,
            k: f.k as usize,
            r: f.r as usize,
            shard: f.shard.len(),
        };
        let index = f.index as usize;
        // 前一帧也是纠错发的：它一块都没到的话也要断定（见模块说明）
        if f.prev_fec() && c > self.floor {
            let p = c - 1;
            if !self.finished.contains(&p) && !self.pending.contains_key(&p) {
                self.insert(p, Pending::new(None, now, via));
            }
        }
        if self.newest.is_none_or(|(n, _)| c > n) {
            self.newest = Some((c, via));
        }
        if !self.pending.contains_key(&c) {
            self.insert(c, Pending::new(Some(shape), now, via));
        }
        let e = self.pending.get_mut(&c).expect("inserted above");
        match e.shape {
            None => {
                *e = Pending::new(Some(shape), now, via);
            }
            // 同一帧的块说法不一样（被改过的）：不要
            Some(s) if s != shape => return None,
            Some(_) => {}
        }
        if e.seen[index] {
            return None;
        }
        e.seen[index] = true;
        e.got += 1;
        e.last = now;
        if e.done {
            return None;
        }
        e.shards[index] = Some(d.slice(FULL_HEADER..));
        if e.got < shape.k {
            return None;
        }
        let shards = std::mem::take(&mut e.shards);
        let (arrived, direct) = (e.arrived, e.via == Via::Direct);
        match join(shape, shards).filter(|s| counter_of(s) == Some(c)) {
            Some(data) => {
                e.done = true;
                self.tally.frames += 1;
                Some(Assembled {
                    data,
                    arrived,
                    direct,
                })
            }
            None => {
                debug!(counter = c, "a video frame sent as datagrams didn't add up");
                self.finish(c);
                None
            }
        }
    }

    /// 到时候了的帧了结掉，返回断定拼不回来的（计数）
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<u64> {
        let due: Vec<u64> = self
            .pending
            .iter()
            .filter(|(c, e)| now >= self.due(**c, e))
            .map(|(c, _)| *c)
            .collect();
        for c in due {
            self.finish(c);
        }
        std::mem::take(&mut self.gone)
    }

    /// 到这个时候 [`Reassembler::poll`] 可能有事做
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|(c, e)| self.due(*c, e)).min()
    }

    /// 取走到现在为止了结的帧的统计
    pub(crate) fn take_tally(&mut self) -> Tally {
        std::mem::take(&mut self.tally)
    }

    /// 这一帧什么时候了结（见模块说明）
    fn due(&self, c: u64, e: &Pending) -> Instant {
        let later = self.newest.is_some_and(|(n, via)| n > c && via == e.via);
        e.last + if later { GONE_AFTER } else { IDLE_GONE }
    }

    fn insert(&mut self, c: u64, p: Pending) {
        // 拼着的太多了（例如被控端换了一串计数）：最老的了结掉
        while self.pending.len() >= MAX_PENDING {
            let Some((&oldest, _)) = self.pending.first_key_value() else {
                break;
            };
            self.finish(oldest);
        }
        self.pending.insert(c, p);
    }

    /// 了结一帧：记下数据报到了多少，拼不回来的记进 `gone`
    fn finish(&mut self, c: u64) {
        let Some(e) = self.pending.remove(&c) else {
            return;
        };
        let total = match e.shape {
            Some(s) => {
                let total = (s.k + s.r) as f64;
                self.avg_total = if self.avg_total == 0.0 {
                    total
                } else {
                    self.avg_total * 0.9 + total * 0.1
                };
                s.k + s.r
            }
            // 一块都没到：按最近每帧平均几块算
            None => self.avg_total.round().max(1.0) as usize,
        };
        self.tally.fragments += total as u32;
        self.tally.missing += total.saturating_sub(e.got) as u32;
        if !e.done {
            self.tally.unrecovered += 1;
            self.gone.push(c);
        }
        if self.finished.len() == FINISHED {
            self.finished.pop_front();
        }
        self.finished.push_back(c);
        // 很老的计数不再要（记着的了结的帧只有这么多）
        self.floor = self.floor.max(c.saturating_sub(4 * FINISHED as u64));
    }
}

impl Pending {
    fn new(shape: Option<Shape>, now: Instant, via: Via) -> Self {
        let n = shape.map_or(0, |s| s.k + s.r);
        Self {
            shape,
            shards: vec![None; n],
            seen: vec![false; n],
            got: 0,
            arrived: now_us(),
            last: now,
            via,
            done: false,
        }
    }
}

/// 收到的块（至少 `k` 块）拼回密封单元：原来的块都在就直接接起来，不然先补上缺的。补不出来为 `None`
fn join(s: Shape, mut shards: Vec<Option<Bytes>>) -> Option<Vec<u8>> {
    if shards[..s.k].iter().any(Option::is_none) {
        let originals = shards[..s.k]
            .iter()
            .enumerate()
            .filter_map(|(i, b)| Some((i, b.as_ref()?)));
        let recovery = shards[s.k..]
            .iter()
            .enumerate()
            .filter_map(|(i, b)| Some((i, b.as_ref()?)));
        let restored = reed_solomon_simd::decode(s.k, s.r, originals, recovery).ok()?;
        for (i, b) in restored {
            shards[i] = Some(Bytes::from(b));
        }
    }
    let mut out = Vec::with_capacity(s.k * s.shard);
    for b in &shards[..s.k] {
        out.extend_from_slice(b.as_ref()?);
    }
    out.truncate(s.len);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::fragments;
    use crate::secure::{Sealer, test_lane};

    fn frame(s: &mut Sealer, n: usize) -> Vec<u8> {
        let body: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
        s.seal(&body).unwrap()
    }

    /// 原来的块都到了、丢了几块（用冗余补上）、乱序：都拼得回原来的密封单元；拼好之后的块不再交出东西
    #[test]
    fn frames_are_put_back_together() {
        let (mut s, _) = test_lane(false);
        let mut r = Reassembler::default();
        let t0 = Instant::now();
        for (n, lose) in [(500, vec![]), (5000, vec![0, 3]), (20_000, vec![1, 2, 5])] {
            let sealed = frame(&mut s, n);
            let d = fragments(1, &sealed, false, 3).unwrap();
            let mut out = None;
            // 倒着来、丢掉几块
            for (i, d) in d.iter().enumerate().rev() {
                if lose.contains(&i) {
                    continue;
                }
                if let Some(a) = r.fragment(d.clone(), Via::Relay, t0) {
                    assert!(out.is_none(), "delivered twice");
                    out = Some(a);
                }
            }
            let a = out.expect("not reassembled");
            assert_eq!(a.data, sealed);
            assert!(!a.direct);
        }
        let t = r.take_tally();
        assert_eq!((t.frames, t.unrecovered), (3, 0));
    }

    /// 丢得太多：后一帧到了、又过了 GONE_AFTER，断定拼不回来；统计里记着该到几块、到了几块。
    /// 前一帧一块都没到（后一帧的标志说它也是纠错发的）：一样断定
    #[test]
    fn frames_that_cannot_be_rebuilt_are_given_up_quickly() {
        let (mut s, _) = test_lane(false);
        let mut r = Reassembler::default();
        let t0 = Instant::now();
        let f1 = fragments(1, &frame(&mut s, 3000), false, 1).unwrap();
        let f2 = fragments(1, &frame(&mut s, 3000), true, 1).unwrap();
        let f3 = fragments(1, &frame(&mut s, 3000), true, 1).unwrap();
        let (c1, c2) = (0u64, 1u64);
        // 第一帧只到了一块
        assert!(r.fragment(f1[0].clone(), Via::Relay, t0).is_none());
        assert_eq!(r.deadline(), Some(t0 + IDLE_GONE));
        // 第二帧一块都没到；第三帧都到了
        let t1 = t0 + Duration::from_millis(5);
        let got = f3
            .iter()
            .filter_map(|d| r.fragment(d.clone(), Via::Relay, t1))
            .count();
        assert_eq!(got, 1);
        assert!(r.poll(t1).is_empty());
        // 后面的到了，前面两帧又过了 GONE_AFTER：都到不齐了
        let gone = r.poll(t1 + GONE_AFTER);
        assert_eq!(gone, [c1, c2]);
        // 迟到的块不要了
        assert!(
            r.fragment(f2[0].clone(), Via::Relay, t1 + GONE_AFTER)
                .is_none()
        );
        let t = r.poll(t1 + IDLE_GONE * 2);
        assert!(t.is_empty());
        let tally = r.take_tally();
        assert_eq!((tally.frames, tally.unrecovered), (1, 2));
        let per = f3.len() as u32;
        assert_eq!(tally.fragments, 3 * per);
        assert_eq!(tally.missing, 2 * per - 1);
    }

    /// 后面没有帧了（画面停了）：等 IDLE_GONE；后一帧走的是另一条路（换路了）：也等 IDLE_GONE
    #[test]
    fn idle_and_path_changes_wait_longer() {
        let (mut s, _) = test_lane(false);
        let mut r = Reassembler::default();
        let t0 = Instant::now();
        let f1 = fragments(1, &frame(&mut s, 3000), false, 1).unwrap();
        let f2 = fragments(1, &frame(&mut s, 300), true, 1).unwrap();
        r.fragment(f1[0].clone(), Via::Relay, t0);
        // 后一帧从直连来，先到了
        assert!(r.fragment(f2[0].clone(), Via::Direct, t0).is_some());
        assert!(r.poll(t0 + GONE_AFTER).is_empty());
        // 旧路上的块还在来
        r.fragment(f1[1].clone(), Via::Relay, t0 + Duration::from_millis(30));
        assert_eq!(r.poll(t0 + Duration::from_millis(30) + IDLE_GONE), [0]);
    }

    /// 被改过的块（拼出来的密封单元计数不对、解不开）：不交出去，算拼不回来
    #[test]
    fn tampered_fragments_do_not_produce_frames() {
        let (mut s, _) = test_lane(false);
        let mut r = Reassembler::default();
        let t0 = Instant::now();
        let sealed = frame(&mut s, 1500);
        let d = fragments(1, &sealed, false, 1).unwrap();
        assert_eq!(d.len(), 3);
        // 第二块的最后一个字节（密封单元末尾的计数，两块正好没有补 0）改掉
        let mut bad = d[1].to_vec();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(r.fragment(d[0].clone(), Via::Relay, t0).is_none());
        assert!(r.fragment(Bytes::from(bad), Via::Relay, t0).is_none());
        assert_eq!(r.take_tally().unrecovered, 1);
        // 之后同一帧的块不要了
        assert!(r.fragment(d[2].clone(), Via::Relay, t0).is_none());
    }
}
