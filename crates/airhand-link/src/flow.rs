//! Shared video flow control and adaptive bitrate. Sequence numbers belong to a
//! session, not an encoder/display; rebuilding a pipeline must retain its Flow.
//!
//! 在途帧窗口 ≈ 基础 RTT × 帧率 + 2（即带宽时延积换算成帧数），最多 [`MAX_WINDOW`] 帧：
//! RTT 300 ms、60 fps 时是 20 帧，窗口太小的话长距离链路上帧率上不去。
//! 基础 RTT 是最近 [`MIN_RTT_WINDOW`] 里的最小样本（单调队列实现的滑动窗口最小值）。
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// 在途帧窗口的上限
pub const MAX_WINDOW: u32 = 32;
/// 基础 RTT 取这么长时间里的最小样本
pub const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct FlowState {
    sent: u32,
    acked: u32,
    pending: VecDeque<(u32, Instant)>,
    srtt_ms: f64,
    /// 滑动窗口最小值：样本按时间排列、RTT 严格递增，队头就是窗口里的最小值
    min_rtt: VecDeque<(f64, Instant)>,
}

impl Default for FlowState {
    fn default() -> Self {
        Self {
            sent: 0,
            acked: 0,
            pending: VecDeque::new(),
            srtt_ms: 0.0,
            min_rtt: VecDeque::new(),
        }
    }
}

impl FlowState {
    pub fn on_sent(&mut self, seq: u32) {
        // Android shares a sequence across subscribers, so skipped numbers are
        // not in-flight frames. Never use sent - acked as the queue length.
        if self.pending.is_empty() {
            self.acked = seq.wrapping_sub(1);
        }
        self.sent = seq;
        self.pending.push_back((seq, Instant::now()));
    }

    pub fn on_ack(&mut self, seq: u32) {
        if seq.wrapping_sub(self.acked) as i32 <= 0 || seq.wrapping_sub(self.sent) as i32 > 0 {
            return;
        }
        self.acked = seq;
        let now = Instant::now();
        while let Some(&(s, t)) = self.pending.front() {
            if s.wrapping_sub(seq) as i32 > 0 {
                break;
            }
            self.pending.pop_front();
            if s == seq {
                let rtt = now.duration_since(t).as_secs_f64() * 1000.0;
                self.srtt_ms = if self.srtt_ms == 0.0 {
                    rtt
                } else {
                    self.srtt_ms * 0.875 + rtt * 0.125
                };
                self.sample_min_rtt(rtt, now);
            }
        }
    }

    /// 记一个 RTT 样本。窗口里只有一个样本时即使过了期也留着（画面静止时不发帧，没有新样本）
    fn sample_min_rtt(&mut self, rtt: f64, now: Instant) {
        while self.min_rtt.back().is_some_and(|&(r, _)| r >= rtt) {
            self.min_rtt.pop_back();
        }
        self.min_rtt.push_back((rtt, now));
        while self.min_rtt.len() > 1
            && self
                .min_rtt
                .front()
                .is_some_and(|&(_, t)| now.duration_since(t) > MIN_RTT_WINDOW)
        {
            self.min_rtt.pop_front();
        }
    }

    fn min_rtt_ms(&self) -> Option<f64> {
        self.min_rtt.front().map(|&(r, _)| r)
    }

    pub fn inflight(&self) -> u32 {
        self.pending.len() as u32
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
    pub fn sent(&self) -> u32 {
        let mut state = self.state.lock().unwrap();
        let seq = state.sent.wrapping_add(1);
        state.on_sent(seq);
        seq
    }
    /// A frame assigned a sequence but cancelled before enqueueing needs no ACK.
    pub fn discard(&self, seq: u32) {
        self.state
            .lock()
            .unwrap()
            .pending
            .retain(|(s, _)| *s != seq);
        self.cv.notify_all();
    }
    pub fn ack(&self, seq: u32) {
        self.state.lock().unwrap().on_ack(seq);
        self.cv.notify_all();
    }
    pub fn wake(&self) {
        self.cv.notify_all();
    }
    pub fn wait_window(&self, fps: u8, stop: &AtomicBool, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .cv
            .wait_timeout_while(state, timeout, |s| {
                s.inflight() >= s.window(fps) && !stop.load(Ordering::Acquire)
            })
            .unwrap();
        !stop.load(Ordering::Acquire) && state.inflight() < state.window(fps)
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
        f.on_ack(100);
        assert_eq!(f.inflight(), 0);
        f.on_sent(1);
        f.on_ack(100);
        assert_eq!(f.inflight(), 1);
        f.on_ack(1);
        f.on_sent(1000);
        assert_eq!(f.inflight(), 1);
        f.on_ack(1);
        assert_eq!(f.inflight(), 1);
        f.on_ack(1000);
        assert_eq!(f.inflight(), 0);
    }
    #[test]
    fn sequence_survives_pipeline_replacement_and_wraps() {
        let f = Flow::default();
        assert_eq!(f.sent(), 1);
        // A replacement pipeline uses the same session flow, including late ACKs.
        assert_eq!(f.sent(), 2);
        f.ack(1);
        assert!(f.wait_window(60, &AtomicBool::new(false), Duration::ZERO));
        let mut state = FlowState::default();
        state.on_sent(u32::MAX);
        state.on_sent(0);
        state.on_ack(0);
        assert_eq!(state.inflight(), 0);
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
        f.sample_min_rtt(50.0, at(0));
        f.sample_min_rtt(120.0, at(1));
        f.sample_min_rtt(80.0, at(2));
        assert_eq!(f.min_rtt_ms(), Some(50.0));
        // 50 过了窗口：最小值是窗口里剩下的 80，而不是刚来的那个样本
        f.sample_min_rtt(200.0, at(11));
        assert_eq!(f.min_rtt_ms(), Some(80.0));
        f.sample_min_rtt(300.0, at(13));
        assert_eq!(f.min_rtt_ms(), Some(200.0));
        // 很久没有样本（画面静止）也保留最后的值，下一个样本来了再替换
        f.sample_min_rtt(250.0, at(100));
        assert_eq!(f.min_rtt_ms(), Some(250.0));
    }

    #[test]
    fn window_covers_long_round_trips() {
        let mut f = FlowState::default();
        f.sample_min_rtt(300.0, Instant::now());
        // 300 ms × 60 fps = 18 帧在路上，再加 2
        assert_eq!(f.window(60), 20);
        assert_eq!(f.window(120), MAX_WINDOW);
        let mut near = FlowState::default();
        near.sample_min_rtt(5.0, Instant::now());
        assert_eq!(near.window(30), 3);
    }
}
