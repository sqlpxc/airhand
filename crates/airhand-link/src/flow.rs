//! 被控端的在途帧控制和自适应码率（桌面版和 Android 共用）。帧序号属于会话，不属于某个编码器或显示器：
//! 重建编码管线时接着用同一个 [`Flow`]。
//!
//! 在途帧窗口 ≈ 基础 RTT × 帧率 + 2（即带宽时延积换算成帧数），最多 [`MAX_WINDOW`] 帧：
//! RTT 300 ms、60 fps 时是 20 帧，窗口太小的话长距离链路上帧率上不去。
//! 基础 RTT 是最近 [`MIN_RTT_WINDOW`] 里的最小样本（单调队列实现的滑动窗口最小值）。
//!
//! 确认里还带着这一帧到达控制端的时刻（控制端的时钟）：减去本端的发送时刻，就是单向时延加上两边时钟之差，
//! 减去它最近的最小值，就是这一帧在网络上多排了多久的队（[`FlowState::queue_delay`]）。
//! 和往返时延不同，它不含界面那一段（IPC、解码前的排队），也不含确认回来的路。
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// 在途帧窗口的上限
pub const MAX_WINDOW: u32 = 32;
/// 单会话已发送、未确认的媒体负载预算；关键帧很大时也不能仅靠帧数限量。
pub const MAX_INFLIGHT_BYTES: usize = 32 * 1024 * 1024;
/// 控制心跳正常也不能永久占着未确认的视频；超过期限由会话关闭并重连。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(20);
/// 基础 RTT 取这么长时间里的最小样本
pub const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);

/// 最近 [`MIN_RTT_WINDOW`] 里的最小值：样本按时间排列、值严格递增，队头就是窗口里的最小值。
/// 窗口里只有一个样本时即使过了期也留着（画面静止时不发帧，没有新样本）
#[derive(Debug, Default)]
struct WindowMin(VecDeque<(f64, Instant)>);

impl WindowMin {
    fn push(&mut self, v: f64, now: Instant) {
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

    fn get(&self) -> Option<f64> {
        self.0.front().map(|&(v, _)| v)
    }
}

#[derive(Debug)]
pub struct FlowState {
    sent: u32,
    acked: u32,
    pending: VecDeque<(u32, Instant, usize)>,
    pending_bytes: usize,
    srtt_ms: f64,
    min_rtt: WindowMin,
    /// 把发送时刻换成微秒的起点（和控制端的到达时刻相减）
    epoch: Instant,
    /// 单向时延（到达时刻 − 发送时刻，含两边时钟之差，毫秒）的最小值
    min_owd: WindowMin,
    /// 最近一帧比最快的时候在网络上多排了多久（毫秒）
    queue_ms: Option<f64>,
}

impl Default for FlowState {
    fn default() -> Self {
        Self {
            sent: 0,
            acked: 0,
            pending: VecDeque::new(),
            pending_bytes: 0,
            srtt_ms: 0.0,
            min_rtt: WindowMin::default(),
            epoch: Instant::now(),
            min_owd: WindowMin::default(),
            queue_ms: None,
        }
    }
}

impl FlowState {
    pub fn on_sent(&mut self, seq: u32, bytes: usize) {
        // Android shares a sequence across subscribers, so skipped numbers are
        // not in-flight frames. Never use sent - acked as the queue length.
        if self.pending.is_empty() {
            self.acked = seq.wrapping_sub(1);
        }
        self.sent = seq;
        self.pending.push_back((seq, Instant::now(), bytes));
        self.pending_bytes += bytes;
    }

    /// 控制端确认到了第 `seq` 帧（累积）。`arrived`：这一帧到达控制端的时刻（控制端的时钟，微秒）
    pub fn on_ack(&mut self, seq: u32, arrived: Option<u64>) {
        if seq.wrapping_sub(self.acked) as i32 <= 0 || seq.wrapping_sub(self.sent) as i32 > 0 {
            return;
        }
        self.acked = seq;
        let now = Instant::now();
        while let Some(&(s, t, bytes)) = self.pending.front() {
            if s.wrapping_sub(seq) as i32 > 0 {
                break;
            }
            self.pending.pop_front();
            self.pending_bytes -= bytes;
            if s != seq {
                continue;
            }
            let rtt = now.duration_since(t).as_secs_f64() * 1000.0;
            self.srtt_ms = if self.srtt_ms == 0.0 {
                rtt
            } else {
                self.srtt_ms * 0.875 + rtt * 0.125
            };
            self.min_rtt.push(rtt, now);
            if let Some(arrived) = arrived {
                let sent = t.duration_since(self.epoch).as_secs_f64() * 1000.0;
                let owd = arrived as f64 / 1000.0 - sent;
                self.min_owd.push(owd, now);
                self.queue_ms = self.min_owd.get().map(|min| owd - min);
            }
        }
    }

    fn min_rtt_ms(&self) -> Option<f64> {
        self.min_rtt.get()
    }

    /// 最近确认的一帧在网络上比最快的时候多排了多久（毫秒，见模块说明）；控制端没报到达时刻时为 `None`
    pub fn queue_delay(&self) -> Option<f64> {
        self.queue_ms
    }

    pub fn inflight(&self) -> u32 {
        self.pending.len() as u32
    }
    pub fn ack_stalled(&self) -> bool {
        self.pending
            .front()
            .is_some_and(|(_, at, _)| at.elapsed() > ACK_TIMEOUT)
    }

    pub fn can_send(&self, fps: u8, bytes: usize) -> bool {
        self.inflight() < self.window(fps)
            && bytes <= MAX_INFLIGHT_BYTES.saturating_sub(self.pending_bytes)
    }

    pub fn window(&self, fps: u8) -> u32 {
        let Some(min_rtt) = self.min_rtt_ms() else {
            return 3;
        };
        ((min_rtt / 1000.0 * fps as f64).ceil() as u32 + 2).clamp(2, MAX_WINDOW)
    }
    pub fn rtt(&self) -> (f64, f64) {
        (self.srtt_ms, self.min_rtt_ms().unwrap_or(0.0))
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
    /// A frame assigned a sequence but cancelled before enqueueing needs no ACK.
    pub fn discard(&self, seq: u32) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.pending.iter().position(|(s, _, _)| *s == seq) {
            let (_, _, bytes) = state.pending.remove(index).unwrap();
            state.pending_bytes -= bytes;
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
    pub fn wait_window(&self, fps: u8, stop: &AtomicBool, timeout: Duration) -> bool {
        self.wait_room(fps, 1, stop, timeout)
    }
    /// 编码后知道真实大小，再等字节预算；等待期间必须保留这一帧，否则后续增量帧失去参考。
    pub fn wait_room(&self, fps: u8, bytes: usize, stop: &AtomicBool, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .cv
            .wait_timeout_while(state, timeout, |s| {
                !s.can_send(fps, bytes) && !stop.load(Ordering::Acquire)
            })
            .unwrap();
        !stop.load(Ordering::Acquire) && state.can_send(fps, bytes)
    }
    pub fn ack_stalled(&self) -> bool {
        self.state.lock().unwrap().ack_stalled()
    }
    pub fn rtt(&self) -> (f64, f64) {
        self.state.lock().unwrap().rtt()
    }
}

pub struct Abr {
    target: u32,
    pub current: u32,
    pub blocked: u32,
    window_start: Instant,
    calm_since: Instant,
}
impl Abr {
    pub fn new(target: u32) -> Self {
        Self {
            target,
            current: target,
            blocked: 0,
            window_start: Instant::now(),
            calm_since: Instant::now(),
        }
    }
    pub fn reset(&mut self, target: u32) {
        *self = Self::new(target);
    }
    pub fn tick(&mut self, srtt: f64, min_rtt: f64) {
        let now = Instant::now();
        if now.duration_since(self.window_start) < Duration::from_secs(1) {
            return;
        }
        if self.blocked >= 3 || (min_rtt > 0.0 && srtt > min_rtt * 2.0 + 60.0) {
            self.current = ((self.current as f64 * 0.75) as u32)
                .max((self.target / 6).max(300).min(self.target));
            self.calm_since = now;
        } else if now.duration_since(self.calm_since) > Duration::from_secs(3)
            && self.current < self.target
        {
            self.current = ((self.current as f64 * 1.12) as u32 + 100).min(self.target);
            self.calm_since = now - Duration::from_secs(2);
        }
        self.blocked = 0;
        self.window_start = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn missing_video_ack_expires_even_when_control_still_works() {
        let mut flow = FlowState::default();
        flow.on_sent(1, 100);
        flow.pending.front_mut().unwrap().1 = Instant::now() - ACK_TIMEOUT - Duration::from_secs(1);
        assert!(flow.ack_stalled());
        flow.on_ack(1, None);
        assert!(!flow.ack_stalled());
    }

    #[test]
    fn byte_budget_is_released_by_ack_and_cancelled_frames() {
        let f = Flow::default();
        let stop = AtomicBool::new(false);
        let seq = f.sent(MAX_INFLIGHT_BYTES - 100);
        assert!(f.wait_room(60, 100, &stop, Duration::ZERO));
        assert!(!f.wait_room(60, 101, &stop, Duration::ZERO));
        f.ack(seq, None);
        assert!(f.wait_room(60, MAX_INFLIGHT_BYTES, &stop, Duration::ZERO));
        let seq = f.sent(MAX_INFLIGHT_BYTES);
        assert!(!f.wait_window(60, &stop, Duration::ZERO));
        f.discard(seq);
        assert!(f.wait_room(60, MAX_INFLIGHT_BYTES, &stop, Duration::ZERO));
        assert!(!f.wait_room(60, MAX_INFLIGHT_BYTES + 1, &stop, Duration::ZERO));
    }

    #[test]
    fn stopped_pipeline_cannot_emit_another_frame() {
        assert!(!Flow::default().wait_window(30, &AtomicBool::new(true), Duration::ZERO));
    }

    #[test]
    fn min_rtt_is_a_sliding_window_minimum() {
        let mut f = FlowState::default();
        assert_eq!(f.window(60), 3);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        f.min_rtt.push(50.0, at(0));
        f.min_rtt.push(120.0, at(1));
        f.min_rtt.push(80.0, at(2));
        assert_eq!(f.min_rtt_ms(), Some(50.0));
        // 50 过了窗口：最小值是窗口里剩下的 80，而不是刚来的那个样本
        f.min_rtt.push(200.0, at(11));
        assert_eq!(f.min_rtt_ms(), Some(80.0));
        f.min_rtt.push(300.0, at(13));
        assert_eq!(f.min_rtt_ms(), Some(200.0));
        // 很久没有样本（画面静止）也保留最后的值，下一个样本来了再替换
        f.min_rtt.push(250.0, at(100));
        assert_eq!(f.min_rtt_ms(), Some(250.0));
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
            f.pending.back_mut().unwrap().1 = f.epoch + Duration::from_millis(at_ms);
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

    #[test]
    fn window_covers_long_round_trips() {
        let mut f = FlowState::default();
        f.min_rtt.push(300.0, Instant::now());
        // 300 ms × 60 fps = 18 帧在路上，再加 2
        assert_eq!(f.window(60), 20);
        assert_eq!(f.window(120), MAX_WINDOW);
        let mut near = FlowState::default();
        near.min_rtt.push(5.0, Instant::now());
        assert_eq!(near.window(30), 3);
    }
}
