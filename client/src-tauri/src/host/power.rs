//! 电源：有控制端连着的时候不熄屏、不睡眠（Windows、macOS），以及 macOS 上的"不自动睡眠"。
//!
//! 熄了屏就没有画面：Windows 的桌面不再刷新，桌面复制拿不到新的画面，控制端只看不动的时候过几分钟画面就停了，
//! 动一下鼠标才恢复；macOS 的 ScreenCaptureKit 采集不了睡着的显示器。所以第一个会话开始时要电源请求、
//! 最后一个会话结束时放开（[`Awake`] 按会话计数）。会话开始时屏幕已经熄了的话还叫醒它一次（[`wake_display`]），
//! 不然第一帧画面要等控制端动了鼠标才来（macOS 上一直等不来）；Windows 的防窥模式下不叫（本机的显示器马上要撤下来，
//! 不能先亮一下）。macOS 的防窥模式采集的还是本机的显示器（只是调黑了，见 `virtual_display`），照样叫。
//!
//! ## Windows
//!
//! 支持现代待机（S0 低功耗空闲）的电脑熄屏就等于待机，进程被暂停、网络断开，会话也就断了。有会话的时候
//! 向系统要两个电源请求（`PowerSetRequest`）：
//!
//! * `PowerRequestDisplayRequired`：不因为没人碰键盘鼠标而关闭显示器；
//! * `PowerRequestSystemRequired`：不因为空闲而睡眠（微软的文档：要保持屏幕亮着，两个都要）。
//!
//! 管不了的：合上盖子、按电源键、开始菜单里选"睡眠"这些用户自己让电脑睡眠的，所有电源请求都会被系统终止；
//! 用电池的现代待机电脑上，系统还会在睡眠超时 5 分钟后终止这些请求。被控端以 SYSTEM 身份在控制台会话里运行，
//! 锁屏不影响它的请求。
//!
//! ## macOS
//!
//! 有会话时要三个电源断言（IOKit 的 `IOPMAssertionCreateWithName`）：`PreventUserIdleDisplaySleep`（不因为没人碰
//! 键盘鼠标而熄屏；设置了"显示器关闭后要求密码"时也就不会因此锁屏）、`PreventUserIdleSystemSleep`，以及
//! `PreventSystemSleep`（头文件说它过时了，但 WindowServer、`caffeinate -s` 都还在用）：接着外接电源时合上笔记本的
//! 盖子也不睡眠，远程控制接着进行（用电池时系统不认它，照常睡眠）。它要在合盖之前就要着：合上的那一刻系统就开始
//! 睡眠了，等补屏的虚拟显示器出来再要来不及。
//! 叫醒显示器用 `IOPMAssertionDeclareUserActivity`（和 `caffeinate -u` 一样，当作本机有人碰了一下）：会话开始时，
//! 以及采集线程发现显示器睡着了的时候（锁屏之后常见，见 `capture::sck`）。亮着的锁屏界面照常采集、照常接收
//! 注入的键鼠，控制端可以输入密码解锁。合着笔记本的盖子、除了内建的显示器又没有别的显示器时不叫：叫醒的只是
//! 盖子后面的屏幕，系统给它 10 秒（WindowServer 的 `DMGrace`）就又关掉，画面跟着断一次，每 10 秒循环一回；
//! 合盖之后由虚拟显示器顶上（见 `virtual_display`）。
//!
//! **有虚拟显示器期间**（[`keep_running`]）：也要着 `PreventSystemSleep`，最后一个会话结束后等重连的那一会儿
//! 合着盖子也不睡眠。
//!
//! **不自动睡眠**（托盘菜单，默认关，`config.toml` 的 `prevent_sleep`）：电脑睡眠时进程被冻结，和服务器的连接断开，
//! 控制端只能看到它离线。打开后被控端运行期间（不只是有会话时，见 [`Hosting`]）一直要着 `PreventUserIdleSystemSleep`
//! 和 `PreventSystemSleep`：电脑不因为空闲而睡眠，合上笔记本的盖子也不睡，一直在线等着被连接；显示器照常熄灭
//! （连上来时叫醒它）。合上盖子之后连上来也要有画面：虚拟显示器得在合盖之前就建好，所以这时没有会话也预备着
//! （`virtual_display::set_ready`）。只在接着外接电源时要（每 [`POWER_CHECK`] 看一次电源，接上、拔掉电源时
//! 跟着变），用电池时照常睡眠。
//!
//! 管不了的：用电池时合上笔记本的盖子、没有会话又没开"不自动睡眠"时合上盖子、苹果菜单里选"睡眠"、
//! 电量低，断言都挡不住。
//! 这些都不需要什么权限，`pmset -g assertions` 里能看到它们（名字以 Airhand 开头）。

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::time::Duration;

/// 有会话期间保持亮屏、不睡眠。所有的都丢掉了才放开
pub struct Awake(());

/// 被控端在运行（`HostService` 拿着它）：打开了"不自动睡眠"时电脑不因为空闲而睡眠（macOS，见模块说明）。
/// 所有的都丢掉了才放开
pub struct Hosting(());

/// "不自动睡眠"开着时多久看一次用的是不是外接电源
#[cfg(target_os = "macos")]
const POWER_CHECK: Duration = Duration::from_secs(30);

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;

    use tracing::{info, warn};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Power::{
        ES_DISPLAY_REQUIRED, PowerClearRequest, PowerCreateRequest, PowerRequestDisplayRequired,
        PowerRequestSystemRequired, PowerSetRequest, SetThreadExecutionState,
    };
    use windows::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };
    use windows::core::{PWSTR, w};

    /// 有几个 [`super::Awake`]，电源请求的句柄（`HANDLE` 不能跨线程，存它的值）
    static STATE: Mutex<(usize, usize)> = Mutex::new((0, 0));

    const REQUESTS: [windows::Win32::System::Power::POWER_REQUEST_TYPE; 2] =
        [PowerRequestSystemRequired, PowerRequestDisplayRequired];

    pub fn acquire() {
        let mut st = STATE.lock().unwrap();
        st.0 += 1;
        if st.0 > 1 {
            return;
        }
        let reason = REASON_CONTEXT {
            // POWER_REQUEST_CONTEXT_VERSION
            Version: 0,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                // `powercfg /requests` 里显示的原因。系统会复制一份
                SimpleReasonString: PWSTR(
                    w!("Airhand: a remote session is active").as_ptr() as *mut u16
                ),
            },
        };
        match unsafe { PowerCreateRequest(&reason) } {
            Ok(h) => {
                for r in REQUESTS {
                    if let Err(e) = unsafe { PowerSetRequest(h, r) } {
                        warn!("can't keep the computer awake ({r:?}): {e}");
                    }
                }
                st.1 = h.0 as usize;
                info!("keeping the display on while controllers are connected");
            }
            Err(e) => warn!("can't create a power request: {e}"),
        }
    }

    pub fn release() {
        let mut st = STATE.lock().unwrap();
        st.0 = st.0.saturating_sub(1);
        if st.0 > 0 || st.1 == 0 {
            return;
        }
        let h = HANDLE(std::mem::take(&mut st.1) as *mut _);
        unsafe {
            for r in REQUESTS {
                let _ = PowerClearRequest(h, r);
            }
            let _ = CloseHandle(h);
        }
        info!("the display may turn off again");
    }

    pub fn wake_display() {
        // 不带 ES_CONTINUOUS：只是当作有人碰了一下，熄了的屏幕亮起来、显示器的空闲计时从头开始
        unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) };
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ptr::NonNull;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use objc2_core_foundation::{CFRetained, CFString, CFType};
    use tracing::{debug, info, warn};

    /// `IOPMAssertionID`
    type AssertionId = u32;
    /// `kIOPMAssertionLevelOn`
    const LEVEL_ON: u32 = 255;
    /// `kIOPMUserActiveLocal`：本机前面有人（`kIOPMUserActiveRemote` 不一定点亮显示器）
    const USER_ACTIVE_LOCAL: u32 = 0;
    /// `kIOReturnSuccess`
    const SUCCESS: i32 = 0;
    /// `kIOPMAssertPreventUserIdleDisplaySleep`
    const NO_IDLE_DISPLAY_SLEEP: &str = "PreventUserIdleDisplaySleep";
    /// `kIOPMAssertPreventUserIdleSystemSleep`
    const NO_IDLE_SLEEP: &str = "PreventUserIdleSystemSleep";
    /// `kIOPMAssertionTypePreventSystemSleep`：接着外接电源时合上盖子也不睡眠（见模块说明）
    const NO_SLEEP: &str = "PreventSystemSleep";
    /// `IOPSGetProvidingPowerSourceType` 说用的是外接电源（`kIOPMACPowerKey`）
    const AC_POWER: &str = "AC Power";
    /// 叫醒显示器最多这么频繁（采集线程打开失败时每 250 ms 重试一次）
    const WAKE_EVERY: Duration = Duration::from_secs(2);

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: &CFString,
            level: u32,
            name: &CFString,
            id: *mut AssertionId,
        ) -> i32;
        fn IOPMAssertionRelease(id: AssertionId) -> i32;
        fn IOPMAssertionDeclareUserActivity(
            name: &CFString,
            user: u32,
            id: *mut AssertionId,
        ) -> i32;
        fn IOPSCopyPowerSourcesInfo() -> Option<NonNull<CFType>>;
        /// 不用释放：属于传进去的那份快照
        fn IOPSGetProvidingPowerSourceType(snapshot: &CFType) -> Option<NonNull<CFString>>;
    }

    /// 要一个电源断言（`pmset -g assertions` 里显示 `name`）。要不到时记一笔，返回 `None`
    fn create(kind: &str, name: &str) -> Option<AssertionId> {
        let (k, n) = (CFString::from_str(kind), CFString::from_str(name));
        let mut id = 0;
        let r = unsafe { IOPMAssertionCreateWithName(&k, LEVEL_ON, &n, &mut id) };
        if r != SUCCESS {
            warn!("can't create the power assertion {kind}: IOReturn {r:#x}");
            return None;
        }
        Some(id)
    }

    fn release(id: AssertionId) {
        unsafe { IOPMAssertionRelease(id) };
    }

    // ───────────── 会话期间 ─────────────

    /// 有几个 [`super::Awake`]，会话期间要着的断言
    static SESSIONS: Mutex<(usize, Vec<AssertionId>)> = Mutex::new((0, Vec::new()));

    pub fn acquire() {
        let mut st = SESSIONS.lock().unwrap();
        st.0 += 1;
        if st.0 > 1 {
            return;
        }
        // PreventSystemSleep：接着电源时合上盖子也不睡眠（见模块说明）。合上盖子的那一刻系统就开始睡了，
        // 等虚拟显示器补上之后再要来不及
        st.1 = [NO_IDLE_DISPLAY_SLEEP, NO_IDLE_SLEEP, NO_SLEEP]
            .into_iter()
            .filter_map(|k| create(k, "Airhand: a remote session is active"))
            .collect();
        info!("keeping the display on while controllers are connected");
    }

    pub fn release_session() {
        let mut st = SESSIONS.lock().unwrap();
        st.0 = st.0.saturating_sub(1);
        if st.0 > 0 || st.1.is_empty() {
            return;
        }
        for id in std::mem::take(&mut st.1) {
            release(id);
        }
        info!("the display may turn off again");
    }

    /// 上次叫醒显示器的时刻，`IOPMAssertionDeclareUserActivity` 给的编号（再叫的时候带上它）
    static WOKEN: Mutex<(Option<Instant>, AssertionId)> = Mutex::new((None, 0));

    pub fn wake_display() {
        if lid_hides_everything() {
            debug!("the lid is closed and there's no other display: not waking the built-in one");
            return;
        }
        let mut st = WOKEN.lock().unwrap();
        if st.0.is_some_and(|t| t.elapsed() < WAKE_EVERY) {
            return;
        }
        st.0 = Some(Instant::now());
        let name = CFString::from_static_str("Airhand: a controller is connected");
        let r = unsafe { IOPMAssertionDeclareUserActivity(&name, USER_ACTIVE_LOCAL, &mut st.1) };
        if r == SUCCESS {
            debug!("declared user activity to wake the display");
        } else {
            warn!("can't wake the display: IOReturn {r:#x}");
        }
    }

    /// 合着盖子，接着的显示器只有内建的（或者一个都没有）：叫醒了也只是在盖子后面亮一会儿（见模块说明）
    fn lid_hides_everything() -> bool {
        use objc2_core_graphics::CGDisplayIsBuiltin;
        crate::host::macos::lid_closed()
            && crate::host::macos::online_display_ids()
                .into_iter()
                .all(|id| CGDisplayIsBuiltin(id))
    }

    /// 有虚拟显示器期间不睡眠（见 [`super::keep_running`]）。丢掉时放开
    pub struct Running(Option<AssertionId>);

    pub fn keep_running() -> Running {
        let id = create(NO_SLEEP, "Airhand: a virtual display is in use");
        if id.is_some() {
            info!("preventing sleep while the virtual display is in use");
        }
        Running(id)
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(id) = self.0.take() {
                release(id);
                info!("the computer may sleep when the lid is closed again");
            }
        }
    }

    // ───────────── 不自动睡眠 ─────────────

    /// 现在用的是不是外接电源。台式机总是；读不出来时也当作是
    pub fn on_ac() -> bool {
        let Some(snapshot) = (unsafe { IOPSCopyPowerSourcesInfo() }) else {
            return true;
        };
        let snapshot = unsafe { CFRetained::from_raw(snapshot) };
        match unsafe { IOPSGetProvidingPowerSourceType(&snapshot) } {
            Some(kind) => unsafe { kind.as_ref() }.to_string() == AC_POWER,
            None => true,
        }
    }

    struct Idle {
        /// 有几个 [`super::Hosting`]
        hosts: usize,
        /// 要着的 `PreventUserIdleSystemSleep`、`PreventSystemSleep`（空的：没要）
        assertions: Vec<AssertionId>,
        /// 有线程在定时看电源（见 [`watch`]）
        watching: bool,
    }

    static IDLE: Mutex<Idle> = Mutex::new(Idle {
        hosts: 0,
        assertions: Vec::new(),
        watching: false,
    });

    /// 被控端在运行、开着"不自动睡眠"
    fn wanted(st: &Idle) -> bool {
        st.hosts > 0 && super::prevent_sleep()
    }

    pub fn host_started() {
        IDLE.lock().unwrap().hosts += 1;
        refresh();
    }

    pub fn host_stopped() {
        {
            let mut st = IDLE.lock().unwrap();
            st.hosts = st.hosts.saturating_sub(1);
        }
        refresh();
    }

    /// 按现在的情况要 / 放开断言。要的话起一个线程定时看电源
    pub fn refresh() {
        let mut st = IDLE.lock().unwrap();
        apply(&mut st);
        if wanted(&st) && !st.watching {
            st.watching = true;
            let spawned = std::thread::Builder::new()
                .name("airhand-power".into())
                .spawn(watch);
            if let Err(e) = spawned {
                warn!("can't watch the power source: {e}");
                st.watching = false;
            }
        }
    }

    /// 定时看用的是不是外接电源，接上、拔掉电源时跟着要 / 放开断言。不再需要（被控端停了、"不自动睡眠"关了）时退出
    fn watch() {
        loop {
            std::thread::sleep(super::POWER_CHECK);
            let mut st = IDLE.lock().unwrap();
            apply(&mut st);
            if !wanted(&st) {
                st.watching = false;
                return;
            }
        }
    }

    fn apply(st: &mut Idle) {
        let wanted = wanted(st);
        let hold = wanted && on_ac();
        if hold && st.assertions.is_empty() {
            // PreventSystemSleep：合上盖子也不睡（见模块说明）
            st.assertions = [NO_IDLE_SLEEP, NO_SLEEP]
                .into_iter()
                .filter_map(|k| create(k, "Airhand: Prevent automatic sleeping"))
                .collect();
            if !st.assertions.is_empty() {
                info!(
                    "preventing sleep (also with the lid closed) while the host runs on AC power"
                );
            }
        } else if !hold && !st.assertions.is_empty() {
            for id in std::mem::take(&mut st.assertions) {
                release(id);
            }
            info!(on_battery = wanted, "the computer may sleep again");
        }
        // 合上盖子之后连上来也要有画面：没有会话时也预备着虚拟显示器（接不接着电源它自己看）。
        // 单元测试里不建（会真的在本机多出一块显示器、屏幕闪一下）
        #[cfg(not(test))]
        crate::host::virtual_display::set_ready(wanted);
    }

    #[cfg(test)]
    pub fn holding() -> bool {
        !IDLE.lock().unwrap().assertions.is_empty()
    }

    #[cfg(test)]
    pub fn session_assertions() -> usize {
        SESSIONS.lock().unwrap().1.len()
    }
}

/// 有会话期间保持亮屏、不睡眠（见模块说明）
pub fn keep_awake() -> Awake {
    #[cfg(any(windows, target_os = "macos"))]
    imp::acquire();
    Awake(())
}

impl Drop for Awake {
    fn drop(&mut self) {
        #[cfg(windows)]
        imp::release();
        #[cfg(target_os = "macos")]
        imp::release_session();
    }
}

/// 有虚拟显示器期间（macOS，见 `virtual_display`）：接着外接电源时合上盖子也不睡眠（见模块说明）。丢掉时放开
#[cfg(target_os = "macos")]
pub use imp::Running;

#[cfg(target_os = "macos")]
pub fn keep_running() -> Running {
    imp::keep_running()
}

/// 现在用的是不是外接电源（macOS；台式机总是，读不出来时也当作是）
#[cfg(target_os = "macos")]
pub fn on_ac() -> bool {
    imp::on_ac()
}

/// 屏幕熄了的话叫醒它（见模块说明）。macOS 上两次之间至少隔 2 秒，多叫的什么也不做；合着盖子、
/// 没有别的显示器时也不叫
pub fn wake_display() {
    #[cfg(any(windows, target_os = "macos"))]
    imp::wake_display();
}

/// "不自动睡眠"：启动时按 config.toml 的 `prevent_sleep`，托盘菜单里切换的马上生效（[`set_prevent_sleep`]）
static PREVENT_SLEEP: LazyLock<AtomicBool> =
    LazyLock::new(|| AtomicBool::new(crate::config::settings().prevent_sleep));

/// 开着"不自动睡眠"没有（只有 macOS 上起作用）
pub fn prevent_sleep() -> bool {
    PREVENT_SLEEP.load(Ordering::Relaxed)
}

/// 打开 / 关掉"不自动睡眠"，马上生效（保存进 config.toml 的是调用的一方，见 `host::set_prevent_sleep`）
pub fn set_prevent_sleep(on: bool) {
    PREVENT_SLEEP.store(on, Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    imp::refresh();
}

/// 被控端开始运行（见 [`Hosting`]）
pub fn hosting() -> Hosting {
    #[cfg(target_os = "macos")]
    imp::host_started();
    Hosting(())
}

impl Drop for Hosting {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        imp::host_stopped();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// 会话期间的三个断言：第一个会话开始时要，最后一个结束时放开
    #[test]
    fn session_assertions_follow_sessions() {
        let a = keep_awake();
        let b = keep_awake();
        assert_eq!(imp::session_assertions(), 3);
        drop(a);
        assert_eq!(imp::session_assertions(), 3, "还有一个会话");
        drop(b);
        assert_eq!(imp::session_assertions(), 0);
    }

    /// 不自动睡眠：被控端在运行、开着、接着外接电源时才要断言；关掉、被控端停了都放开
    #[test]
    fn prevent_sleep_follows_setting_and_host() {
        let was = prevent_sleep();
        set_prevent_sleep(false);
        let host = hosting();
        assert!(!imp::holding());
        set_prevent_sleep(true);
        let ac = imp::on_ac();
        println!("外接电源：{ac}");
        assert_eq!(imp::holding(), ac);
        drop(host);
        assert!(!imp::holding());
        let host = hosting();
        assert_eq!(imp::holding(), ac);
        set_prevent_sleep(false);
        assert!(!imp::holding());
        drop(host);
        set_prevent_sleep(was);
    }

    /// 断言真的到了系统那里：`pmset -g assertions` 里看得到。要接着外接电源。
    /// `cargo test --lib shows_in_pmset -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn shows_in_pmset() {
        let pmset = || {
            let out = std::process::Command::new("/usr/bin/pmset")
                .args(["-g", "assertions"])
                .output()
                .expect("pmset");
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines()
                .filter(|l| l.contains("Airhand"))
                .map(str::trim)
                .map(String::from)
                .collect::<Vec<_>>()
        };
        let was = prevent_sleep();
        let awake = keep_awake();
        let host = hosting();
        set_prevent_sleep(true);
        let held = pmset();
        println!("{held:#?}");
        let has =
            |kind: &str, name: &str| held.iter().any(|l| l.contains(kind) && l.contains(name));
        assert!(has("PreventUserIdleDisplaySleep", "remote session"));
        assert!(has("PreventUserIdleSystemSleep", "remote session"));
        assert!(has("PreventSystemSleep", "remote session"));
        assert!(has(
            "PreventUserIdleSystemSleep",
            "Prevent automatic sleeping"
        ));
        assert!(has("PreventSystemSleep", "Prevent automatic sleeping"));
        drop(awake);
        drop(host);
        set_prevent_sleep(was);
        assert_eq!(pmset(), Vec::<String>::new());
    }
}
