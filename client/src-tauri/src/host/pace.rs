//! 帧率节流（采集线程按订阅者里最高的帧率，编码线程按自己会话的帧率）：按**画面的时间戳**排一条时间线，
//! 每处理一帧往后推一个周期；可以提前 1/4 个周期，按时到的画面不用等。
//!
//! 以前是"处理完这一帧，过 1/帧率 再处理下一帧"。采集本来就按这个帧率出画面时（60 Hz 的屏配 60 fps；
//! macOS 的 ScreenCaptureKit 本来就按设定的帧率出），每帧多出一点处理和睡过头的时间，下一帧要等的一帧比一帧久，
//! 等满一帧就跳掉一帧再从头来：平均多出半帧的延迟，还每隔一阵顿一下。现在时间线跟着画面的时间戳走，
//! 和处理花了多久无关，不会累积。
//!
//! * 时间线落后了（画面静止了一阵、编码慢了）：从这一帧重新排，不会为了补上而连着处理好几帧；
//! * 画面比时间线早（采集的抖动，屏幕比设定的帧率快一点点，例如 60.001 Hz）：时间线每帧最多往前挪 1/64 个周期，
//!   跟得上这样的屏幕，不会一直落后它、越等越久；长期的帧率最多比设定的高 1/64。

use std::time::{Duration, Instant};

pub struct Pacer {
    period: Duration,
    /// 时间线上下一帧的位置
    next: Option<Instant>,
}

impl Pacer {
    pub fn new(fps: u8) -> Self {
        Self {
            period: period(fps),
            next: None,
        }
    }

    pub fn set_fps(&mut self, fps: u8) {
        self.period = period(fps);
    }

    /// 现在（`now`）还要等多久才能处理下一帧
    pub fn wait(&self, now: Instant) -> Duration {
        self.next
            .and_then(|next| next.checked_sub(self.period / 4))
            .map_or(Duration::ZERO, |earliest| {
                earliest.saturating_duration_since(now)
            })
    }

    /// 处理了时间戳为 `stamp` 的一帧（采集的时刻；重新编码的旧画面用现在）
    pub fn done(&mut self, stamp: Instant) {
        let base = match self.next {
            Some(next) => stamp.max(next.checked_sub(self.period / 64).unwrap_or(next)),
            None => stamp,
        };
        self.next = Some(base + self.period);
    }
}

fn period(fps: u8) -> Duration {
    Duration::from_secs(1) / fps.max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u8 = 60;
    const T: f64 = 1000.0 / FPS as f64;

    /// 采集线程：等到时间线允许，再取画面源的下一帧（DXGI / ScreenCaptureKit 在等待期间留着最新的一帧）。
    /// 画面源刷新率 `hz`，处理一帧要 3 ms。返回每一帧 (时间戳, 取到它的时刻)，纳秒
    fn capture(hz: f64, secs: u64) -> Vec<(u64, u64)> {
        let source = (1e9 / hz).round() as u64;
        let t0 = Instant::now();
        let at = |ns: u64| t0 + Duration::from_nanos(ns);
        let mut p = Pacer::new(FPS);
        let start = 1_000_000_000;
        let mut now = start;
        let mut got: Vec<(u64, u64)> = Vec::new();
        while now < start + secs * 1_000_000_000 {
            now += p.wait(at(now)).as_nanos() as u64;
            // 等待期间出过画面就拿最新的一帧，没有就等下一帧
            let latest = now / source * source;
            let stamp = if got.last().is_none_or(|&(s, _)| latest > s) {
                latest
            } else {
                latest + source
            };
            now = now.max(stamp);
            got.push((stamp, now));
            p.done(at(stamp));
            now += 3_000_000;
        }
        got
    }

    /// 屏幕按设定的帧率出画面：每一帧都按时取到，不等、不跳，处理花的时间不会累积成延迟
    #[test]
    fn a_source_at_the_frame_rate_is_never_delayed() {
        for hz in [60.0, 59.94, 60.001] {
            let got = capture(hz, 10);
            let source = (1e9 / hz).round() as u64;
            let n = got.len() as f64;
            assert!(n >= 10.0 * hz - 1.0, "{hz} Hz: {n}");
            // 第一帧是开始之前就有的旧画面，处理它的时候下一帧到了
            for w in got.windows(2).skip(1) {
                assert_eq!(w[1].0 - w[0].0, source, "{hz} Hz: skipped at {w:?}");
                assert_eq!(w[1].1, w[1].0, "{hz} Hz: delayed at {w:?}");
            }
        }
    }

    /// 屏幕比设定的帧率快（120 / 144 Hz 配 60 fps）：只取一部分，帧率差不多就是设定的
    #[test]
    fn a_faster_source_is_thinned_to_the_frame_rate() {
        for hz in [120.0, 144.0, 240.0] {
            let got = capture(hz, 10);
            let fps = got.len() as f64 / 10.0;
            assert!((58.0..=62.0).contains(&fps), "{hz} Hz: {fps} fps");
            // 取到的都是最新的画面：最多晚一帧（画面源的）
            let source = (1e9 / hz).round() as u64;
            for &(stamp, now) in &got {
                assert!(now - stamp < source, "{hz} Hz: {stamp} at {now}");
            }
        }
    }

    /// 静止了一阵之后：新画面马上处理，之后照常按帧率，不会为了补上而连着处理
    #[test]
    fn after_a_pause_the_timeline_restarts() {
        let t0 = Instant::now();
        let mut p = Pacer::new(FPS);
        assert_eq!(p.wait(t0), Duration::ZERO);
        p.done(t0);
        assert!(p.wait(t0) > Duration::from_millis(12));
        let later = t0 + Duration::from_secs(5);
        assert_eq!(p.wait(later), Duration::ZERO);
        p.done(later);
        let w = p.wait(later).as_secs_f64() * 1000.0;
        assert!((w - T * 0.75).abs() < 0.01, "{w}");
        // 帧率变了：下一帧按新的周期
        p.set_fps(30);
        p.done(later);
        let w = p.wait(later).as_secs_f64() * 1000.0;
        assert!(w > 20.0, "{w}");
    }
}
