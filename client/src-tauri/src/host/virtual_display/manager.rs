//! 管理虚拟显示器的线程（Windows）：每一轮看一眼现在的样子，按 [`Plan`] 走一步（见 `super::plan`）。
//!
//! 有会话的时候第一个会话加入时启动，进程退出时（[`super::shutdown`]）恢复原样再结束。改显示设置、启用停用设备
//! 都要几百毫秒到几秒，放在自己的线程上，不挡会话的任务；改完了告诉会话（`Hub::displays`），
//! 会话马上重新列出显示器，不等定时检查。

use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::block::Blocker;
use super::plan::{Plan, Step, Wants};
use super::{HUB, Mode, device, fail, topology};
use crate::host::desktop;

/// 有事的时候多久看一次（启用之后等它出来、改完设置之后看结果）
const BUSY: Duration = Duration::from_millis(500);
/// 有会话的时候多久看一次（发现显示器关了电源）
const WATCH: Duration = Duration::from_secs(1);
/// 没事的时候多久看一次（有会话加入、开关防窥都会马上叫醒它）
const IDLE: Duration = Duration::from_secs(30);
/// 启用之后这么久虚拟显示器还没出来，防窥就算失败
const APPEAR: Duration = Duration::from_secs(15);
/// 查不到显示设置时，先这么多次每 [`BUSY`] 再看一次，之后每 5 秒
const LOOK_TRIES: u32 = 10;
/// 连着这么多轮都要"只留虚拟显示器"（改不成，或者改完了又被放回去），防窥就算失败
const MAX_FAILURES: u32 = 4;

pub(super) fn spawn() {
    if let Err(e) = std::thread::Builder::new()
        .name("airhand-virtual-display".into())
        .spawn(run)
    {
        warn!("can't start the virtual display manager: {e}");
        let mut w = HUB.wants.lock().unwrap();
        w.running = false;
    }
}

fn bump() {
    HUB.displays.send_modify(|g| *g = g.wrapping_add(1));
}

fn run() {
    info!("virtual display manager started");
    let mut plan = Plan::new(device::was_isolated());
    let mut blocker: Option<Blocker> = None;
    let mut enabled_at: Option<Instant> = None;
    let mut failures = 0u32;
    // 连着几次查不到显示设置了
    let mut look_failures = 0u32;
    // 最近看到的本机主显示器的分辨率：虚拟显示器用一样的
    let mut mode = Mode::default();
    // 这次启用之后已经把虚拟显示器换成了 `mode`
    let mut mode_set = false;
    loop {
        let now = Instant::now();
        let (wants, stopping) = {
            let w = HUB.wants.lock().unwrap();
            let wants = Wants {
                privacy: w.privacy(now),
                sessions: w.sessions(now),
            };
            (wants, w.stopping)
        };
        // 要退出了：什么都不要了，恢复原样
        let wants = if stopping { Wants::default() } else { wants };
        if wants.privacy && blocker.is_none() {
            blocker = Some(Blocker::start());
        } else if !wants.privacy {
            blocker = None;
        }
        desktop::follow();
        let facts = device::enabled().and_then(topology::facts);
        let facts = match facts {
            Ok(f) => {
                look_failures = 0;
                f
            }
            Err(e) => {
                look_failures += 1;
                if look_failures == 1 || look_failures.is_multiple_of(LOOK_TRIES) {
                    warn!("can't look at the displays: {e:#}");
                }
                // 设备没了（例如刚在卸载时删掉了）：不再管了，下一个会话加入时重新看能不能用
                if stopping || !device::usable() {
                    break;
                }
                // 插拔显示器的那一下常常查不到（实测拔掉 HDMI 时 `ERROR_GEN_FAILURE`），马上就好：很快再看，
                // 不然补屏要晚好几秒。一直查不到就慢下来
                wait(if look_failures < LOOK_TRIES {
                    BUSY
                } else {
                    Duration::from_secs(5)
                });
                continue;
            }
        };
        if facts.physical_active
            && !plan.isolated
            && let Some(m) = topology::primary_mode()
        {
            mode = m;
        }
        let step = plan.step(wants, &facts, now);
        let mut busy = true;
        match step {
            Step::Idle => {
                busy = false;
                failures = 0;
                if facts.virtual_active && !mode_set {
                    match topology::apply_mode(mode) {
                        Ok(true) => mode_set = true,
                        Ok(false) => {}
                        Err(e) => {
                            info!("{e:#}");
                            mode_set = true;
                        }
                    }
                }
            }
            Step::Wait => {
                // 不是这里启用的（进程启动之前就启用着）：从现在算起
                let since = *enabled_at.get_or_insert(now);
                if wants.privacy && now.duration_since(since) > APPEAR {
                    fail("the virtual display didn't show up".into());
                }
            }
            Step::Enable => {
                info!(?mode, "enabling the virtual display");
                if let Err(e) = device::write_settings(mode) {
                    warn!("{e:#}");
                }
                match device::set_enabled(true) {
                    Ok(()) => {
                        enabled_at = Some(now);
                        mode_set = false;
                        bump();
                    }
                    Err(e) => {
                        warn!("{e:#}");
                        if wants.privacy {
                            fail(crate::host::capture::wire_detail(&e));
                        }
                    }
                }
            }
            Step::Disable => {
                info!("disabling the virtual display");
                match device::set_enabled(false) {
                    Ok(()) => {
                        enabled_at = None;
                        bump();
                    }
                    Err(e) => {
                        warn!("{e:#}, trying again in a moment");
                        plan.disable_failed(Instant::now());
                    }
                }
            }
            // 停用刚失败过：等着再试（要退出了也等：不然虚拟显示器就留在桌面上了）
            Step::Backoff => {}
            Step::Isolate => {
                info!("privacy mode: only the virtual display stays on the desktop");
                // 先记下来：改到一半进程没了，下次启动按它恢复
                device::set_isolated(true);
                // 连着好几次都没改成（报错，或者改完了 Windows 又把本机的显示器放回了桌面）：算失败
                failures += 1;
                let r = topology::isolate();
                if r.is_ok() {
                    plan.isolated = true;
                    mode_set = false;
                    bump();
                }
                if let Err(e) = &r {
                    warn!("{e:#}");
                }
                // 改成了的这一次先不算：下一轮看它留没留住
                let give_up = match &r {
                    Err(_) => failures >= MAX_FAILURES,
                    Ok(()) => failures > MAX_FAILURES,
                };
                if give_up {
                    fail(match r {
                        Err(e) => crate::host::capture::wire_detail(&e),
                        Ok(()) => "Windows keeps putting the local displays back".into(),
                    });
                }
            }
            Step::Prune => {
                // 不记进注册表（不是防窥）：进程意外退出之后停用虚拟显示器就够了，Windows 记着的设置没动过
                info!(
                    "the local display was unplugged but is still on the desktop, leaving only the virtual display"
                );
                match topology::isolate() {
                    Ok(()) => {
                        mode_set = false;
                        bump();
                    }
                    Err(e) => warn!("{e:#}"),
                }
            }
            Step::Restore => {
                info!("restoring the local displays");
                if let Err(e) = topology::restore() {
                    // 没关系：停用虚拟显示器的时候 Windows 已经把本机的显示器放回去了
                    info!("{e:#}");
                }
                plan.isolated = false;
                device::set_isolated(false);
                bump();
            }
        }
        // 只留着虚拟显示器、本机的显示器都撤下了，才算开着
        {
            let mut w = HUB.wants.lock().unwrap();
            let on = plan.isolated && step == Step::Idle && w.privacy(now);
            if w.on != on {
                w.on = on;
                if on {
                    info!("privacy mode is on");
                }
            }
            super::publish(&w);
            if stopping && step == Step::Idle {
                break;
            }
        }
        let period = if busy {
            BUSY
        } else if wants.privacy || wants.sessions || facts.enabled {
            WATCH
        } else {
            IDLE
        };
        wait(period);
    }
    drop(blocker);
    let mut w = HUB.wants.lock().unwrap();
    w.running = false;
    w.on = false;
    w.usable = device::usable();
    super::publish(&w);
    HUB.wake.notify_all();
    info!("virtual display manager stopped");
}

/// 等 `period`，有人叫醒（会话加入、离开，开关防窥，要退出了）就提前回来
fn wait(period: Duration) {
    let mut w = HUB.wants.lock().unwrap();
    if w.stopping {
        // 要退出了：只差恢复原样那几步，不等太久（出了错也不空转）
        drop(w);
        std::thread::sleep(Duration::from_millis(200));
        return;
    }
    if !w.poked {
        w = HUB
            .wake
            .wait_timeout_while(w, period, |w| !w.poked && !w.stopping)
            .unwrap()
            .0;
    }
    w.poked = false;
}

/// 进程启动时、卸载时：上次没恢复的恢复掉（停用虚拟显示器；撤下过本机的显示器的话按 Windows 记着的设置恢复）
pub(super) fn recover() {
    desktop::follow();
    match device::enabled() {
        Ok(true) => {
            info!("the virtual display was left enabled, disabling it");
            if let Err(e) = device::set_enabled(false) {
                warn!("{e:#}");
            }
        }
        Ok(false) => {}
        Err(e) => warn!("{e:#}"),
    }
    if device::was_isolated() {
        info!("the local displays were left off by privacy mode, restoring them");
        if let Err(e) = topology::restore() {
            warn!("{e:#}");
        }
        device::set_isolated(false);
    }
}
