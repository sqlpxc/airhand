//! 一条通道从几条路（中转上的流、直连上的流）收到的帧，按计数排好再解密、交出去（见 `airhand_proto` 的"直连"）。
//! 只有中转一条路的通道（大块通道）也用它：解密、按通道的上限读帧，和别的通道是同一段代码。
//!
//! 每个密封单元末尾的计数是明文（[`crate::secure::counter_of`]），每条路自己是有序的，发送端的计数一个接一个，
//! 所以接收端看计数就知道该交哪一帧、哪一帧是重复的：
//!
//! * **控制、大块通道**：中转上是完整的一份，直连上是同样的帧（发送端来不及时可能少几帧）。
//!   计数正好是下一个的交出去，已经交出去了的（另一条路先到了）丢掉；直连上来早了的先放着，等中转把中间的补上。
//!   中转上的帧必须正好是下一个、或者已经交出去了的：和只有中转时一样，中转删不掉、调换不了一条消息
//!   （直连已经送到了的除外，那对接收端没有区别）；
//! * **画面**（[`Merge::next_video`]）：每一帧只走一条路，被控端换路时新路上的帧可能先到，要等旧路上的；
//!   帧流（每帧一条流，见 `airhand_proto::session` 的"帧流"）上后面的帧也可能先到。按计数交出去，但只交
//!   **解得了**的帧（见下面的"解得了"）：解不了的不交给解码器（[`Video::Skipped`]，控制端替界面确认）。
//!
//! # 解得了
//!
//! 每一帧依赖什么看画面包头（`airhand_proto::media` 的"参考帧"）：关键帧什么都不依赖；带参考帧的增量帧依赖那一帧
//! （它交给过解码器就解得了，中间缺了哪些帧不要紧）；省略参考帧扩展时依赖计数紧挨着的前一帧。
//!
//! 缺着下一帧的时候：
//!
//! * 手里有解得了的后面的帧（关键帧、从交给过解码器的帧恢复的那一帧）：不等了，从手里最早的一帧接着交，解不了的跳过；
//! * 后面的帧到了、下一帧却停住了比 QUIC 重传还久（[`loss_wait`]），就报告等不到了
//!   （[`Video::Lost`]，控制端发 `ControllerDatagram::Lost`），之后定时再报，直到解得了的帧到了。等着的期间
//!   后面的帧先留着：重传补上了下一帧，它们照样解得了。"停住"看的是读到一半的帧流：QUIC 在几条流之间
//!   轮流发，大的帧（关键帧）本来就比跟在后面的小帧晚到，它还在一点一点地到就不是丢了；丢了包的流在重传到之前
//!   什么也读不到（QUIC 只按顺序交出数据）。所以读到一半的帧流里最久没有新数据的那条停了这么久才算（没有读到
//!   一半的：缺的那一帧连开头都没到，从缺着的时候算起）；一直有新的小帧到，不影响卡住的那条算不算停住；
//! * 等了 [`VIDEO_WAIT`] 还没恢复，通过控制通道要一个关键帧（[`Video::Keyframe`]）。
//!
//! 每条路（每条流）一个读任务，把读到的帧放进同一个信箱。对方的流（中转上的那条、每次直连通了之后直连上的那条、
//! 每一条帧流）从这条通道的信箱（[`crate::lanes::Inbox`]）来。
//!
//! # 报告
//!
//! 每路统计收完的帧和其中因重传停顿过的帧（[`VideoStats`]、[`Ticks`]），定时报告给被控端的码率控制。
//!
//! # 到达时刻
//!
//! 交出去的每一帧（解得了的、跳过的）带着它的到达时刻（[`Frame::arrived`]），控制端在确认里告诉被控端，
//! 被控端的码率控制拿它算排队时延（`crate::flow`）。它是这一帧开始到达（第一个字节、第一块到了）和前一帧交出去
//! 这两个时刻里晚的那个，和以前画面是一条有序流时读到这一帧开头的时刻一样：前面的帧在等重传时，
//! 后面的帧即使早就到了也算晚到，码率控制把它当成排队、马上降码率。要的就是这个：公网上（尤其家宽的上行）
//! 大流量的 UDP 常被限速，限速器超了就丢、不排队，只看每帧自己的到达时刻时码率降不下来，一直丢、一直等重传，
//! 延迟翻倍（见 `crate::flow` 的"到达时刻"）。前一帧交出去的时刻是合并交出它的时刻（[`VideoCore::pop`] 的 `now`）。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_proto::SessionLane;
use airhand_proto::media::{Kind, VideoHeader};
use airhand_proto::session::VideoReport;
use anyhow::{Context, Result, bail};
use quinn::RecvStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::debug;

use crate::clock::now_us;
use crate::lanes::{LaneStream, LaneStreams, Via};
use crate::secure::{Opener, counter_of};
use crate::wire::read_packet_watched;

/// 画面等前面的帧最多等这么久，之后通过控制通道要一个关键帧。
pub const VIDEO_WAIT: Duration = Duration::from_secs(1);
/// 画面：最多先放着这么多帧、这么多字节（被控端在途的帧不超过 `flow::MAX_WINDOW` 帧、`flow::MAX_INFLIGHT_BYTES`），
/// 再多就不等了
const MAX_HELD_FRAMES: usize = 2 * crate::flow::MAX_WINDOW as usize;
const MAX_HELD_VIDEO_BYTES: usize = 8 * crate::flow::MAX_INFLIGHT_BYTES;
/// 控制、大块通道：直连上来早了的帧最多先放着这么多字节，再多就丢掉（中转上还有一份）
const MAX_HELD_BYTES: usize = 8 * 1024 * 1024;
/// 画面：记着最近交给解码器的这么多帧（带参考帧的增量帧查它）。长期参考帧最多旧到几秒钟之前
const DECODED: usize = 1024;
/// 还没量到往返时延时按这么多算（毫秒）
const DEFAULT_RTT_MS: f64 = 100.0;
/// 帧流上一帧读到一半、别的帧流照样在来的时候停了这么久（后面的帧又先到了），算是等重传卡住过
/// （见模块说明的“报告”和 [`Ticks`]）。
pub const STALL: Duration = Duration::from_millis(15);

/// 缺着下一帧、后面的帧到了，等多久才报告等不到了：一个半往返再加 30 ms（60 ms 到 800 ms 之间）。
///
/// 要等得比 QUIC 的重传晚：发送端在丢的包发出 9/8 个往返之后（或者后面三个包确认了）才断定它丢了、重发，
/// 重发的再走半个往返；确认还可能晚到（最多 25 ms 的确认延迟）。这里从后面的帧到了（读到一半的帧流最后一次有数据）
/// 算起，那时丢的包本该已经到了，所以重传大约在之后 9/8 个往返加上确认延迟到。只等一个往返的话一半左右会抢在重传前面
/// （`host::write` 的 `losses_are_not_reported_before_the_retransmission`），白白放弃在途的帧、编恢复帧（甚至关键帧）。
/// 经过中转时每一段各自重传，那一段的往返比整个往返短，重传到得更早。
///
/// 重传也丢了、路上堵着时晚半个往返才恢复：比每丢一个包就恢复一次划算（恢复帧参考的是更早的帧，更大；
/// 编码器不能控制长期参考帧时就是关键帧）
pub fn loss_wait(rtt_ms: f64) -> Duration {
    Duration::from_secs_f64(((rtt_ms * 1.5 + 30.0) / 1000.0).clamp(0.06, 0.8))
}

/// 报告了等不到之后，多久还没恢复就再报一次（数据报会丢）
fn loss_repeat(rtt_ms: f64) -> Duration {
    Duration::from_secs_f64((rtt_ms * 2.0 / 1000.0).clamp(0.1, 1.0))
}

/// 交出去的一帧（已经解密）
#[derive(Debug)]
pub struct Frame {
    pub data: Vec<u8>,
    /// 到达时刻（控制端的时钟，见 [`crate::wire::read_packet_timed`]）：画面交出来的帧是它开始到达、前一帧交出去
    /// 两个里晚的那个（见模块说明的"到达时刻"）；控制、大块通道的是它开始从网络上到达的时刻
    pub arrived: u64,
    /// 从直连来的
    pub direct: bool,
}

/// 画面收得怎么样（见模块说明的"报告"）：合并的一方记，报告的一方定时取走。复制出来的是同一个
#[derive(Clone, Default)]
pub struct VideoStats(Arc<Mutex<VideoReport>>);

impl VideoStats {
    /// 取走到现在为止的（之后从 0 开始数）
    pub fn take(&self) -> VideoReport {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    fn record(&self, stalled: bool) {
        let mut report = self.0.lock().unwrap();
        report.frames += 1;
        report.stalled += u32::from(stalled);
    }
}

/// 画面通道交出来的东西（[`Merge::next_video`]，见模块说明）
#[derive(Debug)]
pub enum Video {
    /// 解得了：交给解码器
    Frame(Frame),
    /// 收到了、解不了（参考的帧没交给解码器）：不交给界面，替它确认（被控端的在途窗口才不会停住）。
    /// `arrived` 和 [`Frame::arrived`] 一样
    Skipped { seq: u32, arrived: u64 },
    /// 等不到一帧了：请被控端从交给解码器的最后一帧（`decoded`，还没有时为 0）恢复。恢复之前定时再来
    Lost { decoded: u32 },
    /// 等了太久还是缺帧：要一个关键帧（`after`：交给解码器的最后一帧）
    Keyframe { after: u32 },
}

struct Held {
    /// 密文（按顺序解密）
    data: Vec<u8>,
    arrived: u64,
    direct: bool,
}

/// 控制、大块通道的合并（不碰 IO，测试直接用）
struct Core {
    opener: Opener,
    /// 下一个要交出去的计数
    next: u64,
    held: BTreeMap<u64, Held>,
    held_bytes: usize,
}

impl Core {
    fn new(opener: Opener) -> Self {
        Self {
            next: opener.expected(),
            opener,
            held: BTreeMap::new(),
            held_bytes: 0,
        }
    }

    /// 收到一帧（密封单元）
    fn push(&mut self, direct: bool, data: Vec<u8>, arrived: u64) -> Result<()> {
        let c = counter_of(&data).context("a session frame is too short")?;
        if c < self.next || self.held.contains_key(&c) {
            // 另一条路先送到了
            return Ok(());
        }
        if !direct && c != self.next {
            bail!("a session frame was dropped or reordered");
        }
        // 直连上来早了：先放着，放不下就算了（中转上还有一份）
        if c != self.next && self.held_bytes + data.len() > MAX_HELD_BYTES {
            return Ok(());
        }
        self.held_bytes += data.len();
        self.held.insert(
            c,
            Held {
                data,
                arrived,
                direct,
            },
        );
        Ok(())
    }

    /// 现在能交出去的下一帧
    fn pop(&mut self) -> Result<Option<Frame>> {
        let Some(mut h) = self.held.remove(&self.next) else {
            return Ok(None);
        };
        self.held_bytes -= h.data.len();
        self.opener.open(&mut h.data)?;
        self.next += 1;
        Ok(Some(Frame {
            data: h.data,
            arrived: h.arrived,
            direct: h.direct,
        }))
    }
}

/// 一帧画面依赖什么（见模块说明的"解得了"）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dep {
    /// 关键帧
    Key,
    /// 这一帧（序号）交给过解码器就解得了
    Ref(u32),
    /// 计数紧挨着的前一帧（不带参考帧的增量帧）
    Prev,
}

struct HeldVideo {
    /// 明文（收到就解开了，要看依赖什么）
    data: Vec<u8>,
    arrived: u64,
    direct: bool,
    seq: u32,
    dep: Dep,
}

/// 画面的合并（不碰 IO，测试直接用）
struct VideoCore {
    opener: Opener,
    /// 下一个要考虑的计数
    next: u64,
    held: BTreeMap<u64, HeldVideo>,
    held_bytes: usize,
    /// 最近交给解码器的帧的序号
    decoded: VecDeque<u32>,
    /// 交给解码器的最后一帧：(计数, 序号)
    last: Option<(u64, u32)>,
    /// 从什么时候开始缺着 `next`、手里却有后面的帧
    waiting: Option<Instant>,
    /// 读到一半的帧流里最久没有新数据的那条，最后一次读到数据的时刻（见模块说明的"停住"）；
    /// 没有读到一半的帧流时为 `None`
    progress: Option<Instant>,
    /// 上次报告等不到的时刻（报告了、还没恢复）
    lost_at: Option<Instant>,
    /// 这一轮已经要过关键帧了
    asked_key: bool,
    /// 往返时延（毫秒）
    rtt_ms: f64,
    /// 报告任务定时取走的帧数与停顿数。
    stats: VideoStats,
    /// 上一帧（解得了的、跳过的）交出去的时刻（控制端的时钟，微秒；还没有时为 0），见模块说明的"到达时刻"
    handed: u64,
    /// 把 [`VideoCore::pop`] 的 `now` 换成控制端的时钟：(某个时刻, 那时的 `now_us()`)
    anchor: (Instant, u64),
}

impl VideoCore {
    fn new(opener: Opener) -> Self {
        Self {
            next: opener.expected(),
            opener,
            held: BTreeMap::new(),
            held_bytes: 0,
            decoded: VecDeque::new(),
            last: None,
            waiting: None,
            progress: None,
            lost_at: None,
            asked_key: false,
            rtt_ms: DEFAULT_RTT_MS,
            stats: VideoStats::default(),
            handed: 0,
            anchor: (Instant::now(), now_us()),
        }
    }

    /// `now` 是控制端的时钟的哪一微秒（和 [`crate::wire::read_packet_timed`] 记的到达时刻同一个时钟）
    fn clock_us(&self, now: Instant) -> u64 {
        let (at, us) = self.anchor;
        us + now.saturating_duration_since(at).as_micros() as u64
    }

    /// 交出第 `c` 帧时它的到达时刻（见模块说明的"到达时刻"）：它开始到达、前一帧交出去，两个里晚的那个
    fn hand_over(&mut self, h: &HeldVideo, now: Instant) -> u64 {
        let arrived = h.arrived.max(self.handed);
        self.handed = self.clock_us(now);
        arrived
    }

    /// 测试用：流上读到的一帧，没停过
    #[cfg(test)]
    fn push(&mut self, direct: bool, data: Vec<u8>, arrived: u64) -> Result<()> {
        self.receive(direct, data, arrived, Duration::ZERO)
    }

    /// 收到一帧（密封单元）：收到就解开（要看它依赖什么）
    fn receive(
        &mut self,
        direct: bool,
        mut data: Vec<u8>,
        arrived: u64,
        gap: Duration,
    ) -> Result<()> {
        let c = counter_of(&data).context("a session frame is too short")?;
        if c < self.next || self.held.contains_key(&c) || !self.opener.is_fresh(c) {
            // 另一条路先送到了、太晚了（已经不等它了：流上的算卡住过）
            if c < self.next && self.opener.is_fresh(c) {
                self.stats.record(true);
            }
            return Ok(());
        }
        self.opener.open(&mut data)?;
        anyhow::ensure!(
            Kind::of(&data) == Some(Kind::Video),
            "unexpected packet on the video stream"
        );
        let h = VideoHeader::decode_any(&data).expect("a video packet");
        let dep = match (h.key, h.reference) {
            (true, _) => Dep::Key,
            (false, Some(r)) => Dep::Ref(r),
            (false, None) => Dep::Prev,
        };
        let overtaken = self.held.keys().next_back().is_some_and(|&l| l > c);
        self.stats.record(gap >= STALL && overtaken);
        self.held_bytes += data.len();
        self.held.insert(
            c,
            HeldVideo {
                data,
                arrived,
                direct,
                seq: h.seq,
                dep,
            },
        );
        Ok(())
    }

    /// 计数 `c` 上的这一帧现在解得了（见模块说明的"解得了"）
    fn decodable(&self, c: u64, h: &HeldVideo) -> bool {
        match h.dep {
            Dep::Key => true,
            Dep::Ref(r) => self.decoded.contains(&r),
            Dep::Prev => self.last.is_some_and(|(last, _)| last + 1 == c),
        }
    }

    fn take(&mut self, c: u64) -> Option<HeldVideo> {
        let h = self.held.remove(&c)?;
        self.held_bytes -= h.data.len();
        Some(h)
    }

    /// 交给解码器的最后一帧的序号（还没有时为 0）
    fn last_seq(&self) -> u32 {
        self.last.map_or(0, |(_, s)| s)
    }

    /// 现在能交出去的下一样东西（见模块说明）
    fn pop(&mut self, now: Instant) -> Option<Video> {
        loop {
            let c = self.next;
            if let Some(h) = self.take(c) {
                self.next += 1;
                let arrived = self.hand_over(&h, now);
                if !self.decodable(c, &h) {
                    return Some(Video::Skipped {
                        seq: h.seq,
                        arrived,
                    });
                }
                self.last = Some((c, h.seq));
                if self.decoded.len() == DECODED {
                    self.decoded.pop_front();
                }
                self.decoded.push_back(h.seq);
                self.waiting = None;
                self.lost_at = None;
                self.asked_key = false;
                return Some(Video::Frame(Frame {
                    data: h.data,
                    arrived,
                    direct: h.direct,
                }));
            }
            let Some(first) = self.held.keys().next().copied() else {
                self.waiting = None;
                return None;
            };
            // 手里有解得了的后面的帧：缺着的不等了，从手里最早的一帧接着交（解不了的跳过）。
            // 手里的帧太多了也不留了：反正解不了
            if self.held.iter().any(|(c, h)| self.decodable(*c, h))
                || self.held.len() >= MAX_HELD_FRAMES
                || self.held_bytes > MAX_HELD_VIDEO_BYTES
            {
                debug!(
                    from = self.next,
                    to = first,
                    "video: not waiting for missing frames"
                );
                self.next = first;
                continue;
            }
            let since = *self.waiting.get_or_insert(now);
            let decoded = self.last_seq();
            if let Some(due) = self.lost_due()
                && now >= due
            {
                debug!(missing = self.next, decoded, "video: a frame isn't coming");
                self.lost_at = Some(now);
                return Some(Video::Lost { decoded });
            }
            if !self.asked_key && now >= since + VIDEO_WAIT {
                self.asked_key = true;
                return Some(Video::Keyframe { after: decoded });
            }
            return None;
        }
    }

    /// 缺着的帧到这个时候还是停着，就报告（再报）等不到了：从开始缺着（上次报告）、帧流上最后一次有数据的时刻
    /// 里晚的那个算起。
    fn lost_due(&self) -> Option<Instant> {
        let since = self.waiting?;
        let (from, wait) = match self.lost_at {
            None => (since, loss_wait(self.rtt_ms)),
            Some(t) => (t, loss_repeat(self.rtt_ms)),
        };
        Some(self.progress.map_or(from, |p| p.max(from)) + wait)
    }

    /// 到这个时候 [`VideoCore::pop`] 可能有新的结果（等着缺的帧时）
    fn deadline(&self) -> Option<Instant> {
        let since = self.waiting?;
        let key = (!self.asked_key).then_some(since + VIDEO_WAIT);
        self.lost_due().into_iter().chain(key).min()
    }
}

enum Cores {
    Complete(Core),
    Video(VideoCore),
}

/// 一个读任务从哪条流读
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// 中转上的那条
    Relay,
    /// 直连上的那条
    Direct,
    /// 一条帧流（`direct`：在直连上）
    Frame { direct: bool, reader: u64 },
}

impl Source {
    fn direct(self) -> bool {
        matches!(self, Source::Direct | Source::Frame { direct: true, .. })
    }
}

struct Event {
    source: Source,
    packet: Result<Option<(Vec<u8>, u64)>>,
    /// 读到一半、别的帧流照样在来的时候最长停了多久
    gap: Duration,
}

/// 中转上的那条流
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relay {
    /// 对方还没开过来
    Waiting,
    Open,
    Ended,
}

/// 一条通道的接收端：几条路上的帧合并成一串（见模块说明）。读任务随它一起结束
pub struct Merge {
    lane: SessionLane,
    core: Cores,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    streams: LaneStreams,
    streams_open: bool,
    relay: Relay,
    /// 还开着的帧流
    frames: usize,
    /// 读到一半的帧流（读任务的编号）和它们最后一次读到数据的时刻：读任务写，合并时看（见模块说明的"停住"）
    partial: Arc<Mutex<HashMap<u64, Instant>>>,
    /// 帧流上最近读到数据的时刻（所有的帧流）：一条帧流停住的时候别的有没有在来（见 [`Ticks`]）
    ticks: Arc<Mutex<Ticks>>,
    readers: JoinSet<()>,
}

/// 读任务的编号（[`Merge::partial`] 的键）
static READERS: AtomicU64 = AtomicU64::new(0);

/// 帧流上最近读到数据的时刻（所有的帧流，最近 [`Ticks::KEEP`] 的）。一条帧流读到一半停住了：是等重传
/// （丢了包，QUIC 只按顺序交出数据：这条流后面的数据到了也读不出来，别的流照样在来），还是整条连接都停了
/// （拥塞窗口满了，谁都发不出去；画面停了，没什么可发）？只有前一种算卡住（[`STALL`]），要看停住的这段时间里
/// 别的帧流有没有在来，而且来了有一阵：拥塞窗口一开，停住的和别的同时接着来
#[derive(Default)]
struct Ticks(VecDeque<Instant>);

impl Ticks {
    const KEEP: Duration = Duration::from_secs(2);

    fn push(&mut self, now: Instant) {
        while self
            .0
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > Self::KEEP)
        {
            self.0.pop_front();
        }
        self.0.push_back(now);
    }

    /// 一条帧流上一次读到数据是 `last`，现在又读到了：这期间别的帧流已经在来、它却还停着的时间
    /// （它自己在这期间没读到过，`last` 之后的都是别的流的）
    fn blocked(&self, last: Instant, now: Instant) -> Duration {
        let first_other = self.0.partition_point(|t| *t <= last);
        self.0
            .get(first_other)
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(*t))
    }
}

impl Merge {
    /// `streams`：对方在这条通道上开的流（[`crate::lanes::Inbox`] 里这条通道的信箱）
    pub fn new(lane: SessionLane, opener: Opener, streams: LaneStreams) -> Self {
        let capacity = (4 * 1024 * 1024 / lane.max_frame_size()).clamp(2, 16);
        let (tx, rx) = mpsc::channel(capacity);
        let core = match lane {
            SessionLane::Video(_) => Cores::Video(VideoCore::new(opener)),
            _ => Cores::Complete(Core::new(opener)),
        };
        Self {
            lane,
            core,
            tx,
            rx,
            streams,
            streams_open: true,
            relay: Relay::Waiting,
            frames: 0,
            partial: Arc::default(),
            ticks: Arc::default(),
            readers: JoinSet::new(),
        }
    }

    /// 画面收得怎么样（见模块说明的"报告"）
    pub fn stats(&self) -> VideoStats {
        match &self.core {
            Cores::Video(v) => v.stats.clone(),
            Cores::Complete(_) => VideoStats::default(),
        }
    }

    /// 往返时延（毫秒）：画面据此决定缺着的帧等多久（见模块说明）
    pub fn set_rtt(&mut self, rtt_ms: f64) {
        if let Cores::Video(v) = &mut self.core {
            v.rtt_ms = rtt_ms;
        }
    }

    /// 加一条已经拿到的流（握手时在控制通道上读过的那条）
    pub fn add(&mut self, recv: RecvStream, via: Via) -> Result<()> {
        self.add_stream(LaneStream { recv, via })
    }

    fn add_stream(&mut self, s: LaneStream) -> Result<()> {
        let direct = s.via == Via::Direct;
        if matches!(self.core, Cores::Video(_)) {
            self.frames += 1;
            self.read(
                s.recv,
                Source::Frame {
                    direct,
                    reader: READERS.fetch_add(1, Ordering::Relaxed),
                },
            );
            return Ok(());
        }
        if direct {
            self.read(s.recv, Source::Direct);
            return Ok(());
        }
        anyhow::ensure!(
            self.relay == Relay::Waiting,
            "the relay opened {:?} twice",
            self.lane
        );
        self.relay = Relay::Open;
        self.read(s.recv, Source::Relay);
        Ok(())
    }

    fn read(&mut self, mut recv: RecvStream, source: Source) {
        let tx = self.tx.clone();
        let max = self.lane.max_frame_size();
        // 帧流读到一半时记下最后一次读到数据的时刻：大的帧还在一点一点地到，不是丢了
        let progress = match source {
            Source::Frame { reader, .. } => {
                Some((reader, self.partial.clone(), self.ticks.clone()))
            }
            _ => None,
        };
        self.readers.spawn(async move {
            loop {
                let mut last = None;
                let mut gap = Duration::ZERO;
                let tick = |_| {
                    let Some((id, partial, ticks)) = &progress else {
                        return;
                    };
                    let now = Instant::now();
                    let mut ticks = ticks.lock().unwrap();
                    if let Some(l) = last {
                        gap = gap.max(ticks.blocked(l, now));
                    }
                    ticks.push(now);
                    last = Some(now);
                    // 直到合并任务收到这一帧才移除。否则刚读完的大帧在信箱里等待时会被误报丢失。
                    partial.lock().unwrap().insert(*id, now);
                };
                let packet = read_packet_watched(&mut recv, max, now_us, tick)
                    .await
                    .map_err(Into::into);
                let end = progress.is_some() || !matches!(packet, Ok(Some(_)));
                if tx
                    .send(Event {
                        source,
                        packet,
                        gap,
                    })
                    .await
                    .is_err()
                    || end
                {
                    return;
                }
            }
        });
    }

    fn received(
        &mut self,
        Event {
            source,
            packet,
            gap,
        }: Event,
    ) -> Result<()> {
        if let Source::Frame { reader, .. } = source {
            self.partial.lock().unwrap().remove(&reader);
            self.frames -= 1;
        }
        match packet {
            Ok(Some((data, arrived))) => match &mut self.core {
                Cores::Complete(c) => c.push(source.direct(), data, arrived)?,
                Cores::Video(v) => v.receive(source.direct(), data, arrived, gap)?,
            },
            end if source == Source::Relay => {
                end?;
                self.relay = Relay::Ended;
            }
            // 被放弃的帧流和断开的直连不会结束会话。
            Err(e) => debug!(lane = ?self.lane, ?source, "a session stream ended: {e:#}"),
            Ok(None) => {}
        }
        Ok(())
    }

    /// 下一帧（已经解密）。中转上的流结束了就是这条通道结束了（`None`）；
    /// 中转上的流出错、解不开都是错误，直连上的流断了不算。可以放在 `select!` 里（取消不丢帧）。
    /// 画面用 [`Merge::next_video`]（这里只交出解得了的帧）
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        loop {
            match self.next_video().await? {
                Some(Video::Frame(f)) => return Ok(Some(f)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// 画面的下一样东西（见 [`Video`]）。中转上的流、直连上的流、帧流都结束了，手里的也交完了，就是 `None`。
    /// 可以放在 `select!` 里（取消不丢帧）
    pub async fn next_video(&mut self) -> Result<Option<Video>> {
        loop {
            let now = Instant::now();
            if let Cores::Video(v) = &mut self.core {
                v.progress = self.partial.lock().unwrap().values().min().copied();
            }
            let out = match &mut self.core {
                Cores::Complete(c) => c.pop()?.map(Video::Frame),
                Cores::Video(v) => v.pop(now),
            };
            if let Some(out) = out {
                return Ok(Some(out));
            }
            let relay_done = match self.relay {
                Relay::Ended => true,
                // 会话结束了，对方再也开不过来了
                Relay::Waiting => !self.streams_open,
                Relay::Open => false,
            };
            let (ended, deadline) = match &self.core {
                // 中转上的是完整的一份：它结束了，手里剩下的也补不齐了
                Cores::Complete(_) => (relay_done, None),
                Cores::Video(v) => (!self.streams_open && self.frames == 0, v.deadline()),
            };
            if ended {
                return Ok(None);
            }
            tokio::select! {
                biased;
                // 已结束的读任务也要回收，否则每播放一帧就会留下一个任务句柄。
                Some(done) = self.readers.join_next(), if !self.readers.is_empty() => {
                    done.context("session stream reader failed")?;
                }
                ev = self.rx.recv() => self.received(ev.expect("we keep a sender"))?,
                s = self.streams.recv(), if self.streams_open => match s {
                    Some(s) => self.add_stream(s)?,
                    None => self.streams_open = false,
                },
                () = sleep_until(deadline) => {}
            }
        }
    }
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::{Sealer, test_lane};
    use airhand_proto::media::{self, HEADER_LEN};
    use airhand_proto::session::Codec;

    fn sealed(s: &mut Sealer, body: &[u8]) -> Vec<u8> {
        s.seal(body).unwrap()
    }

    /// 一帧画面。省略参考帧扩展的增量帧依赖前一帧。
    fn frame(s: &mut Sealer, seq: u32, key: bool, reference: Option<u32>) -> Vec<u8> {
        let h = VideoHeader {
            seq,
            stream: 0,
            key,
            codec: Codec::H264,
            width: 8,
            height: 8,
            capture_us: 0,
            reference,
        };
        let p = media::frame(&h.encode(), &[&[seq as u8]]);
        s.seal(&p[4..]).unwrap()
    }

    fn video(s: &mut Sealer, seq: u32, key: bool) -> Vec<u8> {
        frame(s, seq, key, None)
    }

    fn seq_of(f: &Frame) -> u32 {
        VideoHeader::decode_any(&f.data).unwrap().seq
    }

    /// 交出来的东西，简写成：解得了的帧 `F(序号)`、跳过的 `S(序号)`、等不到 `L(decoded)`、要关键帧 `K(after)`
    #[derive(Debug, PartialEq)]
    enum Got {
        F(u32),
        S(u32),
        L(u32),
        K(u32),
    }

    fn drain(m: &mut VideoCore, now: Instant) -> Vec<Got> {
        std::iter::from_fn(|| m.pop(now))
            .map(|v| match v {
                Video::Frame(f) => Got::F(seq_of(&f)),
                Video::Skipped { seq, .. } => Got::S(seq),
                Video::Lost { decoded } => Got::L(decoded),
                Video::Keyframe { after } => Got::K(after),
            })
            .collect()
    }

    /// 控制通道：两条路上是同样的帧，谁先到交谁，重复的丢掉；直连少了几帧，等中转补上
    #[test]
    fn complete_lanes_take_the_first_copy() {
        let (mut s, o) = test_lane(true);
        let frames: Vec<Vec<u8>> = (0..6u8).map(|i| sealed(&mut s, &[i])).collect();
        let mut m = Core::new(o);
        let mut got = Vec::new();
        let pop = |m: &mut Core, got: &mut Vec<(u8, bool)>| {
            while let Some(f) = m.pop().unwrap() {
                got.push((f.data[0], f.direct));
            }
        };
        // 直连快：0、1 从直连来，中转上的同样两帧是重复的
        m.push(true, frames[0].clone(), 0).unwrap();
        m.push(true, frames[1].clone(), 0).unwrap();
        pop(&mut m, &mut got);
        m.push(false, frames[0].clone(), 0).unwrap();
        m.push(false, frames[1].clone(), 0).unwrap();
        // 直连上少了 2（发送端来不及）：3、4 先放着，等中转上的 2
        m.push(true, frames[3].clone(), 0).unwrap();
        m.push(true, frames[4].clone(), 0).unwrap();
        pop(&mut m, &mut got);
        assert_eq!(got, [(0, true), (1, true)]);
        m.push(false, frames[2].clone(), 0).unwrap();
        pop(&mut m, &mut got);
        m.push(false, frames[3].clone(), 0).unwrap();
        m.push(false, frames[4].clone(), 0).unwrap();
        m.push(false, frames[5].clone(), 0).unwrap();
        pop(&mut m, &mut got);
        assert_eq!(
            got,
            [
                (0, true),
                (1, true),
                (2, false),
                (3, true),
                (4, true),
                (5, false)
            ]
        );
        assert!(m.held.is_empty());
    }

    /// 中转上的帧和只有中转时一样：不能跳过一帧（中转删不掉一个"松开按键"），重放的不交出去
    #[test]
    fn the_relay_copy_cannot_skip_frames() {
        let (mut s, o) = test_lane(true);
        let frames: Vec<Vec<u8>> = (0..3u8).map(|i| sealed(&mut s, &[i])).collect();
        let mut m = Core::new(o);
        m.push(false, frames[0].clone(), 0).unwrap();
        assert_eq!(m.pop().unwrap().unwrap().data, [0]);
        m.push(false, frames[0].clone(), 0).unwrap();
        assert!(m.pop().unwrap().is_none());
        assert!(m.push(false, frames[2].clone(), 0).is_err());
        // 篡改过的：解不开
        let mut bad = frames[1].clone();
        bad[0] ^= 1;
        m.push(false, bad, 0).unwrap();
        assert!(m.pop().is_err());
    }

    /// 直连上来早了的帧放不下就丢掉：中转上还有一份
    #[test]
    fn early_direct_frames_are_bounded() {
        let (mut s, o) = test_lane(true);
        let big = vec![0u8; MAX_HELD_BYTES / 2];
        let frames: Vec<Vec<u8>> = (0..4).map(|_| sealed(&mut s, &big)).collect();
        let mut m = Core::new(o);
        for f in &frames[1..] {
            m.push(true, f.clone(), 0).unwrap();
        }
        assert_eq!(m.held.len(), 1);
        for f in &frames {
            m.push(false, f.clone(), 0).unwrap();
            assert!(m.pop().unwrap().is_some());
        }
        assert!(m.held.is_empty() && m.held_bytes == 0);
    }

    /// 被控端从中转换到直连：直连上的帧先到，等中转上的前几帧；换回中转时也一样
    #[test]
    fn video_waits_for_the_old_path() {
        let (mut s, o) = test_lane(false);
        let f: Vec<Vec<u8>> = (0..6).map(|i| video(&mut s, i, i == 0)).collect();
        let mut m = VideoCore::new(o);
        let now = Instant::now();
        m.push(false, f[0].clone(), 0).unwrap();
        m.push(true, f[2].clone(), 0).unwrap();
        m.push(true, f[3].clone(), 0).unwrap();
        assert_eq!(drain(&mut m, now), [Got::F(0)]);
        m.push(false, f[1].clone(), 0).unwrap();
        let mut order = Vec::new();
        while let Some(Video::Frame(f)) = m.pop(now) {
            order.push((seq_of(&f), f.direct));
        }
        assert_eq!(order, [(1, false), (2, true), (3, true)]);
        // 重复的、太老的都不交出去
        m.push(false, f[2].clone(), 0).unwrap();
        assert!(m.pop(now).is_none());
        m.push(false, f[5].clone(), 0).unwrap();
        m.push(true, f[4].clone(), 0).unwrap();
        assert_eq!(drain(&mut m, now), [Got::F(4), Got::F(5)]);
    }

    /// 换路后可靠关键帧可以越过断在旧路上的帧。
    #[test]
    fn video_does_not_wait_for_frames_lost_on_a_dead_path() {
        let (mut s, o) = test_lane(false);
        let f: Vec<Vec<u8>> = (0..10)
            .map(|i| video(&mut s, i, i == 0 || i == 4 || i == 9))
            .collect();
        let mut m = VideoCore::new(o);
        let t0 = Instant::now();
        m.push(true, f[0].clone(), 0).unwrap();
        m.push(true, f[1].clone(), 0).unwrap();
        // 2、3 断在了直连上；被控端换回中转，接着发的 4 是关键帧
        m.push(false, f[4].clone(), 0).unwrap();
        m.push(false, f[5].clone(), 0).unwrap();
        assert_eq!(
            drain(&mut m, t0),
            [Got::F(0), Got::F(1), Got::F(4), Got::F(5)]
        );
        // 断在直连上的帧后来又到了：太晚了
        m.push(true, f[3].clone(), 0).unwrap();
        assert!(m.pop(t0).is_none());
    }

    /// 帧流：后面的小帧先到了、前面的大帧（关键帧）还在一点一点地到：不是丢了，一直有数据就一直不报告；
    /// 停住了 loss_wait 才算（都在 VIDEO_WAIT 之内）
    #[test]
    fn a_large_frame_that_is_still_arriving_is_not_lost() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        m.rtt_ms = 60.0;
        let t0 = Instant::now();
        let _big_key = frame(&mut s, 1, true, None);
        let d2 = frame(&mut s, 2, false, Some(1));
        m.push(false, d2, 0).unwrap();
        assert!(m.pop(t0).is_none());
        let wait = loss_wait(60.0);
        // 关键帧的流上一直有数据：等多久都不报告
        for i in 1..=3 {
            let now = t0 + wait * i;
            m.progress = Some(now - wait / 2);
            assert!(m.pop(now).is_none(), "{i}");
        }
        // 停住了 loss_wait：报告
        let stalled = t0 + wait * 3 + wait / 2;
        assert_eq!(m.deadline(), Some(stalled));
        assert_eq!(drain(&mut m, stalled), [Got::L(0)]);
        // 报告之后又有数据了（重传到了）：不再重报，直到又停住
        let resumed = stalled + loss_repeat(60.0);
        m.progress = Some(resumed);
        assert!(m.pop(resumed).is_none());
        assert_eq!(m.deadline(), Some(resumed + loss_repeat(60.0)));
    }

    /// 到达时刻（见模块说明的"到达时刻"）：一帧等前面的帧（重传）的时候不算开始到达，前一帧交出去的时候才算，
    /// 和以前一条有序流时一样；不用等的帧就是它自己开始到达的时刻
    #[test]
    fn frames_waiting_for_earlier_ones_arrive_late() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        let t0 = Instant::now();
        let base = m.clock_us(t0);
        let ms = Duration::from_millis;
        let f: Vec<Vec<u8>> = (1..=4)
            .map(|seq| frame(&mut s, seq, seq == 1, (seq > 1).then(|| seq - 1)))
            .collect();
        let arrived = |v: Option<Video>| match v {
            Some(Video::Frame(f)) => f.arrived,
            other => panic!("{other:?}"),
        };
        m.push(false, f[0].clone(), base + 1_000).unwrap();
        assert_eq!(arrived(m.pop(t0 + ms(2))), base + 1_000);
        // 第 3 帧先到了，第 2 帧在等重传
        m.push(false, f[2].clone(), base + 20_000).unwrap();
        assert!(m.pop(t0 + ms(21)).is_none());
        // 第 2 帧的重传到了：它自己 15 ms 开始到达（第 1 帧早就交出去了）；第 3 帧等到第 2 帧交出去（90 ms）才算到达
        m.push(false, f[1].clone(), base + 15_000).unwrap();
        assert_eq!(arrived(m.pop(t0 + ms(90))), base + 15_000);
        assert_eq!(arrived(m.pop(t0 + ms(90))), base + 90_000);
        // 不用等的帧：就是它自己的
        m.push(false, f[3].clone(), base + 100_000).unwrap();
        assert_eq!(arrived(m.pop(t0 + ms(101))), base + 100_000);
    }

    /// 帧流：后面的帧先到了、前面的一帧在重传，带参考帧的增量帧等它；重传补上了就照常交出去，不报告
    #[test]
    fn frames_wait_for_their_reference_to_be_retransmitted() {
        let (mut s, o) = test_lane(false);
        let f: Vec<Vec<u8>> = (0..5)
            .map(|i| frame(&mut s, i, i == 0, (i > 0).then(|| i - 1)))
            .collect();
        let mut m = VideoCore::new(o);
        m.rtt_ms = 80.0;
        let t0 = Instant::now();
        m.push(false, f[0].clone(), 0).unwrap();
        m.push(false, f[1].clone(), 0).unwrap();
        m.push(false, f[3].clone(), 0).unwrap();
        m.push(false, f[4].clone(), 0).unwrap();
        assert_eq!(drain(&mut m, t0), [Got::F(0), Got::F(1)]);
        assert_eq!(m.deadline(), Some(t0 + loss_wait(80.0)));
        m.push(false, f[2].clone(), 0).unwrap();
        assert_eq!(
            drain(&mut m, t0 + Duration::from_millis(60)),
            [Got::F(2), Got::F(3), Got::F(4)]
        );
        assert_eq!(m.deadline(), None);
    }

    /// 帧流：一帧一直等不到，报告等不到（带着交给解码器的最后一帧）、定时再报；被控端从交给过解码器的帧恢复，
    /// 恢复的那一帧一到就交出去，中间解不了的跳过；放弃了的帧后来又到了也不要了
    #[test]
    fn a_lost_frame_is_reported_and_recovered_from() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        m.rtt_ms = 100.0;
        let t0 = Instant::now();
        let k = frame(&mut s, 1, true, None);
        let d2 = frame(&mut s, 2, false, Some(1));
        let lost = frame(&mut s, 3, false, Some(2));
        let d4 = frame(&mut s, 4, false, Some(3));
        let d5 = frame(&mut s, 5, false, Some(4));
        for f in [&k, &d2, &d4, &d5] {
            m.push(false, f.clone(), 0).unwrap();
        }
        assert_eq!(drain(&mut m, t0), [Got::F(1), Got::F(2)]);
        // 等一个往返：报告，之后两个往返再报一次
        let wait = loss_wait(100.0);
        assert!(m.pop(t0 + wait / 2).is_none());
        assert_eq!(drain(&mut m, t0 + wait), [Got::L(2)]);
        assert_eq!(m.deadline(), Some(t0 + wait + loss_repeat(100.0)));
        assert!(m.pop(t0 + wait).is_none());
        let again = t0 + wait + loss_repeat(100.0);
        assert_eq!(drain(&mut m, again), [Got::L(2)]);
        // 被控端放弃了 3、4、5，从第 1 帧（长期参考帧）恢复：第 6 帧参考 1
        let r6 = frame(&mut s, 6, false, Some(1));
        let d7 = frame(&mut s, 7, false, Some(6));
        m.push(false, r6, 0).unwrap();
        m.push(false, d7, 0).unwrap();
        assert_eq!(
            drain(&mut m, again),
            [Got::S(4), Got::S(5), Got::F(6), Got::F(7)]
        );
        // 放弃了的帧后来又到了：不要了
        m.push(false, lost, 0).unwrap();
        assert!(m.pop(again).is_none() && m.held.is_empty());
        assert_eq!(m.deadline(), None);
    }

    /// 帧流：报告了等不到，重传却先到了：照常交出去，恢复的那一帧到了也解得了（它参考的是交给过解码器的帧）
    #[test]
    fn a_late_retransmission_is_still_used() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        let t0 = Instant::now();
        let k = frame(&mut s, 1, true, None);
        let slow = frame(&mut s, 2, false, Some(1));
        let d3 = frame(&mut s, 3, false, Some(2));
        m.push(false, k, 0).unwrap();
        m.push(false, d3, 0).unwrap();
        assert_eq!(drain(&mut m, t0), [Got::F(1)]);
        let late = t0 + loss_wait(DEFAULT_RTT_MS);
        assert_eq!(drain(&mut m, late), [Got::L(1)]);
        m.push(false, slow, 0).unwrap();
        assert_eq!(drain(&mut m, late), [Got::F(2), Got::F(3)]);
        let r = frame(&mut s, 4, false, Some(1));
        m.push(false, r, 0).unwrap();
        assert_eq!(drain(&mut m, late), [Got::F(4)]);
    }

    /// 帧流：一直恢复不了（被控端没收到报告、恢复的那一帧也丢了），等了一秒多照样要关键帧（走控制通道），
    /// 但不放弃手里的帧
    #[test]
    fn frames_fall_back_to_a_keyframe() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        let t0 = Instant::now();
        let k = frame(&mut s, 1, true, None);
        let _lost = frame(&mut s, 2, false, Some(1));
        let d3 = frame(&mut s, 3, false, Some(2));
        m.push(false, k, 0).unwrap();
        m.push(false, d3, 0).unwrap();
        assert_eq!(drain(&mut m, t0), [Got::F(1)]);
        let got = drain(&mut m, t0 + VIDEO_WAIT);
        assert_eq!(got, [Got::L(1), Got::K(1)]);
        assert_eq!(m.held.len(), 1);
        let key = frame(&mut s, 4, true, None);
        m.push(false, key, 0).unwrap();
        assert_eq!(drain(&mut m, t0 + VIDEO_WAIT), [Got::S(3), Got::F(4)]);
    }

    /// 参考帧不在交给过解码器的帧里（太旧了、被控端弄错了）：解不了，不交出去
    #[test]
    fn a_reference_that_was_never_decoded_is_not_decodable() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        let t0 = Instant::now();
        m.push(false, frame(&mut s, 1, true, None), 0).unwrap();
        m.push(false, frame(&mut s, 2, false, Some(77)), 0).unwrap();
        assert_eq!(drain(&mut m, t0), [Got::F(1), Got::S(2)]);
    }

    /// 写了一半就 reset 的帧可能丢失。信箱关闭且所有读任务结束后，剩余的缺口也结束等待。
    #[tokio::test]
    async fn frame_streams_come_and_go() {
        let (client, server) = crate::tests::quic_pair().await;
        let (mut s, o) = test_lane(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut m = Merge::new(SessionLane::Video(0), o, rx);
        let accept = async || server.accept_uni().await.unwrap();
        for seq in 0..3u32 {
            let mut send = client.open_uni().await.unwrap();
            let f = frame(&mut s, seq, seq == 0, seq.checked_sub(1));
            let mut wire = (f.len() as u32).to_le_bytes().to_vec();
            wire.extend_from_slice(&f);
            send.write_all(&wire).await.unwrap();
            if seq == 1 {
                // 被控端放弃了这一帧：写到一半 reset（另一端读到一半就断了）
                send.reset(0u32.into()).unwrap();
            } else {
                send.finish().unwrap();
            }
            tx.send(LaneStream {
                recv: accept().await,
                via: Via::Relay,
            })
            .unwrap();
        }
        let first = tokio::time::timeout(Duration::from_secs(2), m.next_video())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, Some(Video::Frame(f)) if seq_of(&f) == 0));
        drop(tx);
        // 第 1 帧可能整个到了（reset 之前就送到了），也可能断了；第 2 帧参考它
        let mut rest = Vec::new();
        while let Some(v) = tokio::time::timeout(Duration::from_secs(3), m.next_video())
            .await
            .unwrap()
            .unwrap()
        {
            rest.push(match v {
                Video::Frame(f) => Got::F(seq_of(&f)),
                Video::Skipped { seq, .. } => Got::S(seq),
                Video::Lost { decoded } => Got::L(decoded),
                Video::Keyframe { after } => Got::K(after),
            });
            assert!(rest.len() <= 2, "unexpected frame events: {rest:?}");
        }
        assert!(
            rest.is_empty() || rest == [Got::F(1), Got::F(2)],
            "{rest:?}"
        );
    }

    #[tokio::test]
    async fn a_video_stream_delivers_only_its_first_frame() {
        let (client, server) = crate::tests::quic_pair().await;
        let (mut sealer, opener) = test_lane(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut video = Merge::new(SessionLane::Video(0), opener, rx);
        let mut send = client.open_uni().await.unwrap();
        for seq in [1, 2] {
            let data = frame(&mut sealer, seq, true, None);
            send.write_all(&(data.len() as u32).to_le_bytes())
                .await
                .unwrap();
            send.write_all(&data).await.unwrap();
        }
        send.finish().unwrap();
        tx.send(LaneStream {
            recv: server.accept_uni().await.unwrap(),
            via: Via::Relay,
        })
        .unwrap();
        drop(tx);
        assert!(
            matches!(video.next_video().await.unwrap(), Some(Video::Frame(f)) if seq_of(&f) == 1)
        );
        assert!(video.next_video().await.unwrap().is_none());
    }

    /// 真的 QUIC 帧流：后面的小帧先到了、前面的大帧还在一点一点地到（比一个往返还久）：不算等不到；
    /// 大帧写到一半停住了（后面的帧照样在来）：报告等不到
    #[tokio::test]
    async fn a_stalled_frame_stream_is_reported_but_a_slow_one_is_not() {
        let (client, server) = crate::tests::quic_pair().await;
        let (mut s, o) = test_lane(false);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut m = Merge::new(SessionLane::Video(0), o, rx);
        m.set_rtt(100.0);
        let wire = |f: Vec<u8>| [&(f.len() as u32).to_le_bytes()[..], &f].concat();
        let accept = async |tx: &mpsc::UnboundedSender<LaneStream>| {
            let recv = server.accept_uni().await.unwrap();
            tx.send(LaneStream {
                recv,
                via: Via::Relay,
            })
            .unwrap();
        };
        // 大帧：先写一小段，剩下的调用的一方写
        let big = async |s: &mut Sealer, seq: u32, key: bool, reference: Option<u32>| {
            let h = VideoHeader {
                seq,
                stream: 0,
                key,
                codec: Codec::H264,
                width: 8,
                height: 8,
                capture_us: 0,
                reference,
            };
            let p = media::frame(&h.encode(), &[&vec![seq as u8; 100_000]]);
            let bytes = wire(s.seal(&p[4..]).unwrap());
            let mut send = client.open_uni().await.unwrap();
            send.write_all(&bytes[..10_000]).await.unwrap();
            (send, bytes)
        };
        let small = async |s: &mut Sealer, seq: u32, reference: u32| {
            let mut send = client.open_uni().await.unwrap();
            send.write_all(&wire(frame(s, seq, false, Some(reference))))
                .await
                .unwrap();
            send.finish().unwrap();
        };
        let next = async |m: &mut Merge| match tokio::time::timeout(
            Duration::from_secs(3),
            m.next_video(),
        )
        .await
        .expect("nothing came")
        .unwrap()
        .unwrap()
        {
            Video::Frame(f) => Got::F(seq_of(&f)),
            Video::Skipped { seq, .. } => Got::S(seq),
            Video::Lost { decoded } => Got::L(decoded),
            Video::Keyframe { after } => Got::K(after),
        };

        // 关键帧很大、慢慢地到：每 20 ms 一段，一共比一个往返（100 ms）久得多
        let (mut key, bytes) = big(&mut s, 1, true, None).await;
        accept(&tx).await;
        small(&mut s, 2, 1).await;
        accept(&tx).await;
        let slowly = tokio::spawn(async move {
            for piece in bytes[10_000..].chunks(10_000) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                key.write_all(piece).await.unwrap();
            }
            key.finish().unwrap();
            key
        });
        assert_eq!(next(&mut m).await, Got::F(1));
        assert_eq!(next(&mut m).await, Got::F(2));
        let _key = slowly.await.unwrap();

        // 大帧写到一半就停住了，后面的小帧一直在来：报告等不到（交给解码器的最后一帧是第 2 帧）
        let (_stuck, _) = big(&mut s, 3, false, Some(2)).await;
        accept(&tx).await;
        let started = Instant::now();
        let mut reference = 3;
        let got = loop {
            tokio::select! {
                got = next(&mut m) => break got,
                () = tokio::time::sleep(Duration::from_millis(16)) => {
                    reference += 1;
                    small(&mut s, reference, reference - 1).await;
                    accept(&tx).await;
                }
            }
        };
        assert_eq!(got, Got::L(2));
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "{:?}",
            started.elapsed()
        );
    }

    /// 报告：帧流上读到一半停过、后面的帧又先到了的算卡住过；停得短的、没被赶超的不算；
    /// 到的时候已经不等它了的也算
    #[test]
    fn stalls_are_counted_for_the_report() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        let t0 = Instant::now();
        let f: Vec<Vec<u8>> = (0..7)
            .map(|i| frame(&mut s, i, i == 0, (i > 0).then(|| i - 1)))
            .collect();
        let stalled = STALL * 2;
        let short = STALL / 2;
        m.receive(false, f[0].clone(), 0, stalled).unwrap();
        m.receive(false, f[2].clone(), 0, short).unwrap();
        // 停了很久、第 2 帧先到了
        m.receive(false, f[1].clone(), 0, stalled).unwrap();
        // 停得短
        m.receive(false, f[4].clone(), 0, short).unwrap();
        m.receive(false, f[3].clone(), 0, short).unwrap();
        assert_eq!(drain(&mut m, t0).len(), 5);
        m.receive(false, f[6].clone(), 0, Duration::ZERO).unwrap();
        assert!(drain(&mut m, t0).is_empty());
        let late = t0 + loss_wait(DEFAULT_RTT_MS);
        assert_eq!(drain(&mut m, late), [Got::L(4)]);
        let r = frame(&mut s, 7, true, None);
        m.push(false, r, 0).unwrap();
        assert_eq!(drain(&mut m, late), [Got::S(6), Got::F(7)]);
        m.receive(false, f[5].clone(), 0, short).unwrap();
        let r = m.stats.take();
        assert_eq!((r.frames, r.stalled), (8, 2));
    }

    #[test]
    fn delivered_frames_carry_their_payload() {
        let (mut s, o) = test_lane(false);
        let mut m = VideoCore::new(o);
        m.push(false, video(&mut s, 0, true), 7).unwrap();
        let Some(Video::Frame(f)) = m.pop(Instant::now()) else {
            panic!()
        };
        assert_eq!((f.data.len(), f.arrived), (HEADER_LEN + 1, 7));
    }
}
