//! 被控端的在途控制和码率控制（桌面版和 Android 共用）。帧序号属于会话，不属于某个编码器或显示器：
//! 重建编码管线时接着用同一个 [`Flow`]。
//!
//! 端到端的拥塞控制只能在这里做：中转把 QUIC 连接拆成了两段，两段的拥塞控制各管各的。
//!
//! 控制端每收到一帧都确认（累积确认，几帧连着到时只确认最后一帧），确认里带着这一帧的**到达时刻**
//! （控制端的时钟，见 `ControllerDatagram::Ack`）：它的第一个字节到达和前一帧交出去这两个时刻里
//! 晚的那个（`crate::merge` 的"到达时刻"），也就是以前画面是一条有序流时读到这一帧开头的时刻。前面的帧在等重传时，
//! 后面的帧即使早就到了也算晚到：丢包等重传也算成排队（见下面的"到达时刻"）。从确认里知道三件事：
//!
//! * **往返时延**：发出到收到确认，含界面那一段（IPC、8 ms 的确认合并）。基础 RTT 是最近 [`MIN_RTT_WINDOW`]
//!   里的最小值（单调队列实现的滑动窗口最小值）；
//! * **排队时延**（[`FlowState::queue_delay`]）：到达时刻减去本端的发送时刻，是单向时延加上两边时钟之差（常数），
//!   再减去它最近 [`MIN_RTT_WINDOW`] 里的最小值，就是这一帧在路上多排了多久的队（本端 QUIC 的发送缓冲、中转、
//!   瓶颈链路的缓冲区，以及等前面的帧重传）。不含界面那一段、确认回来的路，也不含这一帧自己要传多久；
//! * **带宽**：两次确认之间控制端收到了多少字节。只有这一帧发出时前面的还没传完（它排的队比两次到达的间隔还长），
//!   这段时间链路才一直在忙，算出来的才是带宽；不然只是编码器出了多少。
//!
//! **码率**（[`FlowState::bitrate_kbps`]，编码器每帧取一次）随每次确认调整：
//!
//! * 最近 [`STANDING`] 次确认排的队都超过 [`QUEUE_HIGH_MS`]（取最小值，偶尔一帧慢不算），或者排着队、在途字节又已经
//!   挡住了要发的帧（在途字节把队压在门槛以下，码率却比带宽高，只好跳帧）：降到排队期间带宽的 85%，
//!   至少降 10%（量不出带宽时降 20%）。降了之后等一个往返加上排着的队再看：正在消下去就接着等；
//!   降了两次还是不见少，说明不是我们发多了（换了路由、别的下载占着路由器的缓冲区），以现在的单向时延为新的基准；
//! * 不排队（低于 [`QUEUE_LOW_MS`]），编码器也确实用到了估计值的一半以上：每秒约加 15%，接近上次拥塞时的带宽时
//!   每秒约 4%，不超过画质设置里的码率。画面静止、编码器用不了那么多时不加：没有证据说明网络撑得住；
//! * 估计值变化 5% 以上才交给编码器，不用每帧都重新配置编码器。
//!
//! **到达时刻**：只看排队，看不见不排队、直接丢包的网络。公网上（尤其家宽的上行）大流量的 UDP 常被限速
//! （令牌桶，超了就丢），这时只看每一帧自己什么时候开始到，码率一直顶在上限，一直丢、一直等重传：在一条被限到
//! 5 Mbps 的路上（往返 40 ms、上限 8 Mbps），帧的延迟从 35 ms 涨到 93 ms，帧率从 48 掉到 34。所以到达时刻含着等前面的帧
//! 重传（见上），丢包马上就体现成排队、码率马上降下来，和以前一条有序流时一样。代价是丢包多、带宽却够用时码率
//! 也降得多（随机丢 1% 时降到两成左右）：延迟优先。
//!
//! 每路控制端定时报告收完和停顿的帧数。被控端根据近期帧大小估计每帧的数据包数，换算为丢包率。
//! 超过 [`LOSS_HIGH`] 时每秒最多降一次。
//! * 超过 [`LOSS_LOW`]：不再往上加；
//! * 更少：不管。
//!
//! 关闭自适应时编码器使用设置的码率。在途窗口继续按链路估计限流。
//! 网络跟不上时少发几帧，不让延迟越积越多。
//!
//! **在途字节**（已发送、未确认）不超过 码率 × (基础 RTT + [`QUEUE_ALLOWANCE_MS`])：码率估高了、带宽突然掉了，
//! 网络里最多也只积压这么久的数据；编码器等着，等到了就编最新的画面。没有在途的帧时总能发一帧，关键帧再大也不会卡住。
//! 另外最多 [`MAX_WINDOW`] 帧、[`MAX_INFLIGHT_BYTES`] 字节：中转往控制端的连接上所有会话共用一个发送窗口，
//! 画面占满了它，同一连接上的 Bye 也写不进去。
//!
//! **换路**（中转 ⇄ 直连，见 `crate::direct`，[`FlowState::path_changed`]）：新的路时延不一样，基础 RTT、
//! 最小单向时延从头量（不然换到慢一点的路会被当成排队、白白降码率），换路之前发出的帧确认回来时不算样本
//! （它们走的是旧路）；码率的估计接着用。
//! 直连断了换回中转时，断在直连上的帧永远不会确认了：不再算在途，编码器马上能发下一帧（关键帧）。
//!
//! **等不到的帧**（帧流，控制端发了 `ControllerDatagram::Lost`，见 [`FlowState::lost`]）：控制端解到的最后一帧之后、
//! 在途的帧都作废，不再算在途（确认不会来了，它们的流也被放弃了）；下一帧怎么编见 [`crate::refs`]。
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use airhand_proto::session::{QualitySettings, VideoReport};

use crate::refs::{LtrReport, Plan, Refs};

/// 在途帧数的上限（Android 的画面队列、控制端记到达时刻的环形缓冲都按它定大小）
pub const MAX_WINDOW: u32 = 32;
/// 在途字节的上限，与码率、RTT 无关
pub const MAX_INFLIGHT_BYTES: usize = 4 * 1024 * 1024;
/// 控制心跳正常也不能永久占着未确认的视频；超过期限由会话关闭并重连。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(20);
/// 基础 RTT、最小单向时延取这么长时间里的最小样本
pub const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
/// 在途字节按 基础 RTT 加上这么久 算：网络里最多排这么久的队（毫秒）
pub const QUEUE_ALLOWANCE_MS: f64 = 50.0;
/// 排队超过这么久算拥塞（毫秒）。比 [`QUEUE_ALLOWANCE_MS`] 小：在途字节卡住之前先降码率
pub const QUEUE_HIGH_MS: f64 = 30.0;
/// 排队少于这么久算通畅（毫秒）
pub const QUEUE_LOW_MS: f64 = 10.0;
/// 看最近这么多次确认排的队
pub const STANDING: usize = 3;
/// 还没有 RTT 样本时按这么多算（毫秒）
const DEFAULT_RTT_MS: f64 = 100.0;
/// 在途字节至少允许这么多
const MIN_WINDOW_BYTES: usize = 16 * 1024;
/// 编码器用了多少码率看最近这么久（控制端的时钟，微秒）；中间空了这么久（画面静止）就从头算
const RATE_SPAN_US: u64 = 1_000_000;
/// 至少这么长（微秒）才算得出编码器用了多少码率
const MIN_RATE_SPAN_US: u64 = 100_000;
/// 丢包率超过这么多：降码率（见模块说明的"丢包"）
pub const LOSS_HIGH: f64 = 0.10;
/// 丢包率超过这么多：不再加码率
pub const LOSS_LOW: f64 = 0.02;
/// 因为丢包降码率至少隔这么久：报告里的丢包率是最近一秒的，同一段丢包不能降好几次
pub const LOSS_CUT_EVERY: Duration = Duration::from_secs(1);
/// 丢包率这么久没更新就不算数了（画面停着时控制端不报告）
const LOSS_TTL: Duration = Duration::from_secs(3);

/// `kbps` 的码率、`fps` 的帧率下平均一帧多少字节
pub fn frame_bytes(kbps: u32, fps: u8) -> usize {
    kbps as usize * 125 / fps.max(1) as usize
}

/// 最近 [`MIN_RTT_WINDOW`] 里的最小值：样本按时间排列、值严格递增，队头就是窗口里的最小值。
/// 窗口里只有一个样本时即使过了期也留着（画面静止时不发帧，没有新样本）
#[derive(Debug, Default)]
pub(crate) struct WindowMin(VecDeque<(f64, Instant)>);

impl WindowMin {
    pub(crate) fn push(&mut self, v: f64, now: Instant) {
        while self.0.back().is_some_and(|&(x, _)| x >= v) {
            self.0.pop_back();
        }
        self.0.push_back((v, now));
        while self.0.len() > 1
            && self
                .0
                .front()
                .is_some_and(|&(_, t)| now.duration_since(t) > MIN_RTT_WINDOW)
        {
            self.0.pop_front();
        }
    }

    pub(crate) fn get(&self) -> Option<f64> {
        self.0.front().map(|&(v, _)| v)
    }
}

/// 发出去、还没确认的一帧
#[derive(Debug, Clone, Copy)]
struct Sent {
    seq: u32,
    at: Instant,
    bytes: usize,
    /// 在它之前一共发了多少字节
    before: u64,
}

/// 一次带着到达时刻的确认
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// 这一帧的到达时刻（控制端的时钟，微秒，见模块说明）
    arrived: u64,
    /// 在它之前一共发了多少字节：流是有序的，到这个时刻控制端已经收到了这么多
    before: u64,
    /// 它排了多久的队（毫秒）
    queue: f64,
}

#[derive(Debug)]
pub struct FlowState {
    sent: u32,
    acked: u32,
    pending: VecDeque<Sent>,
    pending_bytes: usize,
    /// 一共发了多少字节
    total: u64,
    srtt_ms: f64,
    min_rtt: WindowMin,
    /// 把发送时刻换成毫秒的起点（和控制端的到达时刻相减）
    epoch: Instant,
    /// 单向时延（到达时刻 − 发送时刻，含两边时钟之差，毫秒）的最小值
    min_owd: WindowMin,
    /// 最近的确认（[`RATE_SPAN_US`] 之内）
    samples: VecDeque<Sample>,
    rate: Rate,
    /// 编码器用估计的码率（不然用画质设置里的）
    adaptive: bool,
    /// 上次调码率以来，在途字节挡住过要发的帧（[`FlowState::note_blocked`]）
    blocked: bool,
    /// 换了路（[`FlowState::path_changed`]）：这一帧和之前发的走的是旧路，确认时不算时延样本
    old_path: Option<u32>,
    /// 每一帧参考哪一帧、等不到帧时怎么恢复（见 [`crate::refs`]）
    refs: Refs,
    /// 每帧的估计包数和上一个报告，用于从帧停顿率估计丢包。
    packets_per_frame: f64,
    last_report: Option<(Instant, VideoReport)>,
}

impl Default for FlowState {
    fn default() -> Self {
        Self {
            sent: 0,
            acked: 0,
            pending: VecDeque::new(),
            pending_bytes: 0,
            total: 0,
            srtt_ms: 0.0,
            min_rtt: WindowMin::default(),
            epoch: Instant::now(),
            min_owd: WindowMin::default(),
            samples: VecDeque::new(),
            rate: Rate::new(QualitySettings::default().bitrate_kbps),
            adaptive: true,
            blocked: false,
            old_path: None,
            refs: Refs::default(),
            packets_per_frame: 1.0,
            last_report: None,
        }
    }
}

impl FlowState {
    pub fn on_sent(&mut self, seq: u32, bytes: usize) {
        self.on_sent_at(seq, bytes, Instant::now());
    }

    fn on_sent_at(&mut self, seq: u32, bytes: usize, now: Instant) {
        // 约 1200 字节有效载荷一个 QUIC 包，使用平滑值以减小关键帧的影响。
        self.packets_per_frame =
            0.9 * self.packets_per_frame + 0.1 * bytes.div_ceil(1200).max(1) as f64;
        // Android shares a sequence across subscribers, so skipped numbers are
        // not in-flight frames. Never use sent - acked as the queue length.
        if self.pending.is_empty() {
            self.acked = seq.wrapping_sub(1);
        }
        self.sent = seq;
        self.pending.push_back(Sent {
            seq,
            at: now,
            bytes,
            before: self.total,
        });
        self.pending_bytes += bytes;
        self.total += bytes as u64;
    }

    /// 控制端确认到了第 `seq` 帧（累积）。`arrived`：这一帧的到达时刻（控制端的时钟，微秒，见模块说明）
    pub fn on_ack(&mut self, seq: u32, arrived: Option<u64>) {
        self.on_ack_at(seq, arrived, Instant::now());
    }

    fn on_ack_at(&mut self, seq: u32, arrived: Option<u64>, now: Instant) {
        if seq.wrapping_sub(self.acked) as i32 <= 0 || seq.wrapping_sub(self.sent) as i32 > 0 {
            return;
        }
        self.acked = seq;
        self.refs.acked(seq);
        let mut acked = None;
        while let Some(&f) = self.pending.front() {
            if f.seq.wrapping_sub(seq) as i32 > 0 {
                break;
            }
            self.pending.pop_front();
            self.pending_bytes -= f.bytes;
            if f.seq == seq {
                acked = Some(f);
            }
        }
        let Some(f) = acked else { return };
        // 旧路上发的帧：时延是旧路的，不能拿来当新路的基准
        if let Some(last) = self.old_path {
            if f.seq.wrapping_sub(last) as i32 <= 0 {
                return;
            }
            self.old_path = None;
        }
        let rtt = now.saturating_duration_since(f.at).as_secs_f64() * 1000.0;
        self.srtt_ms = if self.srtt_ms == 0.0 {
            rtt
        } else {
            self.srtt_ms * 0.875 + rtt * 0.125
        };
        self.min_rtt.push(rtt, now);
        if let Some(arrived) = arrived {
            self.on_arrival(f, arrived, now);
        }
    }

    fn on_arrival(&mut self, f: Sent, arrived: u64, now: Instant) {
        let sent = f.at.saturating_duration_since(self.epoch).as_secs_f64() * 1000.0;
        let owd = arrived as f64 / 1000.0 - sent;
        self.min_owd.push(owd, now);
        let queue = owd - self.min_owd.get().unwrap_or(owd);
        // 中间空了很久（画面静止）：之前的样本和现在接不上
        if self
            .samples
            .back()
            .is_some_and(|s| arrived.saturating_sub(s.arrived) > RATE_SPAN_US)
        {
            self.samples.clear();
        }
        self.samples.push_back(Sample {
            arrived,
            before: f.before,
            queue,
        });
        while self
            .samples
            .front()
            .is_some_and(|s| arrived.saturating_sub(s.arrived) > RATE_SPAN_US)
        {
            self.samples.pop_front();
        }
        let n = self.samples.len();
        if n < STANDING {
            return;
        }
        let standing = self
            .samples
            .range(n - STANDING..)
            .map(|s| s.queue)
            .fold(f64::INFINITY, f64::min);
        let signals = Signals {
            standing,
            busy: self.busy_kbps(),
            used: self.used_kbps(),
            srtt: self.srtt_ms,
            blocked: std::mem::take(&mut self.blocked),
        };
        if self.rate.update(now, signals) == Verdict::Rebase {
            self.min_owd = WindowMin::default();
            self.min_owd.push(owd, now);
            self.samples.clear();
        }
    }

    /// 链路一直在忙的那几段（这一帧排的队比和上一次到达的间隔还长）里的交付速率：就是带宽（kbps）
    fn busy_kbps(&self) -> Option<f64> {
        let n = self.samples.len();
        let recent = self.samples.range(n.saturating_sub(STANDING + 1)..);
        let (mut bytes, mut us) = (0u64, 0u64);
        for (a, b) in recent.clone().zip(recent.skip(1)) {
            let gap = b.arrived.saturating_sub(a.arrived);
            if b.queue * 1000.0 >= gap as f64 {
                bytes += b.before - a.before;
                us += gap;
            }
        }
        (us > 0 && bytes > 0).then(|| bytes as f64 * 8000.0 / us as f64)
    }

    /// 最近控制端收到画面的速率（kbps）：编码器实际用了多少码率
    fn used_kbps(&self) -> Option<f64> {
        let (first, last) = (self.samples.front()?, self.samples.back()?);
        let us = last.arrived.saturating_sub(first.arrived);
        (us >= MIN_RATE_SPAN_US).then(|| (last.before - first.before) as f64 * 8000.0 / us as f64)
    }

    /// 最近确认的一帧在网络上比最快的时候多排了多久（毫秒，见模块说明）；控制端没报到达时刻时为 `None`
    pub fn queue_delay(&self) -> Option<f64> {
        self.samples.back().map(|s| s.queue)
    }

    /// 画质设置里的码率（上限）、要不要自适应。码率变了就从这个码率重新开始估计
    pub fn set_target(&mut self, kbps: u32, adaptive: bool) {
        if kbps != self.rate.target {
            self.rate = Rate::new(kbps);
        }
        self.adaptive = adaptive;
    }

    /// 编码器当前使用的码率。
    pub fn bitrate_kbps(&self) -> u32 {
        if self.adaptive {
            self.rate.applied
        } else {
            self.rate.target
        }
    }

    /// 两个相邻报告约覆盖一秒。样本过少时保留已有的码率估计。
    pub fn feedback(&mut self, report: VideoReport, now: Instant) {
        if report.is_empty() || report.stalled > report.frames {
            return;
        }
        let prior = self
            .last_report
            .replace((now, report))
            .filter(|(at, _)| now.saturating_duration_since(*at) < Duration::from_secs(1))
            .map(|(_, r)| r)
            .unwrap_or_default();
        let frames = u64::from(report.frames) + u64::from(prior.frames);
        if frames >= 20 {
            let stalled = u64::from(report.stalled) + u64::from(prior.stalled);
            let clean = 1.0 - stalled as f64 / frames as f64;
            self.rate
                .on_loss(1.0 - clean.powf(1.0 / self.packets_per_frame), now);
        }
    }

    /// 平均一帧多少字节（按编码器现在的码率）
    pub fn frame_bytes(&self, fps: u8) -> usize {
        frame_bytes(self.bitrate_kbps(), fps)
    }

    pub fn inflight(&self) -> u32 {
        self.pending.len() as u32
    }

    pub fn ack_stalled(&self) -> bool {
        self.pending
            .front()
            .is_some_and(|f| f.at.elapsed() > ACK_TIMEOUT)
    }

    /// 在途字节的上限：码率 × (基础 RTT + [`QUEUE_ALLOWANCE_MS`])
    pub fn window_bytes(&self) -> usize {
        let rtt = self.min_rtt.get().unwrap_or(DEFAULT_RTT_MS);
        // kbps × ms / 8 = 字节
        let bytes = self.rate.estimate * (rtt + QUEUE_ALLOWANCE_MS) / 8.0;
        (bytes as usize).clamp(MIN_WINDOW_BYTES, MAX_INFLIGHT_BYTES)
    }

    /// 在途字节挡住了要发的帧（编码器得等、这个会话跳过一帧）：码率控制把它算作拥塞的迹象（见模块说明）
    pub fn note_blocked(&mut self) {
        self.blocked = true;
    }

    /// 能不能再发编码器出的 `bytes` 字节的一帧。没有在途的帧时总能发
    pub fn can_send(&self, bytes: usize) -> bool {
        self.pending.is_empty()
            || (self.pending.len() < MAX_WINDOW as usize
                && self.pending_bytes + bytes <= self.window_bytes())
    }

    /// 在途的、第 `after` 帧之后的帧都作废（控制端等不到它们了，见模块说明的"等不到的帧"）：不再算在途
    pub fn abandon(&mut self, after: u32) {
        let before = self.pending.len();
        self.pending
            .retain(|f| f.seq.wrapping_sub(after) as i32 <= 0);
        if self.pending.len() != before {
            self.pending_bytes = self.pending.iter().map(|f| f.bytes).sum();
        }
    }

    /// 控制端交给解码器的最后一帧是 `decoded`，之后的等不到了（`ControllerDatagram::Lost`）。返回 `true`：
    /// 在途的、`decoded` 之后的帧作废了，它们的流也该放弃；`false`：恢复帧已经在路上了，重复的请求；或者只能编关键帧、
    /// 等不到的帧还会重传、这是第一次报告（见 [`crate::refs`]）
    pub fn lost(&mut self, decoded: u32, now: Instant) -> bool {
        let srtt = if self.srtt_ms > 0.0 {
            self.srtt_ms
        } else {
            DEFAULT_RTT_MS
        };
        if !self.refs.lost(decoded, srtt, now) {
            return false;
        }
        self.abandon(decoded);
        true
    }

    /// (平滑的 RTT, 基础 RTT)，毫秒；还没有样本时是 0
    pub fn rtt(&self) -> (f64, f64) {
        (self.srtt_ms, self.min_rtt.get().unwrap_or(0.0))
    }

    /// 这一路画面停了（不再看这个显示器，见 `airhand_proto::session` 的"多屏"）：在途的帧不用再等确认了，
    /// 晚到的确认也不算。码率估计留着：再看这个显示器时接着用（新的编码管线从关键帧开始）
    pub fn stopped(&mut self) {
        self.pending.clear();
        self.pending_bytes = 0;
        self.acked = self.sent;
    }

    /// 画面换了一条路（见模块说明的"换路"）。`lost`：旧路断了，在途的帧不会确认了
    pub fn path_changed(&mut self, lost: bool) {
        self.min_rtt = WindowMin::default();
        self.min_owd = WindowMin::default();
        self.samples.clear();
        // 还在旧路上的帧确认回来时不算时延样本（旧路快的话，新路会被当成一直在排队）
        self.old_path = Some(self.sent);
        self.last_report = None;
        self.packets_per_frame = 1.0;
        if lost {
            self.pending.clear();
            self.pending_bytes = 0;
            self.acked = self.sent;
        }
    }
}

/// 码率控制收到的信号（见模块说明）
#[derive(Debug, Clone, Copy)]
struct Signals {
    /// 最近 [`STANDING`] 次确认里排队最少的（毫秒）
    standing: f64,
    /// 排着队时量到的带宽（kbps）
    busy: Option<f64>,
    /// 编码器最近实际用了多少码率（kbps）
    used: Option<f64>,
    /// 平滑的 RTT（毫秒）
    srtt: f64,
    /// 在途字节挡住过要发的帧
    blocked: bool,
}

#[derive(Debug, PartialEq)]
enum Verdict {
    Keep,
    /// 排队降不下来：以现在的单向时延为新的基准
    Rebase,
}

#[derive(Debug)]
struct Rate {
    /// 画质设置里的码率（上限）
    target: u32,
    /// 估计值（kbps）
    estimate: f64,
    /// 交给编码器的（kbps）
    applied: u32,
    /// 上次往上加的时刻（一直不排队时才有）
    raised: Option<Instant>,
    /// 降了之后，这之前不再动
    hold: Option<Instant>,
    /// 这一轮排队（超过 [`QUEUE_HIGH_MS`] 到降回 [`QUEUE_LOW_MS`] 以下）里降了几次、上次看时排了多久
    episode: Option<(u32, f64)>,
    /// 上次拥塞时量到的带宽（kbps）：估计值接近它时慢慢加
    capacity: Option<f64>,
    /// 最近报告的丢包率和报告的时刻（见模块说明的"丢包"）
    loss: Option<(f64, Instant)>,
    /// 上次因为丢包降码率的时刻
    loss_cut: Option<Instant>,
}

impl Rate {
    fn new(target: u32) -> Self {
        Self {
            target,
            estimate: target as f64,
            applied: target,
            raised: None,
            hold: None,
            episode: None,
            capacity: None,
            loss: None,
            loss_cut: None,
        }
    }

    /// 报告了最近的丢包率（见模块说明的"丢包"）：太多了就降
    fn on_loss(&mut self, loss: f64, now: Instant) {
        self.loss = Some((loss, now));
        if loss <= LOSS_HIGH || self.loss_cut.is_some_and(|t| now < t + LOSS_CUT_EVERY) {
            return;
        }
        self.loss_cut = Some(now);
        self.raised = None;
        self.estimate = (self.estimate * (1.0 - loss.min(1.0) / 2.0)).max(self.floor());
        self.apply();
    }

    /// 最近丢包不少（超过 [`LOSS_LOW`]）：不再往上加
    fn lossy(&self, now: Instant) -> bool {
        self.loss
            .is_some_and(|(l, at)| l > LOSS_LOW && now < at + LOSS_TTL)
    }

    /// 最低降到这么多
    fn floor(&self) -> f64 {
        (self.target / 6).max(300).min(self.target) as f64
    }

    fn update(&mut self, now: Instant, s: Signals) -> Verdict {
        let held = self.hold.is_some_and(|t| now < t);
        // 降了之后多久再看：一个往返加上排着的队
        let wait = Duration::from_secs_f64(((s.srtt + s.standing) / 1000.0).clamp(0.1, 1.0));
        if s.standing > QUEUE_HIGH_MS || (s.blocked && s.standing > QUEUE_LOW_MS) {
            self.raised = None;
            if held {
                return Verdict::Keep;
            }
            match self.episode {
                // 降过之后正在消下去：再等等
                Some((times, last)) if s.standing < last * 0.8 => {
                    self.episode = Some((times, s.standing));
                    self.hold = Some(now + wait);
                    return Verdict::Keep;
                }
                // 降了两次还是不见少：不是我们发多了
                Some((times, last)) if times >= 2 && s.standing <= last * 1.1 => {
                    self.episode = None;
                    self.hold = None;
                    return Verdict::Rebase;
                }
                _ => {}
            }
            let next = match s.busy {
                Some(busy) => (busy * 0.85).min(self.estimate * 0.9),
                None => self.estimate * 0.8,
            };
            self.estimate = next.max(self.floor());
            if s.busy.is_some() {
                self.capacity = s.busy;
            }
            let times = self.episode.map_or(0, |(t, _)| t);
            self.episode = Some((times + 1, s.standing));
            self.hold = Some(now + wait);
        } else if s.standing < QUEUE_LOW_MS {
            self.episode = None;
            if held {
                self.raised = None;
                return Verdict::Keep;
            }
            let dt = self.raised.map_or(0.0, |t| {
                now.saturating_duration_since(t).as_secs_f64().min(0.2)
            });
            self.raised = Some(now);
            if s.used.is_some_and(|u| u >= self.estimate * 0.5) && !self.lossy(now) {
                let near = self.capacity.is_some_and(|c| self.estimate >= c * 0.9);
                let per_sec = if near { 0.04 } else { 0.15 };
                self.estimate =
                    (self.estimate * (1.0 + per_sec * dt) + 100.0 * dt).min(self.target as f64);
                // 超过上次拥塞时的带宽不少了：网络变好了
                if self.capacity.is_some_and(|c| self.estimate > c * 1.2) {
                    self.capacity = None;
                }
            }
        } else {
            // 排着一点队：不加也不降
            self.raised = None;
        }
        self.apply();
        Verdict::Keep
    }

    /// 估计值变化 5% 以上（往下的都是）才交给编码器
    fn apply(&mut self) {
        let e = self.estimate.round() as u32;
        if e < self.applied || e >= self.applied + self.applied / 20 || e == self.target {
            self.applied = e;
        }
    }
}

#[derive(Default)]
pub struct Flow {
    state: Mutex<FlowState>,
    cv: Condvar,
}

impl Flow {
    /// Assign and record the next session-wide frame before exposing it to the receiver.
    pub fn sent(&self, bytes: usize) -> u32 {
        let mut state = self.state.lock().unwrap();
        let seq = state.sent.wrapping_add(1);
        state.on_sent(seq, bytes);
        seq
    }
    /// 下一帧怎么编（见 [`crate::refs`]）：`slots` 是编码器能控制的长期参考帧槽位数
    pub fn plan(&self, slots: u8) -> Plan {
        self.state.lock().unwrap().refs.plan(slots, Instant::now())
    }

    /// 控制端等不到帧了，还没编出恢复帧：画面静止时也要编一帧
    pub fn recovering(&self) -> bool {
        self.state.lock().unwrap().refs.recovering()
    }

    /// 编码器按 `plan` 编出了一帧（[`Flow::sent`]，另外记下它参考哪一帧）。返回 (序号, 参考帧)
    pub fn sent_frame(
        &self,
        bytes: usize,
        key: bool,
        plan: Plan,
        report: Option<LtrReport>,
    ) -> (u32, Option<u32>) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let seq = state.sent.wrapping_add(1);
        state.on_sent_at(seq, bytes, now);
        let reference = state.refs.encoded(seq, key, plan, report, now);
        (seq, reference)
    }

    /// 见 [`FlowState::lost`]
    pub fn lost(&self, decoded: u32) -> bool {
        let abandoned = self.state.lock().unwrap().lost(decoded, Instant::now());
        self.cv.notify_all();
        abandoned
    }

    /// A frame assigned a sequence but cancelled before enqueueing needs no ACK.
    pub fn discard(&self, seq: u32) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.pending.iter().position(|f| f.seq == seq) {
            let f = state.pending.remove(index).unwrap();
            state.pending_bytes -= f.bytes;
            state.total -= f.bytes as u64;
        }
        drop(state);
        self.cv.notify_all();
    }
    pub fn ack(&self, seq: u32, arrived: Option<u64>) {
        self.state.lock().unwrap().on_ack(seq, arrived);
        self.cv.notify_all();
    }
    pub fn wake(&self) {
        self.cv.notify_all();
    }
    /// 编码之前：等到在途的少到能再发平均大小的一帧（按现在的码率和 `fps` 估计）
    pub fn wait_window(&self, fps: u8, stop: &AtomicBool, timeout: Duration) -> bool {
        self.wait_until(stop, timeout, |s| s.can_send(s.frame_bytes(fps)))
    }
    /// 编码后知道真实大小，再等字节预算；等待期间必须保留这一帧，否则后续增量帧失去参考。
    pub fn wait_room(&self, bytes: usize, stop: &AtomicBool, timeout: Duration) -> bool {
        self.wait_until(stop, timeout, |s| s.can_send(bytes))
    }
    fn wait_until(
        &self,
        stop: &AtomicBool,
        timeout: Duration,
        ready: impl Fn(&FlowState) -> bool,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        if !ready(&state) {
            state.note_blocked();
        }
        let (state, _) = self
            .cv
            .wait_timeout_while(state, timeout, |s| {
                !ready(s) && !stop.load(Ordering::Acquire)
            })
            .unwrap();
        !stop.load(Ordering::Acquire) && ready(&state)
    }
    pub fn ack_stalled(&self) -> bool {
        self.state.lock().unwrap().ack_stalled()
    }
    /// 见 [`FlowState::stopped`]
    pub fn stopped(&self) {
        self.state.lock().unwrap().stopped();
        self.cv.notify_all();
    }

    /// 见 [`FlowState::path_changed`]
    pub fn path_changed(&self, lost: bool) {
        self.state.lock().unwrap().path_changed(lost);
        self.cv.notify_all();
    }
    /// 见 [`FlowState::set_target`]
    pub fn set_target(&self, kbps: u32, adaptive: bool) {
        self.state.lock().unwrap().set_target(kbps, adaptive);
        self.cv.notify_all();
    }
    /// 见 [`FlowState::bitrate_kbps`]
    pub fn bitrate_kbps(&self) -> u32 {
        self.state.lock().unwrap().bitrate_kbps()
    }

    /// 见 [`FlowState::feedback`]
    pub fn feedback(&self, report: VideoReport) {
        self.state.lock().unwrap().feedback(report, Instant::now());
        // 更新码率估计后重新检查在途窗口
        self.cv.notify_all();
    }

    /// 在途的帧有几个（测试用）
    #[cfg(test)]
    pub(crate) fn inflight_for_test(&self) -> u32 {
        self.state.lock().unwrap().inflight()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一路画面停了（多屏时不再看这个显示器）：在途的帧不再算，晚到的确认也不算；之后发的照常算
    #[test]
    fn a_stopped_stream_forgets_its_frames_in_flight() {
        let mut f = FlowState::default();
        f.on_sent(1, 100);
        f.on_sent(2, 100);
        f.stopped();
        assert_eq!(f.inflight(), 0);
        assert!(!f.ack_stalled());
        f.on_ack(2, None);
        assert_eq!(f.inflight(), 0);
        f.on_sent(3, 100);
        assert_eq!(f.inflight(), 1);
        f.on_ack(3, None);
        assert_eq!(f.inflight(), 0);
    }

    #[test]
    fn rejects_future_and_stale_ack_and_counts_only_sent_frames() {
        let mut f = FlowState::default();
        f.on_ack(100, None);
        assert_eq!(f.inflight(), 0);
        f.on_sent(1, 100);
        f.on_ack(100, None);
        assert_eq!(f.inflight(), 1);
        f.on_ack(1, None);
        f.on_sent(1000, 100);
        assert_eq!(f.inflight(), 1);
        f.on_ack(1, None);
        assert_eq!(f.inflight(), 1);
        f.on_ack(1000, None);
        assert_eq!(f.inflight(), 0);
    }

    #[test]
    fn sequence_survives_pipeline_replacement_and_wraps() {
        let f = Flow::default();
        assert_eq!(f.sent(100), 1);
        // A replacement pipeline uses the same session flow, including late ACKs.
        assert_eq!(f.sent(100), 2);
        f.ack(1, None);
        assert!(f.wait_window(60, &AtomicBool::new(false), Duration::ZERO));
        let mut state = FlowState::default();
        state.on_sent(u32::MAX, 100);
        state.on_sent(0, 100);
        state.on_ack(0, None);
        assert_eq!(state.inflight(), 0);
    }

    /// 直连断了：断在上面的帧不再占着窗口，之后（新路上）的确认照常；迟到的旧确认不算。
    /// 只是换了路（两条都通）：在途的照样等确认，时延的基准从头量
    #[test]
    fn a_lost_path_frees_the_window() {
        let f = Flow::default();
        let stop = AtomicBool::new(false);
        let window = f.state.lock().unwrap().window_bytes();
        let lost = f.sent(window);
        assert!(!f.wait_room(1, &stop, Duration::ZERO));
        f.path_changed(false);
        assert!(!f.wait_room(1, &stop, Duration::ZERO));
        f.path_changed(true);
        assert!(f.wait_room(window, &stop, Duration::ZERO));
        let next = f.sent(100);
        f.ack(lost, Some(1));
        assert_eq!(f.state.lock().unwrap().inflight(), 1);
        f.ack(next, Some(2));
        assert_eq!(f.state.lock().unwrap().inflight(), 0);
        {
            let s = f.state.lock().unwrap();
            assert!(s.min_owd.get().is_some() && s.min_rtt.get().is_some());
        }
        // 换到一条慢一些的路：旧路上的帧确认回来时不算样本，新路上的才算
        let old = f.sent(100);
        f.path_changed(false);
        {
            let s = f.state.lock().unwrap();
            assert!(s.min_owd.get().is_none() && s.min_rtt.get().is_none());
        }
        let new = f.sent(100);
        f.ack(old, Some(10_000));
        assert!(f.state.lock().unwrap().min_owd.get().is_none());
        assert_eq!(f.state.lock().unwrap().inflight(), 1);
        f.ack(new, Some(90_000));
        let s = f.state.lock().unwrap();
        assert!(s.min_owd.get().is_some() && s.min_rtt.get().is_some());
        assert_eq!(s.queue_delay(), Some(0.0));
    }

    /// 控制端等不到帧了：它解到的那一帧之后在途的帧不再占着窗口；之后的确认照常，作废了的帧的确认不算
    #[test]
    fn lost_frames_free_the_window() {
        let f = Flow::default();
        let stop = AtomicBool::new(false);
        let window = f.state.lock().unwrap().window_bytes();
        let mut seqs = Vec::new();
        for _ in 0..4 {
            let plan = f.plan(0);
            seqs.push(f.sent_frame(window / 4, false, plan, None).0);
        }
        assert!(!f.wait_room(window / 4, &stop, Duration::ZERO));
        f.ack(seqs[0], Some(1));
        // 控制端解到了第 2 帧（它的确认还在路上），之后的等不到了。编码器不能控制长期参考帧，只能编关键帧：
        // 第一次报告先不管（重传多半还在路上），在途的帧照样等着
        assert!(!f.lost(seqs[1]));
        assert!(!f.recovering());
        assert_eq!(f.state.lock().unwrap().inflight(), 3);
        // 控制端重报了：第 3、4 帧作废
        let again = Instant::now() + crate::refs::KEY_REPEAT_MIN;
        assert!(f.state.lock().unwrap().lost(seqs[1], again));
        assert_eq!(f.state.lock().unwrap().inflight(), 1);
        assert_eq!(f.state.lock().unwrap().pending_bytes, window / 4);
        // 恢复：编码器不能控制长期参考帧，编关键帧；关键帧在路上时重复的请求不算
        assert!(f.recovering());
        assert!(f.plan(0).key);
        let (key, reference) = f.sent_frame(100, true, f.plan(0), None);
        assert_eq!(reference, None);
        assert!(!f.recovering());
        assert!(!f.lost(seqs[1]));
        assert_eq!(f.state.lock().unwrap().inflight(), 2);
        // 作废了的帧的确认（控制端跳过它们时替界面确认的）：确认了第 2 帧，不动关键帧
        f.ack(seqs[3], Some(2));
        assert_eq!(f.state.lock().unwrap().inflight(), 1);
        f.ack(key, Some(3));
        assert_eq!(f.state.lock().unwrap().inflight(), 0);
        // 之后的增量帧参考关键帧
        assert_eq!(f.sent_frame(100, false, f.plan(0), None).1, Some(key));
    }

    /// 丢包：超过 LOSS_HIGH 降（每 LOSS_CUT_EVERY 最多一次），LOSS_LOW 到 LOSS_HIGH 之间不再加，少了照常加
    #[test]
    fn heavy_loss_cuts_and_moderate_loss_holds_the_bitrate() {
        let mut r = Rate::new(10_000);
        let t0 = Instant::now();
        r.on_loss(0.2, t0);
        assert_eq!(r.applied, 9_000);
        // 同一段丢包的下一次报告：不再降
        r.on_loss(0.2, t0 + LOSS_CUT_EVERY / 2);
        assert_eq!(r.applied, 9_000);
        r.on_loss(0.2, t0 + LOSS_CUT_EVERY);
        assert_eq!(r.applied, 8_100);
        // 丢得再多也不低于下限
        for i in 2..40 {
            r.on_loss(1.0, t0 + LOSS_CUT_EVERY * i);
        }
        assert_eq!(r.estimate, r.floor());

        let mut r = Rate::new(10_000);
        r.on_loss(0.2, t0);
        let calm = |used| Signals {
            standing: 0.0,
            busy: None,
            used: Some(used),
            srtt: 50.0,
            blocked: false,
        };
        let grows = |r: &mut Rate, at: Instant| {
            let before = r.estimate;
            r.update(at, calm(r.estimate));
            r.update(at + Duration::from_millis(100), calm(r.estimate));
            r.raised = None;
            r.estimate > before
        };
        // 5%：不降，也不加
        let t1 = t0 + LOSS_CUT_EVERY * 2;
        r.on_loss(0.05, t1);
        assert_eq!(r.applied, 9_000);
        assert!(!grows(&mut r, t1));
        // 不怎么丢了：照常加
        r.on_loss(0.01, t1 + Duration::from_secs(1));
        assert!(grows(&mut r, t1 + Duration::from_secs(1)));
        // 丢包率很久没更新（画面停了、控制端不报告）：不算数
        r.on_loss(0.05, t1 + Duration::from_secs(2));
        assert!(!grows(&mut r, t1 + Duration::from_secs(2)));
        assert!(grows(&mut r, t1 + Duration::from_secs(2) + LOSS_TTL));
    }

    #[test]
    fn stream_reports_need_enough_recent_frames() {
        let mut f = FlowState::default();
        let now = Instant::now();
        let r = VideoReport {
            frames: 10,
            stalled: 5,
        };
        let target = f.bitrate_kbps();
        f.feedback(r, now);
        f.feedback(r, now + Duration::from_secs(3));
        assert_eq!(f.bitrate_kbps(), target);
        f.feedback(r, now + Duration::from_millis(3500));
        assert_eq!(f.bitrate_kbps(), target * 3 / 4);
    }

    #[test]
    fn missing_video_ack_expires_even_when_control_still_works() {
        let mut flow = FlowState::default();
        flow.on_sent(1, 100);
        flow.pending.front_mut().unwrap().at =
            Instant::now() - ACK_TIMEOUT - Duration::from_secs(1);
        assert!(flow.ack_stalled());
        flow.on_ack(1, None);
        assert!(!flow.ack_stalled());
    }

    /// 在途字节按 码率 × (基础 RTT + 允许排的队) 算；没有在途的帧时再大也能发一帧
    #[test]
    fn the_window_is_bytes_at_the_current_rate() {
        let mut f = FlowState::default();
        f.set_target(4000, true);
        // 还没有 RTT 样本：按 100 ms 算，4000 kbps × 150 ms = 75 000 字节
        assert_eq!(f.window_bytes(), 75_000);
        f.min_rtt.push(30.0, Instant::now());
        assert_eq!(f.window_bytes(), 40_000);
        assert!(f.can_send(10 * 1024 * 1024));
        f.on_sent(1, 30_000);
        assert!(f.can_send(10_000));
        assert!(!f.can_send(10_001));
        f.on_ack(1, None);
        assert!(f.can_send(10 * 1024 * 1024));
        // 很慢的码率也至少有 16 KiB；很快的码率、很长的 RTT 也不超过上限
        f.set_target(100, true);
        assert_eq!(f.window_bytes(), MIN_WINDOW_BYTES);
        f.set_target(100_000, true);
        f.min_rtt.push(5_000.0, Instant::now() + MIN_RTT_WINDOW * 2);
        assert_eq!(f.window_bytes(), MAX_INFLIGHT_BYTES);
        // 帧数也有上限
        f.set_target(4000, true);
        let mut f2 = FlowState::default();
        for seq in 1..=MAX_WINDOW {
            assert!(f2.can_send(1));
            f2.on_sent(seq, 1);
        }
        assert!(!f2.can_send(1));
    }

    #[test]
    fn byte_budget_is_released_by_ack_and_cancelled_frames() {
        let f = Flow::default();
        let stop = AtomicBool::new(false);
        let window = f.state.lock().unwrap().window_bytes();
        let seq = f.sent(window - 100);
        assert!(f.wait_room(100, &stop, Duration::ZERO));
        assert!(!f.wait_room(101, &stop, Duration::ZERO));
        f.ack(seq, None);
        assert!(f.wait_room(MAX_INFLIGHT_BYTES * 2, &stop, Duration::ZERO));
        let seq = f.sent(window);
        assert!(!f.wait_window(60, &stop, Duration::ZERO));
        f.discard(seq);
        assert!(f.wait_window(60, &stop, Duration::ZERO));
        assert_eq!(f.state.lock().unwrap().total, (window - 100) as u64);
    }

    #[test]
    fn stopped_pipeline_cannot_emit_another_frame() {
        assert!(!Flow::default().wait_window(30, &AtomicBool::new(true), Duration::ZERO));
    }

    #[test]
    fn min_rtt_is_a_sliding_window_minimum() {
        let mut f = FlowState::default();
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        f.min_rtt.push(50.0, at(0));
        f.min_rtt.push(120.0, at(1));
        f.min_rtt.push(80.0, at(2));
        assert_eq!(f.rtt().1, 50.0);
        // 50 过了窗口：最小值是窗口里剩下的 80，而不是刚来的那个样本
        f.min_rtt.push(200.0, at(11));
        assert_eq!(f.rtt().1, 80.0);
        f.min_rtt.push(300.0, at(13));
        assert_eq!(f.rtt().1, 200.0);
        // 很久没有样本（画面静止）也保留最后的值，下一个样本来了再替换
        f.min_rtt.push(250.0, at(100));
        assert_eq!(f.rtt().1, 250.0);
    }

    /// 到达时刻和发送时刻不在同一个时钟上：只看比最快的时候慢了多少
    #[test]
    fn queue_delay_is_relative_to_the_fastest_frame() {
        let mut f = FlowState::default();
        assert_eq!(f.queue_delay(), None);
        // 控制端的时钟比这边早了一小时；第 1 帧网络上用了 20 ms，第 2 帧 70 ms
        let skew = 3_600_000_000u64;
        let sent = |f: &mut FlowState, seq: u32, at_ms: u64| {
            f.on_sent(seq, 100);
            f.pending.back_mut().unwrap().at = f.epoch + Duration::from_millis(at_ms);
        };
        sent(&mut f, 1, 0);
        f.on_ack(1, Some(skew + 20_000));
        assert_eq!(f.queue_delay(), Some(0.0));
        sent(&mut f, 2, 100);
        f.on_ack(2, Some(skew + 170_000));
        assert!((f.queue_delay().unwrap() - 50.0).abs() < 1e-6);
        // 没报到达时刻的确认不动它
        sent(&mut f, 3, 200);
        f.on_ack(3, None);
        assert!((f.queue_delay().unwrap() - 50.0).abs() < 1e-6);
    }

    // ───────────────────────────── 码率控制 ─────────────────────────────

    /// 模拟的一条路：瓶颈链路有 `kbps` 的带宽和无限大的缓冲区，单程 `prop_ms`
    struct Link {
        kbps: f64,
        prop_ms: f64,
        /// 瓶颈链路什么时候发完手里的（毫秒）
        free: f64,
    }

    impl Link {
        fn new(kbps: f64, prop_ms: f64) -> Self {
            Self {
                kbps,
                prop_ms,
                free: 0.0,
            }
        }

        /// `at` 毫秒时发出一帧：返回 (开始到达控制端, 确认回到被控端) 的时刻（毫秒）
        fn send(&mut self, at: f64, bytes: usize) -> (f64, f64) {
            let start = at.max(self.free);
            self.free = start + bytes as f64 * 8.0 / self.kbps;
            (start + self.prop_ms, self.free + 2.0 * self.prop_ms)
        }
    }

    /// 一个跑着的会话：编码器每帧按 [`FlowState::bitrate_kbps`] 出一帧，在途字节不够就跳过这一帧，
    /// 控制端每帧都确认
    struct Sim {
        f: FlowState,
        link: Link,
        t0: Instant,
        /// 现在（毫秒）
        now: f64,
        seq: u32,
        /// (确认回来的时刻, 序号, 开始到达的时刻（控制端的时钟，微秒）)
        acks: VecDeque<(f64, u32, u64)>,
        /// 最近一秒发出的每一帧：(发出的时刻, 排了多久的队)，毫秒
        queues: VecDeque<(f64, f64)>,
        fps: u8,
    }

    impl Sim {
        fn new(target: u32, link: Link) -> Self {
            let mut f = FlowState::default();
            f.set_target(target, true);
            let t0 = f.epoch;
            Self {
                f,
                link,
                t0,
                now: 0.0,
                seq: 0,
                acks: VecDeque::new(),
                queues: VecDeque::new(),
                fps: 60,
            }
        }

        fn at(&self, ms: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(ms / 1000.0)
        }

        /// 跑 `secs` 秒
        fn run(&mut self, secs: f64) {
            let frame = 1000.0 / self.fps as f64;
            let end = self.now + secs * 1000.0;
            while self.now < end {
                while let Some(&(t, seq, arrived)) = self.acks.front() {
                    if t > self.now {
                        break;
                    }
                    self.acks.pop_front();
                    self.f.on_ack_at(seq, Some(arrived), self.at(t));
                }
                let bytes = frame_bytes(self.f.bitrate_kbps(), self.fps);
                if !self.f.can_send(bytes) {
                    self.f.note_blocked();
                } else {
                    self.seq += 1;
                    self.f.on_sent_at(self.seq, bytes, self.at(self.now));
                    let (first, ack) = self.link.send(self.now, bytes);
                    // 控制端的时钟比这边早 5 秒
                    let arrived = ((first + 5000.0) * 1000.0) as u64;
                    self.acks.push_back((ack, self.seq, arrived));
                    self.queues
                        .push_back((self.now, first - self.link.prop_ms - self.now));
                }
                self.now += frame;
                while self
                    .queues
                    .front()
                    .is_some_and(|&(t, _)| t < self.now - 1000.0)
                {
                    self.queues.pop_front();
                }
            }
        }

        /// 最近一秒排队最长的一帧（毫秒）
        fn worst_queue(&self) -> f64 {
            self.queues.iter().map(|&(_, q)| q).fold(0.0, f64::max)
        }
    }

    /// 网络通畅：一直是画质设置里的码率，不排队
    #[test]
    fn a_clear_path_keeps_the_target() {
        let mut s = Sim::new(8000, Link::new(50_000.0, 20.0));
        s.run(10.0);
        assert_eq!(s.f.bitrate_kbps(), 8000);
        assert!(s.worst_queue() < 1.0, "{}", s.worst_queue());
    }

    /// 带宽比码率低：几百毫秒之内降到带宽以下，之后围着带宽上下，排队不超过允许的
    #[test]
    fn follows_a_bandwidth_drop_quickly() {
        let mut s = Sim::new(8000, Link::new(50_000.0, 20.0));
        s.run(3.0);
        s.link.kbps = 3000.0;
        s.run(0.5);
        let kbps = s.f.bitrate_kbps();
        assert!(kbps <= 3000, "{kbps}");
        s.run(10.0);
        let kbps = s.f.bitrate_kbps();
        assert!((2000..=3400).contains(&kbps), "{kbps}");
        assert!(s.worst_queue() < 80.0, "{}", s.worst_queue());
        // 带宽回来了：逐步回到画质设置里的码率
        s.link.kbps = 50_000.0;
        s.run(15.0);
        assert_eq!(s.f.bitrate_kbps(), 8000);
    }

    /// 一开始码率就比带宽高：在途字节限住了排队，码率很快降下来
    #[test]
    fn starts_too_high() {
        let mut s = Sim::new(8000, Link::new(2000.0, 40.0));
        s.run(2.0);
        let kbps = s.f.bitrate_kbps();
        assert!(kbps <= 2000, "{kbps}");
        s.run(10.0);
        assert!(s.worst_queue() < 100.0, "{}", s.worst_queue());
    }

    /// 路本身变慢了（换了路由，单程多了 40 ms）：降两次之后以新的时延为基准，再回到原来的码率
    #[test]
    fn a_slower_route_is_not_congestion() {
        let mut s = Sim::new(4000, Link::new(50_000.0, 20.0));
        s.run(3.0);
        s.link.prop_ms = 60.0;
        let mut low = u32::MAX;
        for _ in 0..30 {
            s.run(0.1);
            low = low.min(s.f.bitrate_kbps());
        }
        // 只降了两次
        assert!((2800..4000).contains(&low), "{low}");
        s.run(10.0);
        assert_eq!(s.f.bitrate_kbps(), 4000);
        assert!(s.worst_queue() < 1.0, "{}", s.worst_queue());
    }

    /// 偶尔一帧慢（Wi-Fi 的抖动）不算拥塞：看的是最近几帧里最少的
    #[test]
    fn a_single_slow_frame_is_not_congestion() {
        let mut r = Rate::new(4000);
        let now = Instant::now();
        let s = |standing| Signals {
            standing,
            busy: None,
            used: Some(4000.0),
            srtt: 50.0,
            blocked: false,
        };
        // 最小值低于门槛：不降
        r.update(now, s(QUEUE_HIGH_MS));
        assert_eq!(r.applied, 4000);
        r.update(now, s(QUEUE_HIGH_MS + 1.0));
        assert_eq!(r.applied, 3200);
    }

    /// 在途字节把队压在门槛以下、只好跳帧：也算拥塞。没排队时被挡住不算（大的关键帧独自在路上）
    #[test]
    fn window_limited_while_queueing_is_congestion() {
        let now = Instant::now();
        let s = |standing, blocked| Signals {
            standing,
            busy: Some(3000.0),
            used: Some(4000.0),
            srtt: 50.0,
            blocked,
        };
        let mut r = Rate::new(4000);
        r.update(now, s(QUEUE_LOW_MS - 1.0, true));
        r.update(now, s(QUEUE_HIGH_MS - 1.0, false));
        assert_eq!(r.applied, 4000);
        r.update(now, s(QUEUE_LOW_MS + 1.0, true));
        assert_eq!(r.applied, 2550);
    }

    /// 画面静止、编码器用不了多少码率时不往上加；用起来了才加
    #[test]
    fn no_raise_without_evidence() {
        let mut r = Rate::new(4000);
        r.estimate = 1000.0;
        r.applied = 1000;
        let t0 = Instant::now();
        let idle = Signals {
            standing: 0.0,
            busy: None,
            used: Some(100.0),
            srtt: 50.0,
            blocked: false,
        };
        for i in 0..100 {
            r.update(t0 + Duration::from_millis(i * 100), idle);
        }
        assert_eq!(r.applied, 1000);
        for i in 100..200 {
            let busy = Signals {
                used: Some(r.estimate),
                ..idle
            };
            r.update(t0 + Duration::from_millis(i * 100), busy);
        }
        assert!(r.applied > 2500, "{}", r.applied);
    }

    /// 不自适应：编码器一直用画质设置里的码率，在途字节照样按估计值限：少发几帧，排队不会越积越多。
    /// 码率变了从新的码率开始估计；只打开 / 关掉自适应时接着用现在的估计
    #[test]
    fn fixed_bitrate() {
        let mut s = Sim::new(8000, Link::new(2000.0, 20.0));
        s.f.set_target(8000, false);
        s.run(5.0);
        assert_eq!(s.f.bitrate_kbps(), 8000);
        // 一帧要传 67 ms：最多排在一帧后面
        assert!(s.worst_queue() < 70.0, "{}", s.worst_queue());
        s.f.set_target(8000, true);
        assert!(s.f.bitrate_kbps() < 3000, "{}", s.f.bitrate_kbps());
        s.f.set_target(1000, true);
        assert_eq!(s.f.bitrate_kbps(), 1000);
    }

    /// 往上加的时候不是每一点变化都交给编码器
    #[test]
    fn small_changes_are_not_applied() {
        let mut r = Rate::new(4000);
        r.estimate = 2000.0;
        r.applied = 2000;
        r.estimate = 2090.0;
        r.apply();
        assert_eq!(r.applied, 2000);
        r.estimate = 2100.0;
        r.apply();
        assert_eq!(r.applied, 2100);
        r.estimate = 2099.0;
        r.apply();
        assert_eq!(r.applied, 2099);
    }
}
