//! 被控端决定画面的每一帧走帧流还是纠错、配几块冗余（见 `airhand_proto::session` 的"前向纠错"）。
//!
//! 只有增量帧、而且参考的就是前一帧的才考虑纠错（[`Chooser::plan`]）：关键帧、恢复帧总是走帧流。
//! 走不走纠错看控制端的画质设置（[`VideoTransport`]）：固定的照做；自动时看控制端的报告（`VideoReport`，
//! 每 [`airhand_proto::session::REPORT_EVERY`] 一个）：
//!
//! * **改用纠错**：最近 [`WINDOW`] 里从帧流收完的帧，等重传卡住过的（控制端看得到：读到一半停住了一阵，
//!   后面的帧却先到了）不少于 [`ENTER`]。往返时延很短时重传很快，读的一方看不出停顿，也就不会改。
//!   刚改用时还没量过数据报丢多少，先按卡住的帧估计一个丢包率（`1 - (1 - 卡住的比例)^(1 / 每帧块数)`）；
//! * **换回帧流**：纠错时按量到的丢包率估计，换回帧流的话每帧被重传卡住的机会（`1 - (1 - 丢包率)^每帧块数`）
//!   [`CALM_FOR`] 都低于 [`EXIT`]；或者往返时延（发出到确认的最小值）[`FAST_FOR`] 都低于 [`FAST_RTT_MS`]：
//!   重传很快，用不着多发冗余；
//! * 每次切换之后至少 [`DWELL`] 不再切换，免得来回换。
//!
//! 冗余（[`crate::fec::repair_count`]）按最近 [`WINDOW`] 里报告的丢包率配。
//!
//! 报告也换算成码率控制要的（[`Chooser::feedback`]，见 `crate::flow` 的"丢包""纠错的冗余"）：
//!
//! * 最近 [`LOSS_WINDOW`] 的丢包率：有纠错的块就是没到的块的比例；不然按帧流上卡住的帧估计
//!   （`1 - (1 - 卡住的比例)^(1 / 每帧块数)`，和改用纠错时一样；往返很短、重传很快时看不出卡住，估计得偏低）；
//!   收到的太少时量不出来；
//! * 画面在线上的字节是编码器出的多少倍：写任务每帧告诉它（[`Chooser::sent_bytes`]），按最近几十帧算。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use airhand_proto::fec::{MAX_FRAME, shard_size};
use airhand_proto::session::{VideoReport, VideoTransport};
use tokio::time::Instant;
use tracing::info;

use super::repair_count;
use crate::flow::{Feedback, WindowMin};

/// 看最近这么久的报告
pub(crate) const WINDOW: Duration = Duration::from_secs(3);
/// 每次切换之后至少这么久不再切换
pub(crate) const DWELL: Duration = Duration::from_secs(5);
/// 帧流上等重传卡住过的帧至少这么多（比例），才改用纠错
pub(crate) const ENTER: f64 = 0.02;
/// 而且至少这么多帧、其中卡住过的至少这么多（帧少的时候比例不准）
const ENTER_FRAMES: u32 = 30;
const ENTER_STALLED: u32 = 3;
/// 纠错时估计换回帧流每帧被卡住的机会低于这么多……
pub(crate) const EXIT: f64 = 0.005;
/// ……这么久，就换回帧流
pub(crate) const CALM_FOR: Duration = Duration::from_secs(20);
/// 往返时延低于这么多（毫秒）……
pub(crate) const FAST_RTT_MS: f64 = 15.0;
/// ……这么久，也换回帧流
pub(crate) const FAST_FOR: Duration = Duration::from_secs(10);
/// 丢包率至少看这么多块才算数
const MIN_FRAGMENTS: u32 = 20;
/// 记着最近这么多帧的发出时刻（量往返时延）
const SENT: usize = 128;
/// 给码率控制的丢包率看最近这么久的报告（见模块说明）
pub(crate) const LOSS_WINDOW: Duration = Duration::from_secs(1);
/// 纠错的块至少这么多、或者帧流上的帧至少这么多，才量得出丢包率
const LOSS_MIN_FRAGMENTS: u32 = 50;
const LOSS_MIN_FRAMES: u32 = 20;
/// 线上字节的倍数按最近几十帧算：每帧把之前的乘以它
const BYTES_DECAY: f64 = 0.97;

/// 被控端一个会话的画面怎么传（见模块说明）。写任务每帧问一次，读任务交给它控制端的报告、确认和画质设置
pub(crate) struct Chooser(Mutex<State>);

struct State {
    mode: VideoTransport,
    /// 自动时：现在用着纠错
    on: bool,
    /// 上次切换的时刻
    switched: Instant,
    reports: VecDeque<(Instant, VideoReport)>,
    /// 纠错的数据报的丢包率（最近 [`WINDOW`] 里报告的）
    loss: f64,
    /// 纠错的帧平均几块
    shards: f64,
    /// 纠错时：从什么时候起估计换回帧流也很少卡住
    calm: Option<Instant>,
    /// 纠错时：从什么时候起往返时延很短
    fast: Option<Instant>,
    rtt: WindowMin,
    sent: VecDeque<(u32, Instant)>,
    /// 最近的帧编码器出了多少字节、线上发了多少（衰减的和，见 [`Chooser::sent_bytes`]）
    encoded: f64,
    wire: f64,
    /// 最近发的 [`SENT`] 帧：(序号, 走的是纠错)（见 [`Chooser::retransmitted_after`]）
    how: VecDeque<(u32, bool)>,
}

impl Chooser {
    /// `mode`：控制端开始时的画质设置里的
    pub(crate) fn new(mode: VideoTransport) -> Self {
        let mode = mode.or_auto();
        Self(Mutex::new(State {
            mode,
            on: false,
            switched: Instant::now(),
            reports: VecDeque::new(),
            loss: 0.0,
            shards: 1.0,
            calm: None,
            fast: None,
            rtt: WindowMin::default(),
            sent: VecDeque::new(),
            encoded: 0.0,
            wire: 0.0,
            how: VecDeque::new(),
        }))
    }

    /// 第 `seq` 帧发出去了：密封单元 `encoded` 字节，线上发了 `wire` 字节（纠错的是所有数据报，帧流上的就是 `encoded`）；
    /// `fec`：走的是纠错的数据报
    pub(crate) fn sent_bytes(&self, seq: u32, encoded: usize, wire: usize, fec: bool) {
        let mut s = self.0.lock().unwrap();
        s.encoded = s.encoded * BYTES_DECAY + encoded as f64;
        s.wire = s.wire * BYTES_DECAY + wire as f64;
        if s.how.len() == SENT {
            s.how.pop_front();
        }
        s.how.push_back((seq, fec));
    }

    /// 控制端解到第 `decoded` 帧之后等不到了：等不到的那一帧（发给它的、`decoded` 之后的第一帧）还会重传吗？
    /// 走帧流的会（QUIC 重传），走纠错的数据报的不会；记不得了的按会算
    pub(crate) fn retransmitted_after(&self, decoded: u32) -> bool {
        let s = self.0.lock().unwrap();
        s.how
            .iter()
            .find(|(seq, _)| seq.wrapping_sub(decoded) as i32 > 0)
            .is_none_or(|(_, fec)| !fec)
    }

    /// 码率控制要的（见模块说明）：最近 [`LOSS_WINDOW`] 的丢包率、线上字节的倍数
    pub(crate) fn feedback(&self, now: Instant) -> Feedback {
        let s = self.0.lock().unwrap();
        let recent = s
            .reports
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) < LOSS_WINDOW)
            .fold(VideoReport::default(), |a, (_, r)| VideoReport {
                frames: a.frames + r.frames,
                stalled: a.stalled + r.stalled,
                fragments: a.fragments + r.fragments,
                missing: a.missing + r.missing,
                unrecovered: a.unrecovered + r.unrecovered,
            });
        let loss = if recent.fragments >= LOSS_MIN_FRAGMENTS {
            Some(recent.missing as f64 / recent.fragments as f64)
        } else if recent.frames >= LOSS_MIN_FRAMES {
            let stalled = (recent.stalled as f64 / recent.frames as f64).min(1.0);
            Some(1.0 - (1.0 - stalled).powf(1.0 / s.shards.max(1.0)))
        } else {
            None
        };
        let overhead = if s.encoded > 0.0 {
            (s.wire / s.encoded).max(1.0)
        } else {
            1.0
        };
        Feedback { loss, overhead }
    }

    /// 控制端改了画质设置。自动的话从现在用的接着看（刚才固定用纠错，就先接着用纠错）
    pub(crate) fn set_mode(&self, mode: VideoTransport) {
        let mode = mode.or_auto();
        let mut s = self.0.lock().unwrap();
        if mode == s.mode {
            return;
        }
        s.on = match s.mode {
            VideoTransport::Fec => true,
            VideoTransport::Streams => false,
            _ => s.on,
        };
        s.mode = mode;
        s.switched = Instant::now();
        s.reports.clear();
        s.calm = None;
        s.fast = None;
        info!(?mode, "video transport setting changed");
    }

    /// 现在增量帧走不走纠错
    #[cfg(test)]
    pub(crate) fn using(&self) -> bool {
        self.0.lock().unwrap().using()
    }

    /// 这一帧（密封单元 `len` 字节）怎么发：`Some(r)` 走纠错、配 `r` 块冗余；`None` 走帧流。
    /// `eligible`：增量帧、参考的就是前一帧（见模块说明）
    pub(crate) fn plan(&self, len: usize, eligible: bool) -> Option<usize> {
        if !eligible || len > MAX_FRAME {
            return None;
        }
        let mut s = self.0.lock().unwrap();
        // 帧流时也记着每帧多少块：改用纠错时据此估计丢包率（见 State::switch）
        let (_, k) = shard_size(len);
        s.shards = s.shards * 0.9 + k as f64 * 0.1;
        if !s.using() {
            return None;
        }
        Some(repair_count(k, s.loss))
    }

    /// 第 `seq` 帧发出去了（量往返时延用）
    pub(crate) fn sent(&self, seq: u32, now: Instant) {
        let mut s = self.0.lock().unwrap();
        if s.sent.len() == SENT {
            s.sent.pop_front();
        }
        s.sent.push_back((seq, now));
    }

    /// 控制端确认到了第 `seq` 帧（累积确认）
    pub(crate) fn acked(&self, seq: u32, now: Instant) {
        let mut s = self.0.lock().unwrap();
        let mut sample = None;
        while let Some(&(q, at)) = s.sent.front() {
            if q.wrapping_sub(seq) as i32 > 0 {
                break;
            }
            s.sent.pop_front();
            if q == seq {
                sample = Some(now.saturating_duration_since(at));
            }
        }
        if let Some(rtt) = sample {
            s.rtt.push(rtt.as_secs_f64() * 1000.0, now.into_std());
        }
    }

    /// 控制端的报告（见模块说明）
    pub(crate) fn report(&self, r: VideoReport, now: Instant) {
        let mut s = self.0.lock().unwrap();
        s.reports.push_back((now, r));
        while s
            .reports
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > WINDOW)
        {
            s.reports.pop_front();
        }
        let sum = s
            .reports
            .iter()
            .fold(VideoReport::default(), |a, (_, r)| VideoReport {
                frames: a.frames + r.frames,
                stalled: a.stalled + r.stalled,
                fragments: a.fragments + r.fragments,
                missing: a.missing + r.missing,
                unrecovered: a.unrecovered + r.unrecovered,
            });
        if sum.fragments >= MIN_FRAGMENTS {
            s.loss = sum.missing as f64 / sum.fragments as f64;
        }
        if s.mode.or_auto() != VideoTransport::Auto {
            return;
        }
        let settled = now.saturating_duration_since(s.switched) >= DWELL;
        if !s.on {
            let stalls = sum.frames >= ENTER_FRAMES
                && sum.stalled >= ENTER_STALLED
                && sum.stalled as f64 >= sum.frames as f64 * ENTER;
            if settled && stalls {
                // 还没量过纠错的数据报丢多少：先按卡住的帧估计一个（每帧有一块丢了就会卡住，估计得偏低），
                // 不然头几帧只配最少的冗余，补不回来的多；之后按报告里量到的
                let stalled = sum.stalled as f64 / sum.frames as f64;
                let guess = 1.0 - (1.0 - stalled).powf(1.0 / s.shards.max(1.0));
                s.loss = s.loss.max(guess);
                info!(
                    frames = sum.frames,
                    stalled = sum.stalled,
                    loss = s.loss,
                    "video frames keep waiting for retransmissions, switching to forward error correction"
                );
                s.switch(true, now);
            }
            return;
        }
        // 估计换回帧流的话每帧被重传卡住的机会（只在这一段有纠错的帧时更新：画面停着时不算数）
        if r.fragments > 0 || r.unrecovered > 0 {
            let stall = 1.0 - (1.0 - s.loss).powf(s.shards.max(1.0));
            s.calm = if stall < EXIT {
                s.calm.or(Some(now))
            } else {
                None
            };
        }
        let rtt = s.rtt.get();
        s.fast = if rtt.is_some_and(|ms| ms < FAST_RTT_MS) {
            s.fast.or(Some(now))
        } else {
            None
        };
        let lasted = |t: Option<Instant>, d: Duration| {
            t.is_some_and(|t| now.saturating_duration_since(t) >= d)
        };
        if settled && (lasted(s.calm, CALM_FOR) || lasted(s.fast, FAST_FOR)) {
            info!(
                loss = s.loss,
                rtt_ms = rtt,
                "the network is fine, switching video back to retransmissions"
            );
            s.switch(false, now);
        }
    }
}

impl State {
    fn using(&self) -> bool {
        match self.mode {
            VideoTransport::Fec => true,
            VideoTransport::Streams => false,
            _ => self.on,
        }
    }

    fn switch(&mut self, on: bool, now: Instant) {
        self.on = on;
        self.switched = now;
        // 之前的报告是另一种传法下的
        self.reports.clear();
        self.calm = None;
        self.fast = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(frames: u32, stalled: u32) -> VideoReport {
        VideoReport {
            frames,
            stalled,
            ..Default::default()
        }
    }

    fn fec(fragments: u32, missing: u32) -> VideoReport {
        VideoReport {
            fragments,
            missing,
            ..Default::default()
        }
    }

    /// 固定的照做；关键帧、恢复帧、太大的帧总是走帧流
    #[test]
    fn fixed_modes_and_ineligible_frames() {
        let c = Chooser::new(VideoTransport::Fec);
        assert!(c.plan(5000, true).is_some());
        assert!(c.plan(5000, false).is_none());
        assert!(c.plan(MAX_FRAME + 1, true).is_none());
        c.set_mode(VideoTransport::Streams);
        assert!(c.plan(5000, true).is_none());
        // 不认识的按自动：一开始用帧流
        let c = Chooser::new(VideoTransport::Unknown(7));
        assert!(!c.using());
    }

    /// 自动：帧流上卡住的帧多了改用纠错（刚开始、刚切换过的 DWELL 里不换）；丢包率决定冗余
    #[tokio::test(start_paused = true)]
    async fn stalls_switch_to_fec_and_loss_sets_the_redundancy() {
        let c = Chooser::new(VideoTransport::Auto);
        let t0 = Instant::now();
        // 1%：不换
        for i in 0..6 {
            c.report(stream(30, 0), t0 + DWELL + Duration::from_millis(500 * i));
        }
        c.report(stream(30, 2), t0 + DWELL + Duration::from_secs(3));
        assert!(!c.using());
        // 太早了：不换
        let c2 = Chooser::new(VideoTransport::Auto);
        c2.report(stream(60, 10), t0 + Duration::from_secs(1));
        assert!(!c2.using());
        // 5%：换
        let t1 = t0 + DWELL + Duration::from_secs(4);
        c.report(stream(30, 3), t1);
        assert!(c.using());
        let low = c.plan(20_000, true).unwrap();
        // 量到 10% 的丢包：冗余多了
        for i in 1..=4 {
            c.report(fec(100, 10), t1 + Duration::from_millis(500 * i));
        }
        let high = c.plan(20_000, true).unwrap();
        assert!(high > low, "{low} → {high}");
    }

    /// 自动：纠错时丢包率一直很低，CALM_FOR 之后换回帧流；有丢包就一直用纠错
    #[tokio::test(start_paused = true)]
    async fn a_clean_network_switches_back() {
        let c = Chooser::new(VideoTransport::Auto);
        let t0 = Instant::now() + DWELL;
        c.report(stream(30, 5), t0);
        assert!(c.using());
        c.plan(20_000, true);
        let mut t = t0;
        // 一直有丢包（2%，每帧二十几块）：不换
        while t < t0 + CALM_FOR * 2 {
            t += Duration::from_millis(500);
            c.report(fec(200, 4), t);
        }
        assert!(c.using());
        // 不丢了：CALM_FOR 之后换回来
        let quiet = t;
        while t < quiet + CALM_FOR + Duration::from_secs(4) {
            t += Duration::from_millis(500);
            c.report(fec(200, 0), t);
            if !c.using() {
                break;
            }
        }
        assert!(!c.using());
        assert!(t - quiet >= CALM_FOR && t - quiet < CALM_FOR + WINDOW + Duration::from_secs(1));
    }

    /// 自动：往返时延很短（重传很快）时 FAST_FOR 之后换回帧流
    #[tokio::test(start_paused = true)]
    async fn a_short_round_trip_switches_back() {
        let c = Chooser::new(VideoTransport::Auto);
        let t0 = Instant::now() + DWELL;
        c.report(stream(30, 5), t0);
        assert!(c.using());
        let mut t = t0;
        for seq in 1..=40u32 {
            t += Duration::from_millis(500);
            c.sent(seq, t);
            c.acked(seq, t + Duration::from_millis(5));
            // 一直有丢包，但重传很快
            c.report(fec(200, 10), t);
        }
        assert!(!c.using());
    }

    /// 码率控制要的：最近 LOSS_WINDOW 的丢包率（有纠错的块看块，不然按帧流上卡住的帧估计；太少量不出来）、
    /// 线上字节的倍数（按最近的帧）
    #[tokio::test(start_paused = true)]
    async fn feedback_for_the_rate_control() {
        let c = Chooser::new(VideoTransport::Streams);
        let t0 = Instant::now();
        assert_eq!(
            c.feedback(t0),
            Feedback {
                loss: None,
                overhead: 1.0
            }
        );
        // 帧流上的帧每帧 10 块左右；30 帧里卡住了 3 帧
        for _ in 0..100 {
            c.plan(10 * airhand_proto::fec::MAX_SHARD - 100, true);
        }
        c.report(stream(30, 3), t0);
        let loss = c.feedback(t0).loss.unwrap();
        assert!((loss - (1.0 - 0.9f64.powf(0.1))).abs() < 1e-3, "{loss}");
        // 帧太少：量不出来
        c.report(stream(10, 3), t0 + LOSS_WINDOW);
        assert_eq!(c.feedback(t0 + LOSS_WINDOW).loss, None);
        // 有纠错的块就看块；一秒以前的报告不算
        let t1 = t0 + LOSS_WINDOW * 2;
        c.report(fec(100, 20), t1);
        assert_eq!(c.feedback(t1).loss, Some(0.2));
        c.report(fec(100, 0), t1 + LOSS_WINDOW / 2);
        assert_eq!(c.feedback(t1 + LOSS_WINDOW / 2).loss, Some(0.1));
        assert_eq!(c.feedback(t1 + LOSS_WINDOW).loss, Some(0.0));
        // 线上字节：纠错的帧多出冗余，帧流上的一样多
        c.sent_bytes(1, 1000, 1500, true);
        assert_eq!(c.feedback(t1).overhead, 1.5);
        c.sent_bytes(2, 1000, 1000, false);
        let o = c.feedback(t1).overhead;
        assert!(
            (o - (1500.0 * BYTES_DECAY + 1000.0) / (1000.0 * BYTES_DECAY + 1000.0)).abs() < 1e-9
        );
        for seq in 3..300 {
            c.sent_bytes(seq, 1000, 1000, false);
        }
        assert!(c.feedback(t1).overhead < 1.001);
    }

    /// 等不到的那一帧还会不会重传：看 decoded 之后发的第一帧走的是帧流还是纠错（序号会绕回）
    #[test]
    fn which_lost_frames_are_retransmitted() {
        let c = Chooser::new(VideoTransport::Auto);
        for (seq, fec) in [
            (u32::MAX - 1, false),
            (u32::MAX, true),
            (0, true),
            (1, false),
        ] {
            c.sent_bytes(seq, 1000, 1000, fec);
        }
        assert!(!c.retransmitted_after(u32::MAX - 1));
        assert!(!c.retransmitted_after(u32::MAX));
        assert!(c.retransmitted_after(0));
        // 之后还没发过、记不得了：按会重传算
        assert!(c.retransmitted_after(1));
        assert!(Chooser::new(VideoTransport::Auto).retransmitted_after(5));
    }

    /// 改设置：从固定的纠错改成自动，先接着用纠错
    #[test]
    fn switching_to_auto_keeps_what_is_in_use() {
        let c = Chooser::new(VideoTransport::Fec);
        c.set_mode(VideoTransport::Auto);
        assert!(c.using());
        c.set_mode(VideoTransport::Streams);
        c.set_mode(VideoTransport::Auto);
        assert!(!c.using());
    }
}
