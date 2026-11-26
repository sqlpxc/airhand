//! macOS：远程控制时暂停 AWDL，走 Wi-Fi 的远程控制不再周期性地卡顿（菜单栏图标的菜单里的
//! "远程控制时暂停隔空投送"，默认关，记在 config.toml 的 `pause_awdl` 里）。
//!
//! # 为什么
//!
//! AWDL（Apple Wireless Direct Link）是隔空投送、隔空播放、随航、通用控制、连续互通相机、iPhone 镜像……用的
//! Wi-Fi 直连。它没有自己的无线电，和 Wi-Fi 共用一块网卡：开着的时候网卡每隔一会儿切到 AWDL 的信道
//! （6、44、149）上待一小会儿，这期间路由器发来的包只能等着，远程控制的画面和操作就周期性地卡一下。
//! 插网线（不经过这块网卡）或者 `sudo ifconfig awdl0 down` 就好了；只是从 Ventura 起，系统一有需要就会马上把
//! awdl0 重新打开（睡眠唤醒之后也会），一次 `ifconfig` 管不了多久。
//!
//! # 怎么做
//!
//! * 一个以 root 运行的小助手（`helper.c`，怎么装见 [`helper`]）：有人"拿着"它的时候让 awdl0 一直关着
//!   （路由套接字一报接口变了就再关上），最后一个放开时恢复原样。只做这一件事，不接受参数；
//! * Airhand 这边每个远程控制会话拿一个 [`Session`]：本机被控制（`host::session`）、本机控制别人
//!   （控制台的每个连接，见 `console.rs`）都算。有会话、打开了这一项、而且本机正用 Wi-Fi 联网
//!   （[`probe::wifi_in_use`]，插着网线时不用暂停）时，后台线程连上助手拿着它，不需要了就断开。断开就是放开：
//!   Airhand 退出、崩溃时连接也会断，AWDL 不会一直关着。最后一个会话结束后再拿着 [`GRACE`]，断线重连时不来回开关；
//! * 被控端在会话里告诉控制端"这台 Mac 走 Wi-Fi、AWDL 开着"（`Notice::Awdl`，见 [`hint`]），
//!   控制端提示插网线或者打开这一项。
//!
//! 暂停期间这台 Mac 的隔空投送、隔空播放到这台 Mac、随航、通用控制、连续互通相机、iPhone 镜像等用不了，
//! 远程控制结束后恢复。第一次打开时先说明这些，再装助手（要输一次管理员密码）；关掉时助手留着（没人连它就不运行），
//! 卸载 Airhand 时一起拿掉（见 [`remove`]）。

pub mod helper;
mod probe;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tauri::AppHandle;
use tracing::{info, warn};

pub use helper::Installed;

use crate::status::StatusHub;

/// 最后一个会话结束后再拿着这么久：断线重连、马上又连另一台时不来回开关
const GRACE: Duration = Duration::from_secs(20);
/// 后台线程多久看一次（网线插拔、助手还在不在）
const TICK: Duration = Duration::from_secs(3);
/// 连不上助手时隔这么久再试
const RETRY: Duration = Duration::from_secs(15);

static HUB: OnceLock<StatusHub> = OnceLock::new();

/// 菜单里的勾选：启动时按 config.toml 的 `pause_awdl`，切换的马上生效（[`set_enabled`]）
static ENABLED: LazyLock<AtomicBool> =
    LazyLock::new(|| AtomicBool::new(crate::config::settings().pause_awdl));

struct State {
    /// 进行中的远程控制会话（[`Session`]）
    sessions: usize,
    /// 会话数变成 0 的时刻（还在 [`GRACE`] 里的话接着拿着）
    idle_since: Option<Instant>,
    /// 拿着助手（连接开着期间 awdl0 一直关着）
    lease: Option<helper::Lease>,
    /// 后台线程在运行
    keeper: bool,
    /// 助手装上了没有。`None`：还没看过
    installed: Option<Installed>,
    /// 上次没做成的原因（连不上助手、助手关不上 awdl0），菜单里显示
    error: Option<String>,
    /// 正在安装（等用户输密码）
    busy: bool,
    /// 连不上助手时，下次什么时候再试
    retry_at: Option<Instant>,
    /// 有变化，后台线程马上再看一遍（见 [`State::poke`]）
    poked: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    sessions: 0,
    idle_since: None,
    lease: None,
    keeper: false,
    installed: None,
    error: None,
    busy: false,
    retry_at: None,
    poked: false,
});
static WAKE: Condvar = Condvar::new();

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

impl State {
    fn installed(&mut self) -> Installed {
        *self.installed.get_or_insert_with(helper::installed)
    }

    fn view(&mut self) -> View {
        View {
            enabled: enabled(),
            installed: self.installed(),
            paused: self.lease.as_ref().is_some_and(|l| l.error.is_none()),
            error: self.error.clone(),
            busy: self.busy,
        }
    }

    /// 叫后台线程马上再看一遍（它正忙着的话，忙完了不等就接着看：不会漏掉）
    fn poke(&mut self) {
        self.poked = true;
        WAKE.notify_all();
    }

    fn start_keeper(&mut self) {
        if self.keeper {
            return;
        }
        match std::thread::Builder::new().name("awdl".into()).spawn(keep) {
            Ok(_) => self.keeper = true,
            Err(e) => warn!("can't start the AWDL thread: {e}"),
        }
    }
}

/// 菜单栏图标的菜单显示的（见 `tray.rs`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// 打开了"远程控制时暂停隔空投送"
    pub enabled: bool,
    pub installed: Installed,
    /// 正暂停着
    pub paused: bool,
    /// 没能暂停的原因（英文，技术信息）
    pub error: Option<String>,
    /// 正在安装助手
    pub busy: bool,
}

pub fn init(hub: &StatusHub) {
    let _ = HUB.set(hub.clone());
}

/// 菜单跟着变
fn refresh() {
    if let Some(hub) = HUB.get() {
        hub.send_modify(|_| {});
    }
}

pub fn view() -> View {
    state().view()
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 被控端会话：要不要告诉控制端"这台 Mac 走 Wi-Fi、AWDL 开着，画面可能周期性地卡顿"。
/// 暂停着的时候不提示：系统反复打开 awdl0 时，助手关上之前可能正好看到它开着，控制端的提示不该跟着一闪一闪
pub fn hint() -> bool {
    let paused = state().lease.as_ref().is_some_and(|l| l.error.is_none());
    !paused && probe::awdl_up() && probe::wifi_in_use()
}

/// 一个远程控制会话：拿着它期间（打开了这一项、走 Wi-Fi 时）暂停 AWDL
pub struct Session(());

pub fn session() -> Session {
    let mut st = state();
    st.sessions += 1;
    st.idle_since = None;
    st.start_keeper();
    st.poke();
    Session(())
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut st = state();
        st.sessions = st.sessions.saturating_sub(1);
        if st.sessions == 0 {
            st.idle_since = Some(Instant::now());
        }
        st.poke();
    }
}

/// 后台线程：按会话、设置、网线，连上 / 断开助手。没有会话、过了 [`GRACE`]、放开了助手就退出
fn keep() {
    let mut st = state();
    loop {
        let before = st.view();
        let now = Instant::now();
        let busy = st.sessions > 0 || st.idle_since.is_some_and(|t| now.duration_since(t) < GRACE);
        if !busy && st.lease.is_none() {
            st.keeper = false;
            st.error = None;
            let changed = st.view() != before;
            drop(st);
            if changed {
                refresh();
            }
            return;
        }
        if st.lease.as_ref().is_some_and(|l| !l.alive()) {
            warn!("the AWDL helper went away");
            st.lease = None;
            st.installed = None;
        }
        let maybe = busy && enabled() && st.installed() != Installed::No;
        // 看网卡要几百微秒：不拿着锁
        drop(st);
        let want = maybe && probe::wifi_in_use();
        st = state();
        if want && st.lease.is_none() && st.retry_at.is_none_or(|t| Instant::now() >= t) {
            drop(st);
            let r = helper::hold(Path::new(helper::SOCKET));
            st = state();
            match r {
                Ok(lease) => {
                    match &lease.error {
                        None => info!("AWDL paused for remote control"),
                        Some(e) => warn!("the AWDL helper can't take awdl0 down: {e}"),
                    }
                    st.error = lease.error.as_ref().map(|e| e.to_string());
                    st.retry_at = None;
                    st.lease = Some(lease);
                }
                Err(e) => {
                    if st.error.is_none() {
                        warn!("can't reach the AWDL helper: {e}");
                    }
                    st.error = Some(e.to_string());
                    st.retry_at = Some(Instant::now() + RETRY);
                    // 可能是被拿掉了
                    st.installed = None;
                }
            }
        } else if !want && st.lease.take().is_some() {
            info!("AWDL resumed");
        }
        if !want {
            st.error = None;
            st.retry_at = None;
        }
        let changed = st.view() != before;
        if changed {
            drop(st);
            refresh();
            st = state();
        }
        if !st.poked {
            st = WAKE
                .wait_timeout(st, TICK)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.poked = false;
    }
}

/// 打开 / 关掉，写进 config.toml，马上生效。写不进去就不改（不然重新启动之后又变回去）。要写文件：不在主线程上调用
fn set_enabled(on: bool) {
    match crate::config::save_pause_awdl(on) {
        Ok(()) => {
            ENABLED.store(on, Ordering::Relaxed);
            info!(on, "pausing AWDL during remote control: changed");
        }
        Err(e) => warn!("can't change pausing AWDL during remote control: {e:#}"),
    }
    {
        let mut st = state();
        st.retry_at = None;
        if st.sessions > 0 {
            st.start_keeper();
        }
        st.poke();
    }
    refresh();
}

/// 菜单里点了"远程控制时暂停隔空投送"：关掉马上关；打开时助手还没装（或者旧了）就先说明、再装
pub fn toggle(app: &AppHandle) {
    if enabled() {
        std::thread::spawn(|| set_enabled(false));
        return;
    }
    let installed = {
        let mut st = state();
        if st.busy {
            return;
        }
        st.installed = None;
        st.installed()
    };
    match installed {
        Installed::Yes => {
            std::thread::spawn(|| set_enabled(true));
            return;
        }
        // 以前打开过、说明过了：只是更新一下助手
        Installed::Outdated => {
            install(app.clone(), true);
            return;
        }
        Installed::No => {}
    }
    let handle = app.clone();
    // 对话框是模态的：等这次菜单事件处理完，再在主线程上弹出来
    let _ = app.run_on_main_thread(move || {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return;
        };
        if ui::explain(mtm) {
            install(handle, true);
        }
    });
}

/// 菜单里点了"重新安装 / 更新"（打开着这一项，助手却没有了、旧了、连不上）
pub fn repair(app: &AppHandle) {
    install(app.clone(), false);
}

/// 装上助手（等用户输密码，不在主线程上），装好了 `enable` 就打开这一项
fn install(app: AppHandle, enable: bool) {
    {
        let mut st = state();
        if st.busy {
            return;
        }
        st.busy = true;
    }
    refresh();
    std::thread::spawn(move || {
        let r = helper::install(&tr!(
            "Airhand 要安装一个小组件，在远程控制时暂停隔空投送，让 Wi-Fi 更流畅。",
            "Airhand wants to install a small helper that pauses AirDrop during remote control for smoother Wi-Fi."
        ));
        {
            let mut st = state();
            st.busy = false;
            st.installed = None;
            st.error = None;
            st.retry_at = None;
        }
        match r {
            Ok(()) => {
                info!("installed the AWDL helper");
                if enable {
                    set_enabled(true);
                }
            }
            Err(helper::AdminError::Cancelled) => info!("installing the AWDL helper: cancelled"),
            Err(helper::AdminError::Failed(e)) => {
                warn!("can't install the AWDL helper: {e}");
                let _ = app.run_on_main_thread(move || {
                    if let Some(mtm) = objc2::MainThreadMarker::new() {
                        ui::failed(mtm, &e);
                    }
                });
            }
        }
        state().poke();
        refresh();
    });
}

/// 助手装着（卸载 Airhand 时要拿掉它）
pub fn present() -> bool {
    Path::new(helper::PLIST).exists() || Path::new(helper::PROGRAM).exists()
}

/// 卸载 Airhand 时：放开助手，再把它拿掉（要输一次管理员密码，阻塞到输完）
pub fn remove() -> Result<(), String> {
    // 不再拿着，也不再连上去（卸载时配置文件也删了）
    ENABLED.store(false, Ordering::Relaxed);
    state().lease = None;
    if !present() {
        return Ok(());
    }
    let r = helper::uninstall(&tr!(
        "Airhand 要移除在远程控制时暂停隔空投送的组件。",
        "Airhand wants to remove the helper that pauses AirDrop during remote control."
    ));
    state().installed = None;
    r.map_err(|e| e.to_string())
}

/// 对话框
mod ui {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle};
    use objc2_foundation::NSString;

    /// 第一次打开时：说明会怎样、要装什么。继续 = `true`
    pub fn explain(mtm: MainThreadMarker) -> bool {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&tr!(
            "远程控制时暂停隔空投送？",
            "Pause AirDrop during remote control?"
        )));
        alert.setInformativeText(&NSString::from_str(&tr!(
            "隔空投送、隔空播放、随航、通用控制等功能用的 Wi-Fi 直连（AWDL）和 Wi-Fi 共用这台 Mac 的无线网卡，每隔一会儿就把网卡借走一下，走 Wi-Fi 的远程控制因此会周期性地卡顿。\n\n打开之后，这台 Mac 用 Wi-Fi 联网、正在被远程控制或者控制别的电脑时，Airhand 会暂停 AWDL，远程控制结束后恢复。在此期间隔空投送、隔空播放到这台 Mac、随航、通用控制、连续互通相机、iPhone 镜像等用不了。插着网线时不暂停。\n\n这要安装一个只做这件事的小组件（以系统权限运行，要输入一次管理员密码），卸载 Airhand 时会一起移除。",
            "AirDrop, AirPlay, Sidecar, Universal Control and similar features use a Wi-Fi peer-to-peer link (AWDL) that shares this Mac's wireless card with Wi-Fi and keeps borrowing it for a moment, so remote control over Wi-Fi stutters now and then.\n\nWith this on, while this Mac is on Wi-Fi and is being controlled or controls another computer, Airhand pauses AWDL and resumes it when the remote control ends. Meanwhile AirDrop, AirPlay to this Mac, Sidecar, Universal Control, Continuity Camera, iPhone Mirroring and the like don't work. Nothing is paused while you use an Ethernet cable.\n\nThis needs a small helper that does only this (it runs with system privileges; you enter an administrator password once). It's removed when you uninstall Airhand."
        )));
        alert.addButtonWithTitle(&NSString::from_str(&tr!("继续…", "Continue…")));
        alert.addButtonWithTitle(&NSString::from_str(&tr!("取消", "Cancel")));
        crate::tray::front_panel(mtm, &alert.window());
        alert.runModal() == NSAlertFirstButtonReturn
    }

    /// 没装上（不是用户取消的）
    pub fn failed(mtm: MainThreadMarker, detail: &str) {
        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(&tr!(
            "没能安装暂停隔空投送的组件",
            "Couldn't install the helper that pauses AirDrop"
        )));
        alert.setInformativeText(&NSString::from_str(detail));
        alert.addButtonWithTitle(&NSString::from_str(&tr!("好", "OK")));
        crate::tray::front_panel(mtm, &alert.window());
        alert.runModal();
    }
}
