//! 画面、声音走哪条路（被控端决定，见 `airhand_proto` 的"直连"）。
//!
//! 被控端每 [`PROBE_EVERY`] 在中转、直连上各发一个探测数据报，控制端（重新加密之后）从同一条路弹回来，
//! 被控端就知道每条路的往返时延（和画面一样要经过中转的两段、中转本身）。每条路取最近 [`SAMPLES`] 次
//! （约 10 秒）里**最小**的：这是路本身的时延。画面走的那条路上难免排一点队（码率控制允许几十毫秒），
//! 拿排着队的时延去和闲着的那条比，就会换过去、那边又排上队、再换回来；排队归码率控制管（`crate::flow`），
//! 选路只看路本身。
//!
//! * 直连不比中转慢（最多慢 [`PREFER_DIRECT_MS`]：差不多时走直连，省下中转的流量）就换到直连。
//!   第一次是一量出来就换，之后要一直这样 [`SWITCH_AFTER`] 才换：免得来回切换；
//! * 中转比直连快出一截（[`back_margin`]）、一直这样 [`SWITCH_BACK_AFTER`]，换回中转；
//! * 直连 [`dead_after`] 没有回声（断了，或者堵死了），马上换回中转，并且说明在途的帧丢了
//!   （[`Switch::lost`]：被控端随即编一个关键帧、不再等那些帧的确认）；之后 [`HOLD_AFTER_LOSS`] 内不换回去。

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::Via;

/// 多久探测一次
pub const PROBE_EVERY: Duration = Duration::from_millis(250);
/// 每条路取最近这么多次（约 10 秒）里最小的
pub const SAMPLES: usize = 40;
/// 至少这么多次才算量出来了
const MIN_SAMPLES: usize = 3;
/// 直连比中转慢不超过这么多（毫秒）也走直连
pub const PREFER_DIRECT_MS: f64 = 2.0;
/// 直连一直不比中转慢这么久才换过去（第一次除外）
pub const SWITCH_AFTER: Duration = Duration::from_secs(2);
/// 中转一直明显更快这么久才换回中转
pub const SWITCH_BACK_AFTER: Duration = Duration::from_secs(3);
/// 直连出过问题之后这么久不换回去
pub const HOLD_AFTER_LOSS: Duration = Duration::from_secs(5);

/// 一条路这么久没有回声就算断了：至少 1 秒，往返时延长的路按 4 个往返算
pub fn dead_after(rtt_ms: Option<f64>) -> Duration {
    let rtt = Duration::from_secs_f64(rtt_ms.unwrap_or(0.0) / 1000.0);
    (rtt * 4).max(Duration::from_secs(1))
}

/// 中转要比直连快这么多（毫秒）才换回去：至少 5 ms，或者中转往返时延的 10%
pub fn back_margin(relay_ms: f64) -> f64 {
    (relay_ms * 0.1).max(5.0)
}

/// 换路
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    pub to: Via,
    /// 换走是因为直连断了：在途的帧到不了
    pub lost: bool,
}

#[derive(Default)]
struct Path {
    rtts: VecDeque<f64>,
    last: Option<Instant>,
}

impl Path {
    fn sample(&mut self, rtt_ms: f64, now: Instant) {
        if self.rtts.len() == SAMPLES {
            self.rtts.pop_front();
        }
        self.rtts.push_back(rtt_ms);
        self.last = Some(now);
    }

    /// 最近这些次里最小的（毫秒，路本身的时延）；次数还不够时为 `None`
    fn rtt(&self) -> Option<f64> {
        if self.rtts.len() < MIN_SAMPLES {
            return None;
        }
        self.rtts.iter().copied().reduce(f64::min)
    }

    /// 最近有回声
    fn alive(&self, now: Instant) -> bool {
        self.last
            .is_some_and(|t| now.saturating_duration_since(t) < dead_after(self.rtt()))
    }
}

/// 一条直连从通了到断了，画面、声音走哪条路（见模块说明）
pub struct Selector {
    relay: Path,
    direct: Path,
    media: Via,
    /// 另一条路从这时起一直比现在走的好
    better_since: Option<Instant>,
    /// 这之前不换到直连（它刚出过问题）
    hold_until: Option<Instant>,
    /// 走过直连了（第一次换过去不用等）
    used_direct: bool,
}

impl Default for Selector {
    fn default() -> Self {
        Self {
            relay: Path::default(),
            direct: Path::default(),
            media: Via::Relay,
            better_since: None,
            hold_until: None,
            used_direct: false,
        }
    }
}

impl Selector {
    /// 现在走哪条路
    pub fn media(&self) -> Via {
        self.media
    }

    /// 一条路的往返时延（毫秒），给日志看
    pub fn rtt(&self, via: Via) -> Option<f64> {
        self.path(via).rtt()
    }

    fn path(&self, via: Via) -> &Path {
        match via {
            Via::Relay => &self.relay,
            Via::Direct => &self.direct,
        }
    }

    /// `via` 上的探测弹回来了，用了 `rtt_ms`
    pub fn sample(&mut self, via: Via, rtt_ms: f64, now: Instant) {
        match via {
            Via::Relay => self.relay.sample(rtt_ms, now),
            Via::Direct => self.direct.sample(rtt_ms, now),
        }
    }

    /// 往直连上写画面失败了：当它断了
    pub fn broken(&mut self, now: Instant) -> Option<Switch> {
        self.direct.last = None;
        self.decide(now)
    }

    /// 每次探测之后看一看要不要换路
    pub fn decide(&mut self, now: Instant) -> Option<Switch> {
        let direct_alive = self.direct.alive(now);
        let (to, lost) = match self.media {
            Via::Direct if !direct_alive => (Via::Relay, true),
            Via::Direct => {
                let (Some(d), Some(r)) = (self.direct.rtt(), self.relay.rtt()) else {
                    self.better_since = None;
                    return None;
                };
                if !(self.relay.alive(now) && r + back_margin(r) < d) {
                    self.better_since = None;
                    return None;
                }
                let since = *self.better_since.get_or_insert(now);
                if now.saturating_duration_since(since) < SWITCH_BACK_AFTER {
                    return None;
                }
                (Via::Relay, false)
            }
            Via::Relay => {
                let held = self.hold_until.is_some_and(|t| now < t);
                let Some(d) = self.direct.rtt().filter(|_| direct_alive && !held) else {
                    self.better_since = None;
                    return None;
                };
                // 中转上的探测没有回声（量不出来）：直连通着就走直连
                let better = match self.relay.rtt() {
                    Some(r) if self.relay.alive(now) => d <= r + PREFER_DIRECT_MS,
                    _ => true,
                };
                if !better {
                    self.better_since = None;
                    return None;
                }
                let since = *self.better_since.get_or_insert(now);
                if self.used_direct && now.saturating_duration_since(since) < SWITCH_AFTER {
                    return None;
                }
                self.used_direct = true;
                (Via::Direct, false)
            }
        };
        self.media = to;
        self.better_since = None;
        if lost {
            self.hold_until = Some(now + HOLD_AFTER_LOSS);
        }
        Some(Switch { to, lost })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 [`PROBE_EVERY`] 往前走，两条路各回一次声（`None`：没有回声），返回这期间的换路
    fn run(
        s: &mut Selector,
        now: &mut Instant,
        ticks: usize,
        relay: Option<f64>,
        direct: Option<f64>,
    ) -> Vec<Switch> {
        let mut out = Vec::new();
        for _ in 0..ticks {
            *now += PROBE_EVERY;
            if let Some(r) = relay {
                s.sample(Via::Relay, r, *now);
            }
            if let Some(d) = direct {
                s.sample(Via::Direct, d, *now);
            }
            out.extend(s.decide(*now));
        }
        out
    }

    fn to(via: Via, lost: bool) -> Switch {
        Switch { to: via, lost }
    }

    /// 直连更快：一量出来就换过去；直连慢不了多少也走直连
    #[test]
    fn a_faster_direct_path_is_used_as_soon_as_it_is_measured() {
        let mut s = Selector::default();
        let mut now = Instant::now();
        // 量出来之前不换
        assert!(run(&mut s, &mut now, 2, Some(80.0), Some(20.0)).is_empty());
        assert_eq!(
            run(&mut s, &mut now, 1, Some(80.0), Some(20.0)),
            [to(Via::Direct, false)]
        );
        assert_eq!(s.media(), Via::Direct);
        let mut s = Selector::default();
        let switches = run(&mut s, &mut now, 4, Some(30.0), Some(31.5));
        assert_eq!(switches, [to(Via::Direct, false)]);
    }

    /// 直连比中转慢（例如两端的运营商之间绕路）：一直走中转
    #[test]
    fn a_slower_direct_path_is_not_used() {
        let mut s = Selector::default();
        let mut now = Instant::now();
        assert!(run(&mut s, &mut now, 40, Some(30.0), Some(60.0)).is_empty());
        assert_eq!(s.media(), Via::Relay);
    }

    /// 画面走的直连上排着队（码率控制允许的那几十毫秒）：不算直连变慢，不换回中转
    #[test]
    fn queueing_on_the_media_path_does_not_make_it_flap() {
        let mut s = Selector::default();
        let mut now = Instant::now();
        run(&mut s, &mut now, 3, Some(40.0), Some(25.0));
        assert_eq!(s.media(), Via::Direct);
        // 直连上大多数探测都排在画面后面（25 + 30 ms），偶尔有一个没排队
        for _ in 0..20 {
            assert!(run(&mut s, &mut now, 9, Some(40.0), Some(55.0)).is_empty());
            assert!(run(&mut s, &mut now, 1, Some(40.0), Some(25.0)).is_empty());
        }
    }

    /// 直连本身变慢了（换了路由）：中转一直明显更快才换回去；直连又变快了也要等一会儿才换回来
    #[test]
    fn switches_back_only_when_the_relay_is_clearly_faster_for_a_while() {
        let mut s = Selector::default();
        let mut now = Instant::now();
        run(&mut s, &mut now, 3, Some(50.0), Some(20.0));
        assert_eq!(s.media(), Via::Direct);
        // 只慢一点点（在 back_margin 之内）：不换
        assert!(run(&mut s, &mut now, 60, Some(50.0), Some(54.0)).is_empty());
        // 一直明显更慢：最快的那次也比中转慢（旧的样本出了窗口）之后再等一会儿才换
        let ticks = SAMPLES + (SWITCH_BACK_AFTER.as_millis() / PROBE_EVERY.as_millis()) as usize;
        let early = run(&mut s, &mut now, SAMPLES - 1, Some(50.0), Some(90.0));
        assert!(early.is_empty(), "{early:?}");
        let switches = run(&mut s, &mut now, ticks, Some(50.0), Some(90.0));
        assert_eq!(switches, [to(Via::Relay, false)]);
        // 又快回来了：不会马上换回去，过一会儿才换
        let quick = run(&mut s, &mut now, 4, Some(50.0), Some(20.0));
        assert!(quick.is_empty(), "{quick:?}");
        let later = run(&mut s, &mut now, 12, Some(50.0), Some(20.0));
        assert_eq!(later, [to(Via::Direct, false)]);
    }

    /// 直连没有回声了：马上换回中转（在途的帧丢了），之后一阵子不换回去
    #[test]
    fn a_dead_direct_path_fails_over_immediately() {
        let mut s = Selector::default();
        let mut now = Instant::now();
        run(&mut s, &mut now, 3, Some(50.0), Some(20.0));
        assert_eq!(s.media(), Via::Direct);
        let switches = run(&mut s, &mut now, 4, Some(50.0), None);
        assert_eq!(switches, [to(Via::Relay, true)]);
        // 很快又通了：先等 HOLD_AFTER_LOSS
        let ticks = (HOLD_AFTER_LOSS.as_millis() / PROBE_EVERY.as_millis()) as usize;
        let held = run(&mut s, &mut now, ticks - 4, Some(50.0), Some(20.0));
        assert!(held.is_empty(), "{held:?}");
        let back = run(&mut s, &mut now, 20, Some(50.0), Some(20.0));
        assert_eq!(back, [to(Via::Direct, false)]);
        // 往直连上写失败：也一样
        assert_eq!(s.broken(now), Some(to(Via::Relay, true)));
        assert_eq!(s.broken(now), None);
    }

    /// 中转上的探测没有回声：直连通着就走直连，不会因为量不出中转而一直等
    #[test]
    fn a_silent_relay_does_not_block_the_direct_path() {
        let mut s = Selector::default();
        let mut now = Instant::now();
        let switches = run(&mut s, &mut now, 3, None, Some(20.0));
        assert_eq!(switches, [to(Via::Direct, false)]);
        // 直连上一直没有中转可比：不换回去
        assert!(run(&mut s, &mut now, 20, None, Some(25.0)).is_empty());
    }

    #[test]
    fn timeouts_scale_with_the_path() {
        assert_eq!(dead_after(None), Duration::from_secs(1));
        assert_eq!(dead_after(Some(20.0)), Duration::from_secs(1));
        assert_eq!(dead_after(Some(500.0)), Duration::from_secs(2));
        assert_eq!(back_margin(20.0), 5.0);
        assert_eq!(back_margin(300.0), 30.0);
    }
}
