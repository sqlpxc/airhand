//! 虚拟显示器下一步该做什么（纯逻辑，执行见 `super::manager`）。
//!
//! 每一轮看一眼现在的样子（[`Facts`]）和想要什么（[`Wants`]），只决定一步（[`Step`]）；执行完下一轮再看。
//! 中途发生了什么（显示器插拔、别的程序改了显示设置、启用之后虚拟显示器还没出来）都在下一轮按新的样子处理，
//! 不用记一长串计划、也不怕执行到一半被打断。
//!
//! * **防窥**：启用虚拟显示器 → 等它出来 → 只留它在桌面上（撤下本机的显示器）。之后本机又有显示器回到桌面上
//!   （有人重新插了显示器、Windows 按记着的设置放回去了）就再撤一次；
//! * **防窥结束**：先停用虚拟显示器（它是桌面上唯一的显示器，没了之后 Windows 自己就把本机的显示器放回去），
//!   再按 Windows 记着的设置恢复一遍；
//! * **补屏**：有会话、本机却没有显示器在桌面上（显示器关了电源，线上的热插拔信号也断了，Windows 就把它移走了；
//!   或者根本没接显示器）时启用虚拟显示器。本机的显示器回到桌面上一会儿之后停用。补着屏的时候接上了新的显示器
//!   （例如重新打开了显示器的电源）也停用一下，让 Windows 把它放回桌面；先等一会儿（Windows 可能自己就放回去了，
//!   它还在处理新显示器的时候停用常常失败）。放不回去（例如合着笔记本的盖子，接着但不能用）就过一会儿再补，
//!   直到接着的显示器又有变化才再试。停用失败了过一会儿再试（[`Plan::disable_failed`]：只试一次的话虚拟显示器
//!   就一直留着，接回来的显示器用不上）。最后一个会话结束后再留一会儿（等断线重连）；
//! * **拔掉的显示器还在桌面上**：拔掉最后一个显示器时 Windows 可能不改显示设置（实测：合着盖的笔记本拔掉 HDMI，
//!   NVIDIA），那条路径照样在桌面上，画面都在它上面、再也不动了（见 `super::topology::monitor_present`）。
//!   它不算本机的显示器在桌面上（连着 [`STALE_SETTLE`] 都是这样才算）：补屏。虚拟显示器出来时 Windows 实测会自己
//!   把拔掉的撤下桌面；没撤的话撤下来、只留虚拟显示器（[`Step::Prune`]）。插回去之后和上面一样
//!   （接上了新的显示器：停用一下虚拟显示器，让 Windows 把它放回桌面）。

use std::time::{Duration, Instant};

/// 一个显示器接口：显卡的 LUID 和接口编号
pub(super) type Target = (i64, u32);

/// 现在的样子
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Facts {
    /// 虚拟显示器的设备启用着
    pub enabled: bool,
    /// Windows 认出了虚拟显示器的显示器（启用之后要一会儿）
    pub virtual_connected: bool,
    /// 虚拟显示器在桌面上
    pub virtual_active: bool,
    /// 有本机的显示器在桌面上（接着的，见 `physical_stale`）
    pub physical_active: bool,
    /// 有拔掉了的本机显示器还在桌面上（见模块说明）
    pub physical_stale: bool,
    /// 接着的本机显示器（不管在不在桌面上），排好序的
    pub physical_connected: Vec<Target>,
}

/// 想要什么
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Wants {
    /// 防窥模式
    pub privacy: bool,
    /// 有会话（或者最后一个会话刚结束不久）：要有显示器可看
    pub sessions: bool,
}

/// 下一步
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// 什么也不用做
    Idle,
    /// 等虚拟显示器出来
    Wait,
    /// 启用虚拟显示器
    Enable,
    /// 停用虚拟显示器
    Disable,
    /// 只留虚拟显示器在桌面上（防窥）
    Isolate,
    /// 按 Windows 记着的设置恢复本机的显示器（防窥结束）
    Restore,
    /// 补着屏：把拔掉了、还在桌面上的本机显示器撤下来，只留虚拟显示器（见模块说明）
    Prune,
    /// 停用虚拟显示器刚失败了：等一会儿再试（见 [`Plan::disable_failed`]）
    Backoff,
}

/// 本机的显示器回到桌面上这么久之后才停用补屏的虚拟显示器（Windows 一个一个放回去，别刚放回一个就停）
pub(super) const SETTLE: Duration = Duration::from_secs(2);
/// 补着屏时接上了新的显示器：停用虚拟显示器之后等这么久，Windows 还没把它放回桌面就再补屏
pub(super) const PROBE: Duration = Duration::from_secs(8);
/// 撤下拔掉的显示器没成（或者 Windows 又把它放回去了）：过这么久再试
pub(super) const PRUNE_RETRY: Duration = Duration::from_secs(5);
/// 桌面上的本机显示器看起来拔掉了这么久才算（插拔的那一下设备节点可能一闪而过：别为它把好好的显示器撤下来）
pub(super) const STALE_SETTLE: Duration = Duration::from_secs(2);
/// 停用虚拟显示器失败了：过这么久再试。显示器刚插上、Windows 还在处理时停用会失败（实测：等了 5 秒，
/// `ERROR_INVALID_DATA`），只试一次的话补屏的虚拟显示器就一直留着，接回来的显示器用不上
pub(super) const RETRY: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub(super) struct Plan {
    /// 撤下了本机的显示器（防窥），还没恢复。由执行的一方在撤下 / 恢复成功之后设置
    pub isolated: bool,
    /// 为了补屏启用的：那时接着哪些本机显示器
    headless: Option<Vec<Target>>,
    /// 让新接上的显示器回到桌面上：这之前不补屏
    probing: Option<Instant>,
    /// 本机的显示器回到桌面上的时刻
    physical_since: Option<Instant>,
    /// 上次撤下拔掉的显示器的时刻
    pruned_at: Option<Instant>,
    /// 桌面上的本机显示器看起来都拔掉了（`physical_stale`、没有 `physical_active`）的时刻
    stale_since: Option<Instant>,
    /// 补着屏时接上了新的显示器的时刻（等 [`SETTLE`] 再停用虚拟显示器：Windows 可能自己就把它放回桌面了）
    plugged_since: Option<Instant>,
    /// 为了让新接上的显示器回到桌面而停用时，`headless` 原来记着的（停用失败的话放回去，见 [`Plan::disable_failed`]）
    probe_seen: Option<Vec<Target>>,
    /// 停用失败了：这之前不再试
    retry_at: Option<Instant>,
}

impl Plan {
    /// `isolated`：上次撤下了本机的显示器、还没恢复（进程意外退出过，见 `super::device::was_isolated`）
    pub fn new(isolated: bool) -> Self {
        Self {
            isolated,
            ..Self::default()
        }
    }

    /// 上一步的停用失败了（由执行的一方告诉这里）：过 [`RETRY`] 再试。为了让新接上的显示器回到桌面才停用的话，
    /// 它接着算新接上的
    pub fn disable_failed(&mut self, now: Instant) {
        self.retry_at = Some(now + RETRY);
        if self.probing.take().is_some() {
            self.headless = self.probe_seen.take();
        }
    }

    /// 停用虚拟显示器（刚失败过的话先等着）
    fn disable(&self, now: Instant) -> Step {
        if self.retry_at.is_some_and(|t| now < t) {
            Step::Backoff
        } else {
            Step::Disable
        }
    }

    pub fn step(&mut self, wants: Wants, f: &Facts, now: Instant) -> Step {
        if f.physical_stale && !f.physical_active {
            let since = *self.stale_since.get_or_insert(now);
            // 刚看到拔掉：等它稳定下来（防窥、撤下过本机显示器的照常处理）
            if !wants.privacy
                && !self.isolated
                && now.saturating_duration_since(since) < STALE_SETTLE
            {
                return Step::Idle;
            }
        } else {
            self.stale_since = None;
        }
        if f.physical_active {
            self.physical_since.get_or_insert(now);
        } else {
            self.physical_since = None;
        }
        if !f.physical_stale {
            self.pruned_at = None;
        }
        if wants.privacy {
            self.headless = None;
            self.probing = None;
            if !f.enabled {
                return Step::Enable;
            }
            if !f.virtual_connected {
                return Step::Wait;
            }
            if !self.isolated || !f.virtual_active || f.physical_active {
                return Step::Isolate;
            }
            return Step::Idle;
        }
        if !f.enabled {
            self.retry_at = None;
            self.plugged_since = None;
            self.probe_seen = None;
        }
        if self.isolated {
            return if f.enabled {
                self.disable(now)
            } else {
                Step::Restore
            };
        }
        if !f.enabled {
            if let Some(until) = self.probing {
                if now < until && !f.physical_active {
                    return Step::Idle;
                }
                self.probing = None;
            }
            if wants.sessions && !f.physical_active {
                self.headless = Some(f.physical_connected.clone());
                return Step::Enable;
            }
            self.headless = None;
            return Step::Idle;
        }
        // 启用着（补屏，或者进程启动之前就启用着）
        if !wants.sessions {
            return self.disable(now);
        }
        if f.physical_active {
            let since = self.physical_since.unwrap_or(now);
            return if now.saturating_duration_since(since) >= SETTLE {
                self.disable(now)
            } else {
                Step::Idle
            };
        }
        if f.physical_stale && f.virtual_connected {
            let due = self
                .pruned_at
                .is_none_or(|t| now.saturating_duration_since(t) >= PRUNE_RETRY);
            if due {
                self.pruned_at = Some(now);
                return Step::Prune;
            }
        }
        match &self.headless {
            Some(seen) if f.physical_connected.iter().any(|t| !seen.contains(t)) => {
                // 接上了新的显示器：等一会儿（Windows 可能自己就把它放回桌面了，它还在处理的时候停用也常常失败），
                // 再停用虚拟显示器，让 Windows 把它放回桌面
                let since = *self.plugged_since.get_or_insert(now);
                if now.saturating_duration_since(since) < SETTLE {
                    return Step::Idle;
                }
                let step = self.disable(now);
                if step == Step::Disable {
                    self.probing = Some(now + PROBE);
                    self.probe_seen = self.headless.take();
                }
                step
            }
            Some(_) => {
                self.plugged_since = None;
                Step::Idle
            }
            None => {
                self.headless = Some(f.physical_connected.clone());
                Step::Idle
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: Target = (1, 7);

    fn facts(enabled: bool, virtual_active: bool, physical_active: bool) -> Facts {
        Facts {
            enabled,
            virtual_connected: enabled,
            virtual_active,
            physical_active,
            physical_stale: false,
            physical_connected: if physical_active {
                vec![MONITOR]
            } else {
                vec![]
            },
        }
    }

    const SESSION: Wants = Wants {
        privacy: false,
        sessions: true,
    };
    const PRIVACY: Wants = Wants {
        privacy: true,
        sessions: true,
    };
    const NOBODY: Wants = Wants {
        privacy: false,
        sessions: false,
    };

    /// 显示器开着：什么也不做；关了电源（从桌面上没了）：补一个，开回来一会儿之后停用
    #[test]
    fn fills_in_while_the_monitor_is_off() {
        let mut p = Plan::default();
        let t0 = Instant::now();
        assert_eq!(p.step(SESSION, &facts(false, false, true), t0), Step::Idle);
        assert_eq!(
            p.step(SESSION, &facts(false, false, false), t0),
            Step::Enable
        );
        // 启用了，还没出来；出来了
        let waiting = Facts {
            enabled: true,
            ..Facts::default()
        };
        assert_eq!(p.step(SESSION, &waiting, t0), Step::Idle);
        assert_eq!(p.step(SESSION, &facts(true, true, false), t0), Step::Idle);
        // 显示器开回来了，Windows 把它放回了桌面
        assert_eq!(p.step(SESSION, &facts(true, true, true), t0), Step::Idle);
        assert_eq!(
            p.step(SESSION, &facts(true, true, true), t0 + SETTLE),
            Step::Disable
        );
        assert_eq!(
            p.step(SESSION, &facts(false, false, true), t0 + SETTLE),
            Step::Idle
        );
        // 没有会话了：不补屏；补着的停用
        assert_eq!(p.step(NOBODY, &facts(false, false, false), t0), Step::Idle);
        assert_eq!(p.step(NOBODY, &facts(true, true, false), t0), Step::Disable);
    }

    /// 补着屏时接上了显示器、Windows 却没把它放回桌面（例如它的设置是"只在 2 上显示"，或者合着盖子）：
    /// 停用一下让它试试，放不回去就过一会儿再补，同一组显示器不再反复试
    #[test]
    fn a_new_monitor_gets_one_try() {
        let mut p = Plan::default();
        let t0 = Instant::now();
        assert_eq!(
            p.step(SESSION, &facts(false, false, false), t0),
            Step::Enable
        );
        assert_eq!(p.step(SESSION, &facts(true, true, false), t0), Step::Idle);
        let plugged = Facts {
            physical_connected: vec![MONITOR],
            ..facts(true, true, false)
        };
        // 先等一会儿：Windows 可能自己就把它放回桌面了
        assert_eq!(p.step(SESSION, &plugged, t0), Step::Idle);
        let t1 = t0 + SETTLE;
        assert_eq!(p.step(SESSION, &plugged, t1), Step::Disable);
        let disabled = Facts {
            physical_connected: vec![MONITOR],
            ..facts(false, false, false)
        };
        assert_eq!(p.step(SESSION, &disabled, t1), Step::Idle);
        assert_eq!(p.step(SESSION, &disabled, t1 + PROBE), Step::Enable);
        let enabled = Facts {
            physical_connected: vec![MONITOR],
            ..facts(true, true, false)
        };
        assert_eq!(p.step(SESSION, &enabled, t1 + PROBE), Step::Idle);
        // 显示器拔掉了：不用试
        assert_eq!(
            p.step(SESSION, &facts(true, true, false), t1 + PROBE),
            Step::Idle
        );
    }

    /// 停用失败了（显示器刚插上、Windows 还在处理）：过一会儿再试，接回来的显示器还算新接上的，不会就此一直补着屏；
    /// 本机的显示器回到桌面之后要停用时失败了也一样
    #[test]
    fn a_failed_disable_is_retried() {
        let mut p = Plan::default();
        let t0 = Instant::now();
        assert_eq!(
            p.step(SESSION, &facts(false, false, false), t0),
            Step::Enable
        );
        assert_eq!(p.step(SESSION, &facts(true, true, false), t0), Step::Idle);
        let plugged = Facts {
            physical_connected: vec![MONITOR],
            ..facts(true, true, false)
        };
        assert_eq!(p.step(SESSION, &plugged, t0), Step::Idle);
        let t1 = t0 + SETTLE;
        assert_eq!(p.step(SESSION, &plugged, t1), Step::Disable);
        p.disable_failed(t1);
        assert_eq!(p.step(SESSION, &plugged, t1), Step::Backoff);
        // 不用再等它稳定下来
        assert_eq!(p.step(SESSION, &plugged, t1 + RETRY), Step::Disable);
        p.disable_failed(t1 + RETRY);
        // Windows 自己把它放回了桌面：照常停用，刚失败过就先等着
        let back = Facts {
            physical_connected: vec![MONITOR],
            ..facts(true, true, true)
        };
        let t2 = t1 + RETRY;
        assert_eq!(p.step(SESSION, &back, t2), Step::Idle);
        assert_eq!(p.step(SESSION, &back, t2 + SETTLE), Step::Disable);
        p.disable_failed(t2 + SETTLE);
        assert_eq!(p.step(SESSION, &back, t2 + SETTLE), Step::Backoff);
        assert_eq!(p.step(SESSION, &back, t2 + SETTLE + RETRY), Step::Disable);
        // 停用了：之后接上的显示器照常
        let off = Facts {
            physical_connected: vec![MONITOR],
            ..facts(false, false, true)
        };
        assert_eq!(p.step(SESSION, &off, t2 + SETTLE + RETRY), Step::Idle);
    }

    /// 防窥：启用、等它出来、只留它；本机的显示器又回到桌面上就再撤；结束时先停用再恢复
    #[test]
    fn privacy_isolates_and_restores() {
        let mut p = Plan::default();
        let t0 = Instant::now();
        assert_eq!(
            p.step(PRIVACY, &facts(false, false, true), t0),
            Step::Enable
        );
        let waiting = Facts {
            enabled: true,
            virtual_connected: false,
            ..facts(true, false, true)
        };
        assert_eq!(p.step(PRIVACY, &waiting, t0), Step::Wait);
        // Windows 把它扩展到了桌面上（本机的也还在）
        assert_eq!(p.step(PRIVACY, &facts(true, true, true), t0), Step::Isolate);
        p.isolated = true;
        assert_eq!(p.step(PRIVACY, &facts(true, true, false), t0), Step::Idle);
        // 有人插拔了显示器，Windows 把它放回了桌面
        assert_eq!(p.step(PRIVACY, &facts(true, true, true), t0), Step::Isolate);
        // 结束：先停用，再恢复
        assert_eq!(p.step(NOBODY, &facts(true, true, false), t0), Step::Disable);
        assert_eq!(
            p.step(NOBODY, &facts(false, false, true), t0),
            Step::Restore
        );
        p.isolated = false;
        assert_eq!(p.step(NOBODY, &facts(false, false, true), t0), Step::Idle);
    }

    /// 拔掉了最后一个显示器，Windows 却没把它撤下桌面（路径还在、画面不再动）：补屏，虚拟显示器出来之后把拔掉的撤下来；
    /// 撤不下来过一会儿再试；插回去之后停用虚拟显示器，让 Windows 把它放回桌面
    #[test]
    fn an_unplugged_monitor_left_on_the_desktop() {
        let mut p = Plan::default();
        let t0 = Instant::now();
        let unplugged = |enabled: bool, virtual_active: bool| Facts {
            physical_stale: true,
            ..facts(enabled, virtual_active, false)
        };
        // 刚拔掉：先等一下，一直是拔掉的才补屏
        assert_eq!(p.step(SESSION, &unplugged(false, false), t0), Step::Idle);
        let t1 = t0 + STALE_SETTLE;
        assert_eq!(p.step(SESSION, &unplugged(false, false), t1), Step::Enable);
        // 启用了，还没出来：等
        let waiting = Facts {
            enabled: true,
            physical_stale: true,
            ..Facts::default()
        };
        assert_eq!(p.step(SESSION, &waiting, t1), Step::Idle);
        // 出来了（Windows 把它扩展到了拔掉的那个旁边）：撤下拔掉的
        assert_eq!(p.step(SESSION, &unplugged(true, true), t1), Step::Prune);
        // 没撤下来：过一会儿再试
        assert_eq!(p.step(SESSION, &unplugged(true, true), t1), Step::Idle);
        assert_eq!(
            p.step(SESSION, &unplugged(true, true), t1 + PRUNE_RETRY),
            Step::Prune
        );
        // 撤下来了：只剩虚拟显示器
        assert_eq!(p.step(SESSION, &facts(true, true, false), t1), Step::Idle);
        // 插回去了，Windows 没把它放回桌面：等一会儿，停用一下让它试试
        let plugged = Facts {
            physical_connected: vec![MONITOR],
            ..facts(true, true, false)
        };
        assert_eq!(p.step(SESSION, &plugged, t1), Step::Idle);
        assert_eq!(p.step(SESSION, &plugged, t1 + SETTLE), Step::Disable);
        // 只是一闪而过：不补屏
        let mut p = Plan::default();
        assert_eq!(p.step(SESSION, &unplugged(false, false), t0), Step::Idle);
        assert_eq!(p.step(SESSION, &facts(false, false, true), t0), Step::Idle);
        assert_eq!(p.step(SESSION, &unplugged(false, false), t1), Step::Idle);
        // 没有会话了：不撤
        let mut p = Plan::default();
        assert_eq!(p.step(NOBODY, &unplugged(true, true), t1), Step::Idle);
        assert_eq!(
            p.step(NOBODY, &unplugged(true, true), t1 + STALE_SETTLE),
            Step::Disable
        );
    }

    /// 补着屏的时候打开防窥：不用再启用，直接只留它
    #[test]
    fn privacy_while_filling_in() {
        let mut p = Plan::default();
        let t0 = Instant::now();
        assert_eq!(
            p.step(SESSION, &facts(false, false, false), t0),
            Step::Enable
        );
        assert_eq!(
            p.step(PRIVACY, &facts(true, true, false), t0),
            Step::Isolate
        );
        p.isolated = true;
        assert_eq!(p.step(PRIVACY, &facts(true, true, false), t0), Step::Idle);
        // 关掉防窥：显示器还关着、会话还在，恢复之后又会补屏
        assert_eq!(
            p.step(SESSION, &facts(true, true, false), t0),
            Step::Disable
        );
        assert_eq!(
            p.step(SESSION, &facts(false, false, false), t0),
            Step::Restore
        );
        p.isolated = false;
        assert_eq!(
            p.step(SESSION, &facts(false, false, false), t0),
            Step::Enable
        );
    }
}
