//! 远程控制结束之后：退出防窥模式、锁屏。
//!
//! 会话确认了握手才算数（见 `airhand_link::host::Platform::confirmed`：重放的握手消息不算一次远程控制）。
//! 一个会话怎么结束的决定远程控制什么时候算结束：
//!
//! * **正常结束**（控制端断开：断开按钮、关掉标签页 / 窗口；本机断开所有控制端、更换控制码）：马上算；
//! * **断线**（失联、连接出错，被控端和服务器的连接断了，被控端的任务被中止）：控制端多半在重连，
//!   再等 [`RECONNECT`]，没人回来才算。期间又断了一个就从那时重新等；正常结束了一个就不等了；
//! * **本机有人要用电脑**：防窥挡着本机的键盘鼠标时有人按了键、点了鼠标（[`local_input`]）。断线了在等的不再等，
//!   马上算；还连着的不管，不过之后 [`PRESENT`] 里断线了也不等。控制端的应用被杀掉（手机上划掉后台）时
//!   发不出断开，中转要过 10 秒左右才发现它不在了，再等重连的话本机的人要对着黑屏等快一分钟。
//!   远程控制结束时照样先锁屏，本机的人看到的只是锁屏界面；控制端重连上来时防窥照样打开（握手里带着）。
//!
//! 结束的时候：
//!
//! * 还有别的会话（还有人在控制）：不锁屏。断开的这个打开了防窥模式（别人都没开）的话退出防窥
//!   （[`virtual_display::release_privacy`]；还有断线的在等重连时等它们）；
//! * 一个会话都没有了：**先锁屏**（托盘菜单里的"结束远程后锁屏"，默认开，见 [`lock_on_end`]），等锁屏界面出来，
//!   **再退出防窥模式**。
//!   反过来的话，本机的显示器恢复之后、锁屏之前，那一下本机的桌面就露出来了。
//!
//! 被控端停下来（退出、升级、卸载、关机）时还有远程控制（连着的、断线了在等的）也算结束：锁屏，
//! 之后 `virtual_display::shutdown` 恢复本机的显示器（[`shutdown`]）。
//!
//! 锁屏：Windows 是 `LockWorkStation`（系统服务里的被控端以 SYSTEM 身份运行在用户的会话里，也锁得了；
//! 没人登录、已经锁着时什么也不做）；macOS 是"锁定屏幕"（`SACLockScreenImmediate`，等会话信息里说锁上了；
//! 已经锁着时什么也不做）；Linux 是 `loginctl lock-session`，不行再试 `xdg-screensaver lock`。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tracing::info;

use super::virtual_display;

/// 断线的控制端：等它重连这么久
pub const RECONNECT: Duration = Duration::from_secs(30);
/// 本机的人按了键、点了鼠标（[`local_input`]）之后这么久里断线的控制端不等它重连：控制端的应用被杀掉时，
/// 中转大约 10 秒后（QUIC 的空闲超时）才发现它不在了，这之前按的也算
const PRESENT: Duration = Duration::from_secs(30);

/// 远程控制在不在、断线的在不在等（见模块说明）。纯逻辑，执行在 [`run`]
#[derive(Debug, Default)]
struct State {
    /// 确认了握手、还没结束的会话
    live: HashSet<u64>,
    /// 有断线的会话在等重连
    waiting: bool,
    /// 每次开始等、不再等都加一：过时的等待到期时什么也不做
    epoch: u64,
    /// 被控端要停下来了（[`shutdown`] 之后什么也不做）
    stopping: bool,
    /// 本机的人上一次按键、点鼠标（防窥挡着的时候，见 [`local_input`]）；用过一次就清掉
    local: Option<Instant>,
}

/// 要做什么
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Nothing,
    /// 等 [`RECONNECT`]，然后 [`State::expire`] 这一轮
    Wait(u64),
    /// 还有别的会话：只退出防窥（打开它的都走了的话）
    Release,
    /// 远程控制结束了：锁屏，再退出防窥
    End,
}

impl State {
    fn join(&mut self, session_id: u64) {
        self.live.insert(session_id);
    }

    /// 会话结束了：`clean` 是正常结束（见模块说明）
    fn leave(&mut self, session_id: u64, clean: bool) -> Action {
        self.leave_at(session_id, clean, Instant::now())
    }

    fn leave_at(&mut self, session_id: u64, clean: bool, now: Instant) -> Action {
        if !self.live.remove(&session_id) || self.stopping {
            return Action::Nothing;
        }
        if !clean {
            if self.someone_here(now) {
                info!(
                    session_id,
                    "someone at the computer wants to use it, not waiting for the controller"
                );
                return self.stop_waiting();
            }
            self.epoch += 1;
            self.waiting = true;
            return Action::Wait(self.epoch);
        }
        if !self.live.is_empty() {
            // 断线的还在等：到时候一起
            return if self.waiting {
                Action::Nothing
            } else {
                Action::Release
            };
        }
        // 最后一个正常结束了：不用再等断线的了
        self.epoch += 1;
        self.waiting = false;
        self.local = None;
        Action::End
    }

    /// 第 `epoch` 轮等待到期了
    fn expire(&mut self, epoch: u64) -> Action {
        if epoch != self.epoch || !self.waiting || self.stopping {
            return Action::Nothing;
        }
        self.waiting = false;
        if self.live.is_empty() {
            self.local = None;
            Action::End
        } else {
            Action::Release
        }
    }

    /// 防窥挡住了本机的人按键、点鼠标（见 [`local_input`]）
    fn local_input(&mut self, now: Instant) -> Action {
        if self.stopping {
            return Action::Nothing;
        }
        if self.waiting {
            return self.stop_waiting();
        }
        self.local = Some(now);
        Action::Nothing
    }

    /// 本机的人刚按过键、点过鼠标（见 [`PRESENT`]）
    fn someone_here(&self, now: Instant) -> bool {
        self.local
            .is_some_and(|t| now.saturating_duration_since(t) < PRESENT)
    }

    /// 不再等断线的控制端重连：一个会话都不剩了就结束，不然只退出防窥（和等待到期一样）
    fn stop_waiting(&mut self) -> Action {
        self.epoch += 1;
        self.waiting = false;
        self.local = None;
        if self.live.is_empty() {
            Action::End
        } else {
            Action::Release
        }
    }

    /// 被控端要停下来了：还有远程控制的话要锁屏
    fn stop(&mut self) -> bool {
        if self.stopping {
            return false;
        }
        self.stopping = true;
        self.epoch += 1;
        !self.live.is_empty() || self.waiting
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap()
}

/// 一个会话（确认了握手的）：[`Slot`] 里放着它，会话结束时按结果交回（丢掉了算断线）
struct Presence {
    session_id: u64,
    ended: bool,
}

impl Presence {
    fn end(mut self, clean: bool) {
        self.ended = true;
        leave(self.session_id, clean);
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        if !self.ended {
            leave(self.session_id, false);
        }
    }
}

/// 会话和启动它的地方共用的一格：会话确认了握手时放进去（[`Slot::confirmed`]），
/// 结束后按结果取出来（[`Slot::end`]）。任务被中止、都丢掉了也算结束（断线）
#[derive(Clone, Default)]
pub struct Slot(Arc<Mutex<Option<Presence>>>);

impl Slot {
    /// 控制端确认了握手（重复调用没关系）
    pub fn confirmed(&self, session_id: u64) {
        let mut p = self.0.lock().unwrap();
        if p.is_none() {
            state().join(session_id);
            *p = Some(Presence {
                session_id,
                ended: false,
            });
        }
    }

    /// 会话结束了：`clean` 是正常结束（控制端断开、本机断开了它），不是的话等它重连（见模块说明）
    pub fn end(&self, clean: bool) {
        let p = self.0.lock().unwrap().take();
        if let Some(p) = p {
            p.end(clean);
        }
    }
}

fn leave(session_id: u64, clean: bool) {
    let action = state().leave(session_id, clean);
    run(action);
}

/// 防窥模式挡住了本机的人按键、点鼠标、滚滚轮（Windows 的低级钩子、macOS 的事件监听在自己的线程上调用，
/// 见 `virtual_display`）：本机有人要用电脑。断线了在等重连的控制端不再等：一个会话都不剩了就锁屏、
/// 退出防窥；还连着的控制端不管（见模块说明）
pub fn local_input() {
    let action = state().local_input(Instant::now());
    if action != Action::Nothing {
        info!(
            "someone at the computer wants to use it, not waiting for the controller to reconnect"
        );
        // 挡输入的线程要马上回到它的消息循环 / run loop：退出防窥要拿别的锁，放在自己的线程上
        spawn(move || run(action));
    }
}

/// 做 `action`：要等的、要锁屏的（锁屏之后要等锁屏界面出来）放在自己的线程上，不挡会话的任务
fn run(action: Action) {
    match action {
        Action::Nothing => {}
        Action::Release => virtual_display::release_privacy(),
        Action::Wait(epoch) => spawn(move || {
            std::thread::sleep(RECONNECT);
            let action = state().expire(epoch);
            run(action);
        }),
        Action::End => spawn(|| {
            info!("the remote control ended");
            lock();
            virtual_display::release_privacy();
        }),
    }
}

fn spawn(f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new()
        .name("airhand-session-end".into())
        .spawn(f)
    {
        tracing::warn!("can't handle the end of the remote control: {e}");
    }
}

/// 结束远程后锁屏：启动时按 config.toml 的 `lock_on_end`，托盘菜单里切换的马上生效（[`set_lock_on_end`]）
static LOCK_ON_END: LazyLock<AtomicBool> =
    LazyLock::new(|| AtomicBool::new(crate::config::settings().lock_on_end));

/// 远程控制结束后锁不锁屏
pub fn lock_on_end() -> bool {
    LOCK_ON_END.load(Ordering::Relaxed)
}

/// 改成锁 / 不锁（保存进 config.toml 的是调用的一方，见 `host::set_lock_on_end`）。下一次结束时就按它
pub fn set_lock_on_end(on: bool) {
    LOCK_ON_END.store(on, Ordering::Relaxed);
}

/// 设置允许的话锁屏（见模块说明）
fn lock() {
    if !lock_on_end() {
        return;
    }
    // 跑测试的电脑不锁
    if cfg!(test) {
        return;
    }
    info!("locking the screen");
    imp::lock();
}

/// 被控端要停下来了（在 `virtual_display::shutdown` 之前调用）：还有远程控制的话锁屏（见模块说明）
pub fn shutdown() {
    if state().stop() {
        info!("stopping with a remote control going on");
        lock();
    }
}

#[cfg(windows)]
mod imp {
    use std::time::{Duration, Instant};

    use tracing::info;
    use windows::Win32::System::Shutdown::LockWorkStation;

    /// 锁屏之后最多等锁屏界面这么久（再退出防窥）
    const SHOWN: Duration = Duration::from_secs(2);

    pub fn lock() {
        // 没人登录（登录界面）、已经锁着时会失败：本来就看不到桌面
        if let Err(e) = unsafe { LockWorkStation() } {
            info!("can't lock the screen: {e}");
            return;
        }
        // 它只是发起锁屏（异步的）：等输入桌面换成安全桌面
        let deadline = Instant::now() + SHOWN;
        while !crate::host::desktop::input_is_secure() {
            if Instant::now() >= deadline {
                info!("the lock screen didn't show up in time");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::time::{Duration, Instant};

    use tracing::{info, warn};

    use crate::host::macos;

    /// 锁屏之后最多等锁上这么久（再退出防窥）
    const SHOWN: Duration = Duration::from_secs(2);
    /// 会话信息里说锁上了之后再等这么久，让锁屏界面画出来（没有量过要多久，宁可多等一会儿：防窥晚一点退出）
    const DRAWN: Duration = Duration::from_millis(500);

    /// 私有框架 login.framework 的 `SACLockScreenImmediate`：和菜单里的"锁定屏幕"一样，不要什么权限。
    /// 它只是发起锁屏：等会话信息里说锁上了（[`macos::screen_locked`]）
    pub fn lock() {
        if macos::screen_locked() == Some(true) {
            return;
        }
        if !request() {
            return;
        }
        let deadline = Instant::now() + SHOWN;
        while macos::screen_locked() != Some(true) {
            if Instant::now() >= deadline {
                info!("the lock screen didn't show up in time");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(DRAWN);
    }

    /// 发起锁屏，成没成
    fn request() -> bool {
        unsafe {
            let lib = libc::dlopen(
                c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login"
                    .as_ptr(),
                libc::RTLD_LAZY,
            );
            if lib.is_null() {
                warn!("can't lock the screen: login.framework isn't there");
                return false;
            }
            let f = libc::dlsym(lib, c"SACLockScreenImmediate".as_ptr());
            if f.is_null() {
                warn!("can't lock the screen: SACLockScreenImmediate isn't there");
                return false;
            }
            let f = std::mem::transmute::<*mut libc::c_void, extern "C" fn() -> i32>(f);
            let r = f();
            if r != 0 {
                warn!("can't lock the screen: SACLockScreenImmediate returned {r}");
            }
            r == 0
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use std::process::{Command, Stdio};

    use tracing::warn;

    /// 先请 logind 锁这个会话（桌面环境的锁屏程序响应它），不行（不在 logind 的会话里、没有 loginctl）再试 xdg-screensaver
    pub fn lock() {
        let ok = |program: &str, args: &[&str]| {
            Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !ok("loginctl", &["lock-session"]) && !ok("xdg-screensaver", &["lock"]) {
            warn!(
                "can't lock the screen: neither loginctl lock-session nor xdg-screensaver lock worked"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 控制端断开了：马上结束；不是确认了握手的会话不算
    #[test]
    fn a_clean_end_ends_now() {
        let mut s = State::default();
        assert_eq!(s.leave(1, true), Action::Nothing);
        s.join(1);
        assert_eq!(s.leave(1, true), Action::End);
        assert_eq!(s.leave(1, true), Action::Nothing);
    }

    /// 断线了：等它重连，没回来才结束；过时的等待什么也不做
    #[test]
    fn a_dropped_session_gets_time_to_come_back() {
        let mut s = State::default();
        s.join(1);
        let Action::Wait(first) = s.leave(1, false) else {
            panic!()
        };
        // 回来了，又断了：从这时重新等
        s.join(2);
        let Action::Wait(second) = s.leave(2, false) else {
            panic!()
        };
        assert_eq!(s.expire(first), Action::Nothing);
        assert_eq!(s.expire(second), Action::End);
        assert_eq!(s.expire(second), Action::Nothing);
    }

    /// 重连上了：等待到期时还有人在控制，不锁屏（只退出防窥，它要的话重连时又打开了）；它正常断开时才结束
    #[test]
    fn a_reconnected_session_keeps_it_going() {
        let mut s = State::default();
        s.join(1);
        let Action::Wait(e) = s.leave(1, false) else {
            panic!()
        };
        s.join(2);
        assert_eq!(s.expire(e), Action::Release);
        assert_eq!(s.leave(2, true), Action::End);
    }

    /// 几个控制端：走了一个只退出防窥，最后一个走了才结束
    #[test]
    fn the_last_one_ends_it() {
        let mut s = State::default();
        s.join(1);
        s.join(2);
        assert_eq!(s.leave(1, true), Action::Release);
        assert_eq!(s.leave(2, true), Action::End);
    }

    /// 有断线的在等：别人正常断开时不退出防窥（等断线的），最后一个正常断开了就不等了
    #[test]
    fn a_clean_end_while_one_is_waiting() {
        let mut s = State::default();
        s.join(1);
        s.join(2);
        s.join(3);
        let Action::Wait(e) = s.leave(1, false) else {
            panic!()
        };
        assert_eq!(s.leave(2, true), Action::Nothing);
        assert_eq!(s.leave(3, true), Action::End);
        assert_eq!(s.expire(e), Action::Nothing);
    }

    /// 停下来时还有远程控制（连着的、在等的）就锁屏；之后什么也不做
    #[test]
    fn stopping_locks_if_someone_was_in_control() {
        let mut s = State::default();
        assert!(!s.stop());

        let mut s = State::default();
        s.join(1);
        assert!(s.stop());
        assert!(!s.stop());
        assert_eq!(s.leave(1, false), Action::Nothing);

        let mut s = State::default();
        s.join(1);
        let Action::Wait(e) = s.leave(1, false) else {
            panic!()
        };
        assert!(s.stop());
        assert_eq!(s.expire(e), Action::Nothing);
    }

    /// 断线了在等重连时本机有人按键：不等了，马上结束；那一轮等待到期时什么也不做
    #[test]
    fn someone_at_the_computer_stops_the_wait() {
        let t0 = Instant::now();
        let mut s = State::default();
        s.join(1);
        let Action::Wait(e) = s.leave_at(1, false, t0) else {
            panic!()
        };
        assert_eq!(s.local_input(t0), Action::End);
        assert_eq!(s.expire(e), Action::Nothing);
        // 用过了：下一次断线照样等
        s.join(2);
        assert!(matches!(s.leave_at(2, false, t0), Action::Wait(_)));
    }

    /// 还连着时按的键不打断它；之后不久它断线了（控制端的应用被杀掉，中转过一会儿才发现）就不等它重连
    #[test]
    fn a_recent_press_skips_the_wait() {
        let t0 = Instant::now();
        let mut s = State::default();
        s.join(1);
        assert_eq!(s.local_input(t0), Action::Nothing);
        assert_eq!(
            s.leave_at(1, false, t0 + Duration::from_secs(10)),
            Action::End
        );

        // 很久以前按的不算
        let mut s = State::default();
        s.join(1);
        s.local_input(t0);
        assert!(matches!(
            s.leave_at(1, false, t0 + PRESENT),
            Action::Wait(_)
        ));
    }

    /// 还有别的控制端连着：本机有人按键也不锁屏，只是不等断线的重连（它打开的防窥退出）；没在等的时候什么也不做
    #[test]
    fn others_still_connected() {
        let t0 = Instant::now();
        let mut s = State::default();
        s.join(1);
        s.join(2);
        let Action::Wait(e) = s.leave_at(1, false, t0) else {
            panic!()
        };
        assert_eq!(s.local_input(t0), Action::Release);
        assert_eq!(s.expire(e), Action::Nothing);
        assert_eq!(s.local_input(t0), Action::Nothing);
        assert_eq!(s.leave_at(2, true, t0), Action::End);
    }
}
