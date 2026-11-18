//! 被控端的参考帧：每一帧参考哪一帧（写进画面包头，见 `airhand_proto::media` 的"参考帧"），控制端等不到帧时怎么恢复
//! （`ControllerDatagram::Lost`，见 `airhand_proto::session` 的"帧流"）。和编码器无关，编码器只要能做两件事：
//!
//! * 把一帧标成**长期参考帧**放进某个槽位（[`Plan::mark`]）；
//! * 下一帧只参考某个槽位里的长期参考帧，之后的帧也不再参考这之前的短期参考帧、别的槽位（[`Plan::recover`]，
//!   参考帧失效）。
//!
//! 并且如实报告每一帧标进了哪个槽位、参考了哪些槽位（[`LtrReport`]；硬件编码器在关键帧之后常常自己标几帧）。
//! 做不到的编码器（槽位数为 0）恢复时只能编关键帧。
//!
//! # 每一帧参考哪一帧
//!
//! 增量帧参考前一帧（编码器可能还参考了更早的帧、长期参考帧，但都在前一帧的依赖里：前一帧解得了，它们也都解过了）。
//! 恢复帧只参考那个槽位里的帧。所以每一帧的依赖是一条链，控制端只要看它参考的那一帧交给过解码器没有。
//!
//! # 恢复
//!
//! 控制端说它交给解码器的最后一帧是 X（`Lost { decoded: X }`），X 之后的都解不了了：
//!
//! * 在途的、X 之后的帧作废（调用的一方放弃它们，见 `crate::flow::FlowState::abandon`）；
//! * 下一帧从**序号不比 X 新的、最新的有效槽位**恢复：有效的槽位都在现在这条链上（恢复时别的槽位、关键帧时所有槽位
//!   都作废），不比 X 新的就是 X 的依赖，控制端一定解过它；
//! * 没有这样的槽位（刚开始、编码器做不到、编码器没照做）：编关键帧；
//! * 已经发出去的恢复帧（关键帧）控制端解得了、还在路上（[`recover_wait`]），重复的请求不再编。
//!
//! # 只能编关键帧时
//!
//! 关键帧比等重传贵得多：它大，要传好几帧的时间，之后的帧都得等它。控制端报告等不到的时候多半是重传还在路上
//! （帧流上的帧最后总会到，除非被放弃了），所以恢复只能编关键帧时（没有能用的槽位、编码器做不到），
//! 等同一处的第二次报告才编关键帧（[`KeyframeGate`]）。控制端每两个往返重报一次（`crate::merge`）。
//! 期间重传到了就不用编关键帧，有可用的长期参考帧则在第一次报告时恢复。
//!
//! # 什么时候标长期参考帧
//!
//! 标得勤，恢复时参考的帧就新，恢复帧就小；但是不能把控制端确认收到了的最新的那个槽位换掉：所以最新的槽位被确认了、
//! 又过了 [`MARK_EVERY`]，才把最旧的槽位换成下一帧。

use std::time::{Duration, Instant};

/// 两次标长期参考帧至少隔这么久
pub const MARK_EVERY: Duration = Duration::from_millis(250);

/// 只能编关键帧时，同一处的第二次报告至少要晚这么久（控制端最快 100 ms 重报一次；更快的是同一次报告的副本）
pub const KEY_REPEAT_MIN: Duration = Duration::from_millis(50);
/// 只能编关键帧时，第一次报告过了这么久还没有第二次，就当作没报告过
const KEY_REPORT_TTL: Duration = Duration::from_secs(5);

/// 恢复只能编关键帧时的报告（见模块说明的"只能编关键帧时"）：同一处的报告第二次来了才编
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyframeGate {
    /// 第一次报告：控制端解到的帧、报告的时刻
    first: Option<(u32, Instant)>,
}

impl KeyframeGate {
    /// 控制端报告解到第 `decoded` 帧之后等不到了，恢复只能编关键帧。`true`：编（同一处报告第二次了）；
    /// `false`：先等着（第一次，或者紧跟着第一次的副本）
    pub fn confirm(&mut self, decoded: u32, now: Instant) -> bool {
        match self.first {
            Some((d, at)) if d == decoded && now.saturating_duration_since(at) < KEY_REPORT_TTL => {
                let again = now.saturating_duration_since(at) >= KEY_REPEAT_MIN;
                if again {
                    self.first = None;
                }
                again
            }
            _ => {
                self.first = Some((decoded, now));
                false
            }
        }
    }
}

/// 发出去的恢复帧多久之内算还在路上：重复的恢复请求不再编（往返时延的两倍多一点，至少 300 毫秒）
pub fn recover_wait(srtt_ms: f64) -> Duration {
    Duration::from_secs_f64(((srtt_ms * 2.0 + 100.0) / 1000.0).max(0.3))
}

/// 编码器报告的一帧的长期参考帧
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LtrReport {
    /// 这一帧标进了哪个槽位
    pub marked: Option<u8>,
    /// 这一帧参考了哪些槽位（第 i 位是第 i 个槽位）
    pub used: u16,
}

/// 下一帧怎么编
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Plan {
    /// 编关键帧
    pub key: bool,
    /// 标成长期参考帧放进这个槽位
    pub mark: Option<u8>,
    /// 只参考这个槽位里的长期参考帧（恢复）
    pub recover: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    seq: u32,
    /// 还能用来恢复（恢复时别的槽位作废）
    valid: bool,
}

/// 最近一次恢复（关键帧也算）
#[derive(Debug, Clone, Copy)]
struct Recovery {
    seq: u32,
    /// 恢复帧参考的帧；关键帧为 `None`
    base: Option<u32>,
    at: Instant,
}

/// `a` 比 `b` 新（序号会绕回）
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// 一个会话的参考帧（见模块说明）
#[derive(Debug, Default)]
pub struct Refs {
    slots: Vec<Option<Slot>>,
    /// 上一帧的序号（增量帧参考它）
    last: Option<u32>,
    /// 上次标长期参考帧的时刻
    marked_at: Option<Instant>,
    /// 控制端确认到了哪一帧
    acked: Option<u32>,
    /// 控制端要从这一帧恢复，还没编出恢复帧
    pending: Option<u32>,
    recovered: Option<Recovery>,
    /// 编码器没照做（恢复帧参考了别的）：之后恢复都编关键帧
    unreliable: bool,
    /// 恢复只能编关键帧时的第一次报告（见模块说明的"只能编关键帧时"）
    key_gate: KeyframeGate,
}

impl Refs {
    /// 控制端确认到了第 `seq` 帧（累积）
    pub fn acked(&mut self, seq: u32) {
        if self.acked.is_none_or(|a| after(seq, a)) {
            self.acked = Some(seq);
        }
    }

    /// 控制端交给解码器的最后一帧是 `decoded`，之后的等不到了（见模块说明的"恢复"）。返回 `true`：要恢复，
    /// 在途的、`decoded` 之后的帧作废；`false`：控制端解得了的恢复帧已经在路上了，这是重复的请求；
    /// 或者只能编关键帧且这是同一处的第一次报告。
    pub fn lost(&mut self, decoded: u32, srtt_ms: f64, now: Instant) -> bool {
        if let Some(r) = self.recovered
            && after(r.seq, decoded)
            && r.base.is_none_or(|b| !after(b, decoded))
            && now.saturating_duration_since(r.at) < recover_wait(srtt_ms)
        {
            return false;
        }
        if self.usable_slot(decoded).is_none() && !self.key_gate.confirm(decoded, now) {
            return false;
        }
        self.pending = Some(decoded);
        true
    }

    /// 从 `decoded` 恢复用哪个槽位：序号不比它新的、最新的有效槽位（见模块说明的"恢复"）。没有、编码器没照做过时为 `None`
    fn usable_slot(&self, decoded: u32) -> Option<u8> {
        if self.unreliable {
            return None;
        }
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, (*s)?)))
            .filter(|(_, s)| s.valid && !after(s.seq, decoded))
            .max_by_key(|(_, s)| s.seq.wrapping_sub(decoded) as i32)
            .map(|(i, _)| i as u8)
    }

    /// 有恢复请求还没处理：画面静止时也要编一帧
    pub fn recovering(&self) -> bool {
        self.pending.is_some()
    }

    /// 下一帧怎么编。`slots`：编码器的槽位数（0：不能控制长期参考帧）。不改状态：编码器这一帧没编出来，下次再来
    pub fn plan(&mut self, slots: u8, now: Instant) -> Plan {
        if self.slots.len() != slots as usize {
            // 换了编码器（编码器重建时总是先编关键帧）
            self.slots = vec![None; slots as usize];
            self.marked_at = None;
        }
        if let Some(decoded) = self.pending {
            return match self.usable_slot(decoded) {
                Some(i) => Plan {
                    recover: Some(i),
                    ..Plan::default()
                },
                None => Plan {
                    key: true,
                    ..Plan::default()
                },
            };
        }
        if slots == 0 || self.unreliable {
            return Plan::default();
        }
        Plan {
            mark: self.mark_due(now),
            ..Plan::default()
        }
    }

    /// 该标长期参考帧了的话，标进哪个槽位：空的，或者最旧的（最新的那个控制端确认了之后才换，见模块说明）
    fn mark_due(&self, now: Instant) -> Option<u8> {
        if let Some(i) = self.slots.iter().position(|s| !s.is_some_and(|s| s.valid)) {
            return Some(i as u8);
        }
        if self.marked_at.is_some_and(|t| now < t + MARK_EVERY) {
            return None;
        }
        let newest = self
            .slots
            .iter()
            .flatten()
            .map(|s| s.seq)
            .reduce(|a, b| if after(b, a) { b } else { a })?;
        if !self.acked.is_some_and(|a| !after(newest, a)) {
            return None;
        }
        let oldest = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, (*s)?.seq)))
            .min_by_key(|(_, seq)| seq.wrapping_sub(newest) as i32)?;
        Some(oldest.0 as u8)
    }

    /// 编码器按 `plan` 编出了第 `seq` 帧（`key`：是关键帧；`report`：编码器报告的长期参考帧）。
    /// 返回它参考的帧，写进画面包头（关键帧为 `None`）
    pub fn encoded(
        &mut self,
        seq: u32,
        key: bool,
        plan: Plan,
        report: Option<LtrReport>,
        now: Instant,
    ) -> Option<u32> {
        let reference = if key {
            // 关键帧之后所有的长期参考帧都作废（编码器可能马上自己标几帧，见下面的 report）
            self.slots.iter_mut().for_each(|s| *s = None);
            self.pending = None;
            self.key_gate = KeyframeGate::default();
            self.recovered = Some(Recovery {
                seq,
                base: None,
                at: now,
            });
            None
        } else if let Some(i) = plan.recover {
            let base = self.slots.get(i as usize).copied().flatten().map(|s| s.seq);
            match (base, report) {
                // 照做了：只参考了那个槽位
                (Some(base), Some(r)) if r.used == 1 << i => {
                    for (j, s) in self.slots.iter_mut().enumerate() {
                        if j != i as usize
                            && let Some(s) = s
                        {
                            s.valid = false;
                        }
                    }
                    self.pending = None;
                    self.recovered = Some(Recovery {
                        seq,
                        base: Some(base),
                        at: now,
                    });
                    Some(base)
                }
                // 没照做：不知道它参考了什么，当它参考前一帧（控制端解不了，会再要）；之后都编关键帧
                _ => {
                    tracing::warn!(
                        ?report,
                        slot = i,
                        "the encoder didn't recover from the long-term reference frame, using keyframes"
                    );
                    self.unreliable = true;
                    self.last
                }
            }
        } else {
            self.last
        };
        if let Some(i) = report.and_then(|r| r.marked)
            && let Some(s) = self.slots.get_mut(i as usize)
        {
            *s = Some(Slot { seq, valid: true });
        }
        if plan.mark.is_some() {
            self.marked_at = Some(now);
        }
        self.last = Some(seq);
        reference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个照做的编码器：关键帧之后自己把头两帧标进 0、1 号槽位（和 Media Foundation 的 H.264 编码器一样）
    struct Encoder {
        slots: u8,
        auto: u8,
    }

    impl Encoder {
        fn encode(&mut self, plan: Plan, forced: bool) -> (bool, LtrReport) {
            let key = plan.key || forced;
            if key {
                self.auto = 0;
            }
            let mut r = LtrReport::default();
            if let Some(i) = plan.recover.filter(|_| !key) {
                r.used = 1 << i;
            }
            if self.auto < self.slots {
                r.marked = Some(self.auto);
                self.auto += 1;
            } else if let Some(i) = plan.mark {
                r.marked = Some(i);
            }
            (key, r)
        }
    }

    struct Sim {
        refs: Refs,
        enc: Encoder,
        seq: u32,
        now: Instant,
    }

    impl Sim {
        fn new(slots: u8) -> Self {
            Self {
                refs: Refs::default(),
                enc: Encoder { slots, auto: 0 },
                seq: 0,
                now: Instant::now(),
            }
        }

        /// 编一帧：(序号, 关键帧, 参考帧)
        fn frame(&mut self, forced: bool) -> (u32, bool, Option<u32>) {
            self.now += Duration::from_millis(100);
            let plan = self.refs.plan(self.enc.slots, self.now);
            let (key, report) = self.enc.encode(plan, forced);
            self.seq += 1;
            let report = (self.enc.slots > 0).then_some(report);
            let r = self.refs.encoded(self.seq, key, plan, report, self.now);
            (self.seq, key, r)
        }
    }

    #[test]
    fn delta_frames_reference_the_previous_one() {
        let mut s = Sim::new(0);
        assert_eq!(s.frame(true), (1, true, None));
        assert_eq!(s.frame(false), (2, false, Some(1)));
        assert_eq!(s.frame(false), (3, false, Some(2)));
    }

    /// 编码器不能控制长期参考帧：恢复只能编关键帧，同一处的报告第二次来了才编（第一次多半是重传还在路上）
    #[test]
    fn without_long_term_references_recovery_is_a_keyframe() {
        let mut s = Sim::new(0);
        for _ in 0..4 {
            s.frame(false);
        }
        // 第一次报告：不恢复，接着编增量帧
        assert!(!s.refs.lost(2, 50.0, s.now));
        assert!(!s.refs.recovering());
        assert_eq!(s.frame(false), (5, false, Some(4)));
        // 紧跟着的副本不算第二次
        assert!(!s.refs.lost(2, 50.0, s.now - Duration::from_millis(90)));
        // 控制端重报（还是解到第 2 帧）：编关键帧
        assert!(s.refs.lost(2, 50.0, s.now + Duration::from_millis(100)));
        assert!(s.refs.recovering());
        assert_eq!(s.frame(false), (6, true, None));
        assert!(!s.refs.recovering());
        assert_eq!(s.frame(false), (7, false, Some(6)));
        // 别处（控制端解到的帧不一样）的报告从头算
        assert!(!s.refs.lost(5, 50.0, s.now + recover_wait(50.0)));
        assert!(!s.refs.lost(7, 50.0, s.now + recover_wait(50.0)));
        assert!(
            s.refs
                .lost(7, 50.0, s.now + recover_wait(50.0) + KEY_REPEAT_MIN)
        );
    }

    /// 第一次报告过了太久才又来一次：当作第一次
    #[test]
    fn an_old_first_report_is_forgotten() {
        let mut g = KeyframeGate::default();
        let t0 = Instant::now();
        assert!(!g.confirm(3, t0));
        assert!(!g.confirm(3, t0 + KEY_REPORT_TTL));
        assert!(g.confirm(3, t0 + KEY_REPORT_TTL + KEY_REPEAT_MIN));
        // 编过了：之后同一处的报告又从第一次算
        assert!(!g.confirm(3, t0 + KEY_REPORT_TTL * 2));
    }

    /// 从序号不比控制端解到的帧新的、最新的有效槽位恢复；恢复之后别的槽位作废
    #[test]
    fn recovery_uses_the_newest_slot_the_controller_has() {
        let mut s = Sim::new(2);
        // 1（关键帧）、2 自己标进了 0、1 号槽位
        s.frame(true);
        s.frame(false);
        for _ in 3..=6 {
            s.frame(false);
        }
        // 控制端确认到了 6：最新的槽位（2）确认过了，过了 MARK_EVERY 就把最旧的（0 号，第 1 帧）换成下一帧
        s.refs.acked(6);
        assert_eq!(s.refs.plan(2, s.now + MARK_EVERY).mark, Some(0));
        assert_eq!(s.frame(false), (7, false, Some(6)));
        assert_eq!(
            s.refs.slots[0],
            Some(Slot {
                seq: 7,
                valid: true
            })
        );
        // 第 7 帧还没确认：不换第 2 帧
        for _ in 8..=10 {
            assert_eq!(s.refs.plan(2, s.now + MARK_EVERY).mark, None);
            s.frame(false);
        }
        // 控制端解到了 5：第 7 帧比 5 新，用第 2 帧（1 号槽位）恢复
        assert!(s.refs.lost(5, 50.0, s.now));
        assert_eq!(s.frame(false), (11, false, Some(2)));
        assert!(!s.refs.slots[0].unwrap().valid);
        assert_eq!(s.frame(false), (12, false, Some(11)));
        // 恢复帧在路上，重复的请求不再编；过了一阵还要，就再恢复
        assert!(!s.refs.lost(5, 50.0, s.now));
        assert!(s.refs.lost(5, 50.0, s.now + recover_wait(50.0)));
        // 0 号槽位又标了第 12 帧（比 5 新）：还是第 2 帧
        assert_eq!(s.frame(false), (13, false, Some(2)));
    }

    /// 控制端解到的帧比所有槽位都旧（刚开始）：编关键帧，之后编码器重新标槽位
    #[test]
    fn no_usable_slot_means_a_keyframe() {
        let mut s = Sim::new(2);
        s.frame(true);
        s.frame(false);
        s.frame(false);
        // 第 1 帧（关键帧）刚发出去不久时控制端说什么都没解到：关键帧还在路上
        assert!(!s.refs.lost(0, 50.0, s.now));
        // 过了一阵还要：只能编关键帧，第二次报告来了才编
        let later = s.now + recover_wait(50.0);
        assert!(!s.refs.lost(0, 50.0, later));
        assert!(s.refs.lost(0, 50.0, later + KEY_REPEAT_MIN));
        assert_eq!(s.frame(false), (4, true, None));
        assert_eq!(
            s.refs.slots[0],
            Some(Slot {
                seq: 4,
                valid: true
            })
        );
        assert_eq!(s.frame(false), (5, false, Some(4)));
        // 控制端还没收到关键帧时又报了（解到的还是 0）：关键帧解得了、还在路上，不再编
        assert!(!s.refs.lost(0, 50.0, s.now));
    }

    /// 编码器没照做（恢复帧还参考了别的槽位）：不能说它参考了哪一帧，之后都编关键帧
    #[test]
    fn an_encoder_that_ignores_recovery_falls_back_to_keyframes() {
        let mut r = Refs::default();
        let now = Instant::now();
        let mark = |i| {
            Some(LtrReport {
                marked: Some(i),
                used: 0,
            })
        };
        r.plan(2, now);
        r.encoded(1, true, Plan::default(), mark(0), now);
        r.encoded(2, false, Plan::default(), mark(1), now);
        r.encoded(3, false, Plan::default(), Some(LtrReport::default()), now);
        assert!(r.lost(2, 50.0, now));
        let plan = r.plan(2, now);
        assert_eq!(plan.recover, Some(1));
        let bad = Some(LtrReport {
            marked: None,
            used: 0b11,
        });
        assert_eq!(r.encoded(4, false, plan, bad, now), Some(3));
        // 请求还在：下一帧编关键帧
        assert!(r.plan(2, now).key);
        assert_eq!(
            r.encoded(
                5,
                true,
                Plan {
                    key: true,
                    ..Plan::default()
                },
                None,
                now
            ),
            None
        );
        // 之后也不标长期参考帧了
        assert_eq!(r.plan(2, now + MARK_EVERY * 10), Plan::default());
    }

    /// 序号绕回
    #[test]
    fn sequence_numbers_wrap() {
        let mut r = Refs::default();
        let now = Instant::now();
        r.plan(2, now);
        let mark = |i| {
            Some(LtrReport {
                marked: Some(i),
                used: 0,
            })
        };
        r.encoded(u32::MAX - 1, true, Plan::default(), mark(0), now);
        r.encoded(u32::MAX, false, Plan::default(), mark(1), now);
        r.encoded(0, false, Plan::default(), Some(LtrReport::default()), now);
        r.encoded(1, false, Plan::default(), Some(LtrReport::default()), now);
        assert!(r.lost(0, 50.0, now));
        assert_eq!(r.plan(2, now).recover, Some(1));
    }
}
