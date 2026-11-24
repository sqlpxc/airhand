//! Airhand 客户端：同一个二进制既是被控端也是控制端。
//!
//! * 启动后默认是**被控端**：只有托盘图标，没有任何窗口；
//! * 托盘菜单可切换为**控制端**：打开现代化控制台，可同时控制多台电脑。Windows 上打开控制台时被控端照常运行，
//!   macOS / Linux 上被控端暂停、关掉控制台再恢复（见 [`status::HOST_WITH_CONSOLE`]）；
//! * Windows 需要安装：为所有用户安装时被控端作为系统服务运行（见 `winsvc`），托盘只负责显示和操作；
//!   仅为当前用户安装时（见 `winsvc::user`）没有系统服务，和调试构建直接运行时一样，被控端在本进程里运行。
//!   macOS / Linux 的被控端一直在本进程里运行；
//! * macOS 的登录时自动启动是系统的登录项（见 [`login_item`]），第一次从"应用程序"文件夹运行时默认打开；
//! * 配置和数据只有一个 `config.toml`（见 [`config`]）：服务器设置、本机的控制码、控制台的"我的设备"
//!   （见 [`devices`]）。Windows 在安装目录，macOS 在 `~/Library/Application Support/app.airhand.client/`，
//!   Linux 在 `~/.config/airhand/`；被删掉了会自动重新生成。其余运行时产生的文件退出时删除（见 [`ephemeral`]）；
//! * 界面支持中文 / 英文，默认跟随系统语言，选过的记在 `config.toml` 里（见 [`i18n`]）。

// 最先声明：其它模块都要用到 `tr!`
#[macro_use]
mod i18n;
mod clipboard;
mod clipfiles;
mod config;
mod console;
mod devices;
#[cfg(test)]
mod e2e;
mod ephemeral;
mod fullscreen;
pub mod host;
#[cfg(target_os = "macos")]
mod login_item;
mod popout;
mod single_instance;
pub mod status;
mod sysinfo;
mod tray;
#[cfg(target_os = "macos")]
mod uninstall;
#[cfg(windows)]
mod winsvc;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use airhand_link::secure::Identity;
use airhand_link::{Problem, ServerState};
use serde::Serialize;
use tauri::webview::{PermissionKind, PermissionResponse};
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tracing::{info, warn};

use host::{HostCmd, HostCode, HostService};
use i18n::Lang;
use status::{HostStatus, Mode, StatusHub};

const CONSOLE: &str = "main";

/// 正在退出（或卸载）：关掉控制台时不再切回被控端（见 [`on_console_destroyed`]）
static EXITING: AtomicBool = AtomicBool::new(false);

pub struct AppState {
    hub: StatusHub,
    services: Mutex<Services>,
    /// 本机的控制码：本进程里的被控端停下再启动（macOS / Linux 打开、关掉控制台）时接着用它
    host_code: HostCode,
    console_opening: AtomicBool,
    webview_dirs: Mutex<Vec<PathBuf>>,
    /// Windows 安装版：被控端在系统服务里运行（托盘只负责显示和操作），切换模式不影响它
    #[cfg_attr(not(windows), allow(dead_code))]
    service_mode: bool,
    /// 控制台连过来的 WebSocket 地址（第一次要的时候开始监听，见 [`console`]）
    console: tokio::sync::OnceCell<String>,
}

#[derive(Default)]
struct Services {
    host: Option<Hosting>,
    /// 控制端模式：控制台的身份（端到端加密）。控制端服务每个控制台连接一个，在 [`console`] 里
    controller: Option<Identity>,
}

/// 被控端在哪里运行
enum Hosting {
    /// 本进程里
    InProcess(HostService),
    /// Windows 安装版：系统服务里，托盘经命名管道和它通信
    #[cfg(windows)]
    Service(winsvc::ipc::AgentLink),
}

/// 托盘菜单里对配对的操作
#[derive(Debug, Clone, Copy)]
enum PairingCmd {
    /// 换一个配对码（暂停了的配对随之恢复）
    NewCode,
    /// 忘掉所有配对过的控制端
    Forget,
}

impl Hosting {
    fn send(&self, cmd: HostCmd) {
        match self {
            Hosting::InProcess(h) => h.send(cmd),
            #[cfg(windows)]
            Hosting::Service(link) => link.send(match cmd {
                HostCmd::KickAll => winsvc::ipc::ToAgent::KickAll,
                HostCmd::NewCode => winsvc::ipc::ToAgent::NewCode,
            }),
        }
    }

    /// 托盘菜单里切换"结束远程后锁屏"（见 `host::ending`）。`hub`：本进程里的被控端用的状态（就是托盘的）
    fn set_lock_on_end(&self, hub: &StatusHub, on: bool) {
        match self {
            Hosting::InProcess(_) => {
                let hub = hub.clone();
                // 要写配置文件，不在主线程上做
                std::thread::spawn(move || host::set_lock_on_end(&hub, on));
            }
            #[cfg(windows)]
            Hosting::Service(link) => link.send(winsvc::ipc::ToAgent::SetLockOnEnd { on }),
        }
    }

    /// 菜单里切换"不自动睡眠"（macOS，见 `host::power`）。`hub`：本进程里的被控端用的状态（就是托盘的）
    fn set_prevent_sleep(&self, hub: &StatusHub, on: bool) {
        match self {
            Hosting::InProcess(_) => {
                let hub = hub.clone();
                // 要写配置文件，不在主线程上做
                std::thread::spawn(move || host::set_prevent_sleep(&hub, on));
            }
            // 服务里的被控端没有这一项（`HostStatus::prevent_sleep` 是 `None`，菜单里不显示）
            #[cfg(windows)]
            Hosting::Service(_) => {}
        }
    }

    fn pairing(&self, cmd: PairingCmd) {
        match self {
            Hosting::InProcess(h) => {
                let gate = h.gate().clone();
                // 要写配置文件，不在主线程上做
                std::thread::spawn(move || match cmd {
                    PairingCmd::NewCode => gate.new_pairing_code(),
                    PairingCmd::Forget => gate.forget_paired(),
                });
            }
            #[cfg(windows)]
            Hosting::Service(link) => link.send(match cmd {
                PairingCmd::NewCode => winsvc::ipc::ToAgent::NewPairingCode,
                PairingCmd::Forget => winsvc::ipc::ToAgent::ForgetPaired,
            }),
        }
    }
}

// ───────────────────────────── 模式切换 ─────────────────────────────

/// 切换后台服务（不涉及窗口）。Windows 上被控端一直运行（系统服务里，或者本进程里，见
/// [`status::HOST_WITH_CONSOLE`]），只启停控制端；macOS / Linux 上本进程里的被控端和控制端轮流运行。
fn switch_services(app: &AppHandle, mode: Mode) {
    let state = app.state::<AppState>();
    let current = state.hub.borrow().mode;
    if current != mode {
        info!(?mode, "switching mode");
        // 被控端跟着模式启停（打开控制台时停下）
        let host_follows = !status::HOST_WITH_CONSOLE;
        let mut sv = state.services.lock().unwrap();
        // 先停止旧服务（Drop 即中止并断开连接），再启动新服务
        sv.controller = None;
        if host_follows {
            sv.host = None;
        }
        state.hub.send_modify(|s| {
            s.mode = mode;
            s.controller = ServerState::Connecting;
            if host_follows {
                s.host = HostStatus::default();
            }
        });
        match mode {
            Mode::Host if host_follows => {
                sv.host = Some(Hosting::InProcess(HostService::start(
                    state.hub.clone(),
                    state.host_code.clone(),
                )));
                #[cfg(target_os = "macos")]
                host::macos::request_permissions();
            }
            Mode::Host => {}
            Mode::Controller => {
                // 控制台的身份（端到端加密）：第一次用时生成，记进 config.toml。
                // 控制端服务在控制台连上来时启动（见 console）
                let (identity, new_key) = config::controller_identity();
                if let Some(key) = new_key {
                    save_controller_key(&sv, key);
                }
                sv.controller = Some(identity);
            }
        }
    }
}

/// 记下控制台新生成的身份。Windows 安装版：配置文件由服务里的被控端维护，经命名管道交给它写
#[cfg_attr(not(windows), allow(unused_variables))]
fn save_controller_key(sv: &Services, key: String) {
    #[cfg(windows)]
    if let Some(Hosting::Service(link)) = &sv.host {
        link.send(winsvc::ipc::ToAgent::SaveControllerKey { key });
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = config::save_controller_key(&key) {
            warn!("can't save the console's identity: {e:#}");
        }
    });
}

fn set_mode(app: &AppHandle, mode: Mode) {
    switch_services(app, mode);
    match mode {
        Mode::Controller => open_console(app),
        Mode::Host => {
            if let Some(w) = app.get_webview_window(CONSOLE) {
                let _ = w.destroy();
            }
        }
    }
}

/// macOS：是否在程序坞里显示图标（也决定能不能用 Command+Tab 切换到控制台）。
/// 只有托盘图标时是"辅助程序"，打开控制台时变成普通程序。
#[cfg(target_os = "macos")]
fn show_in_dock(app: &AppHandle, visible: bool) {
    let policy = if visible {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    let _ = app.set_activation_policy(policy);
}

fn open_console(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    show_in_dock(app, true);
    if let Some(w) = app.get_webview_window(CONSOLE) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // 双击后的松键也会请求打开控制台；前一次还在异步建窗时不再创建。
    if app
        .state::<AppState>()
        .console_opening
        .swap(true, Ordering::AcqRel)
    {
        return;
    }
    let app = app.clone();
    // Windows 上在主线程同步创建 WebView2 可能死锁，放到异步任务中创建
    tauri::async_runtime::spawn(async move {
        let handle = app.clone();
        let builder =
            WebviewWindowBuilder::new(&app, CONSOLE, WebviewUrl::App("index.html".into()))
                // 把对方的一个显示器放到单独的窗口里（见 popout）
                .on_new_window(move |url, features| popout::on_new_window(&handle, url, features));
        // WebView2 / WebKitGTK 需要一个数据目录（放在临时目录里，用完删除，见 ephemeral）；
        // macOS 的 WKWebView 没有这个概念，隐私模式下不保存网页数据（WebKit 自己在系统临时目录里建的目录见 ephemeral）
        #[cfg(not(target_os = "macos"))]
        let builder = {
            let dir = ephemeral::new_dir();
            app.state::<AppState>()
                .webview_dirs
                .lock()
                .unwrap()
                .push(dir.clone());
            builder.data_directory(dir)
        };
        let builder = builder
            .title(console_title())
            // 首屏就用当前语言（之后的切换通过 lang-changed 事件通知）
            .initialization_script(format!(
                "window.__AIRHAND_LANG__ = {:?};",
                i18n::current().code()
            ))
            .inner_size(1360.0, 860.0)
            .min_inner_size(820.0, 560.0)
            .center()
            .incognito(true)
            .disable_drag_drop_handler()
            .background_color(tauri::window::Color(9, 11, 16, 255))
            .theme(Some(tauri::Theme::Dark));
        // 无边框：标题栏就是控制台顶部的标签栏（拖动、双击最大化、窗口按钮都由界面处理）。
        // macOS 保留系统的红绿灯按钮，叠在标签栏左侧（垂直居中于 40px 高的标签栏）
        #[cfg(target_os = "macos")]
        let builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true)
            .traffic_light_position(tauri::LogicalPosition::new(16.0, 22.0));
        #[cfg(not(target_os = "macos"))]
        let builder = builder.decorations(false);
        let built = builder.build();
        match built {
            Ok(w) => {
                let _ = w.set_focus();
            }
            Err(e) => warn!("can't open the console: {e}"),
        }
        app.state::<AppState>()
            .console_opening
            .store(false, Ordering::Release);
    });
}

fn console_title() -> String {
    tr!("Airhand 控制台", "Airhand Console")
}

/// 切换界面语言（托盘菜单或控制台），记进配置文件：选过一次之后不再跟随系统语言。
fn set_lang(app: &AppHandle, lang: Lang) {
    let state = app.state::<AppState>();
    if i18n::set(lang) {
        info!(?lang, "switching UI language");
    }
    // 选的和现在的一样也记下：可能只是碰巧和系统语言一样，用户选的是它
    save_language(&state, lang);
    // 总是通知状态循环刷新托盘菜单：勾选项被点击时系统会自动翻转勾选状态，需要重建菜单恢复
    state.hub.send_modify(|_| {});
}

/// 把选的界面语言记进 config.toml。Windows 安装版：配置文件由服务里的被控端维护，经命名管道交给它写
/// （它也换成这个语言：它写进配置文件的说明按这个语言写）
fn save_language(state: &AppState, lang: Lang) {
    #[cfg(windows)]
    if state.service_mode {
        if let Some(Hosting::Service(link)) = &state.services.lock().unwrap().host {
            link.send(winsvc::ipc::ToAgent::SetLanguage { lang });
        }
        return;
    }
    #[cfg(not(windows))]
    let _ = state;
    std::thread::spawn(move || {
        if let Err(e) = config::save_language(lang) {
            warn!("can't save the UI language: {e:#}");
        }
    });
}

fn on_console_destroyed(app: &AppHandle) {
    // 关闭控制台 = 回到默认的被控端模式。退出、卸载时关掉的不算：不要再启动被控端
    //（卸载时它连上服务器会把控制码写回配置文件）
    if !EXITING.load(Ordering::Acquire) {
        switch_services(app, Mode::Host);
    }
    // 弹出窗口里的画面是控制台画的，控制台没了它们也就没用了
    popout::close_all(app);
    #[cfg(target_os = "macos")]
    {
        // 全屏中被关掉时菜单栏和程序坞还藏着
        fullscreen::reset(app);
        show_in_dock(app, false);
    }
    let dirs: Vec<PathBuf> =
        std::mem::take(&mut *app.state::<AppState>().webview_dirs.lock().unwrap());
    for d in dirs {
        ephemeral::remove_in_background(d);
    }
}

pub(crate) fn on_menu(app: &AppHandle, id: &str) {
    match id {
        // Windows 安装版从被控端模式打开控制台时，也要启动控制端服务，不能只创建窗口。
        "to_controller" | "console" => set_mode(app, Mode::Controller),
        "to_host" => set_mode(app, Mode::Host),
        "lang_zh" => set_lang(app, Lang::Zh),
        "lang_en" => set_lang(app, Lang::En),
        "kick_all" | "new_code" => {
            let cmd = if id == "kick_all" {
                HostCmd::KickAll
            } else {
                HostCmd::NewCode
            };
            if let Some(h) = &app.state::<AppState>().services.lock().unwrap().host {
                h.send(cmd);
            }
        }
        "lock_on_end" => {
            let state = app.state::<AppState>();
            let current = state.hub.borrow().host.lock_on_end;
            if let (Some(current), Some(h)) = (current, &state.services.lock().unwrap().host) {
                h.set_lock_on_end(&state.hub, !current);
            }
            // 勾选项被点击时系统会自动翻转勾选状态：先按原来的设置重建菜单，改好了（被控端确认后）再刷新
            state.hub.send_modify(|_| {});
        }
        "prevent_sleep" => {
            let state = app.state::<AppState>();
            let current = state.hub.borrow().host.prevent_sleep;
            if let (Some(current), Some(h)) = (current, &state.services.lock().unwrap().host) {
                h.set_prevent_sleep(&state.hub, !current);
            }
            // 同上：先按原来的设置重建菜单，改好了再刷新
            state.hub.send_modify(|_| {});
        }
        "new_pairing" | "forget_paired" => {
            let cmd = if id == "new_pairing" {
                PairingCmd::NewCode
            } else {
                PairingCmd::Forget
            };
            if let Some(h) = &app.state::<AppState>().services.lock().unwrap().host {
                h.pairing(cmd);
            }
        }
        #[cfg(windows)]
        "autostart" => {
            let state = app.state::<AppState>();
            let (user, service) = {
                let st = state.hub.borrow();
                (st.user_autostart, st.service.map(|s| s.autostart))
            };
            if let Some(current) = user {
                // 仅为当前用户安装：登录时自动启动是本用户的启动项，托盘自己改（先写配置文件：它为准）
                let hub = state.hub.clone();
                let on = !current;
                std::thread::spawn(move || {
                    match config::save_autostart(on)
                        .and_then(|()| winsvc::user::choose_autostart(on))
                    {
                        Ok(()) => {
                            info!(on, "autostart changed");
                            hub.send_modify(|s| s.user_autostart = Some(on));
                        }
                        Err(e) => warn!("can't change autostart: {e:#}"),
                    }
                });
            } else if let (Some(current), Some(Hosting::Service(link))) =
                (service, &state.services.lock().unwrap().host)
            {
                link.send(winsvc::ipc::ToAgent::SetAutostart { on: !current });
            }
            // 勾选项被点击时系统会自动翻转勾选状态：先按原来的设置重建菜单，改好了（被控端确认后）再刷新
            state.hub.send_modify(|_| {});
        }
        #[cfg(windows)]
        "start_service" => {
            std::thread::spawn(|| {
                if let Err(e) = winsvc::start() {
                    warn!("can't start the Airhand service: {e:#}");
                }
            });
        }
        #[cfg(target_os = "macos")]
        "autostart" => {
            let hub = app.state::<AppState>().hub.clone();
            // 等用户允许的也算"打开"：再点一下是去系统设置里允许（菜单里就是这么写的）
            let on = hub.borrow().login_item != Some(login_item::State::On);
            // 会经 XPC 问系统，不在主线程上等
            std::thread::spawn(move || match login_item::set(on, true) {
                Ok(s) => {
                    info!(state = ?s, "open at login changed");
                    record_login_item(s);
                    hub.send_modify(|st| st.login_item = Some(s));
                }
                Err(e) => {
                    warn!("{e}");
                    hub.send_modify(|_| {});
                }
            });
            // 勾选项被点击时系统会自动翻转勾选状态：先按原来的状态重建菜单，切换完了再刷新
            app.state::<AppState>().hub.send_modify(|_| {});
        }
        #[cfg(target_os = "macos")]
        "receive_files" => {
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || clipfiles::macos::choose(&handle));
        }
        #[cfg(target_os = "macos")]
        "cancel_receive" => clipfiles::macos::cancel(),
        #[cfg(target_os = "macos")]
        "uninstall" => {
            // 对话框是模态的：等这次菜单事件处理完，再在主线程上弹出来
            let handle = app.clone();
            let _ = app.run_on_main_thread(move || uninstall_app(&handle));
        }
        #[cfg(target_os = "macos")]
        "perm_screen" => {
            host::macos::open_privacy_settings(host::macos::PrivacyPane::ScreenRecording)
        }
        #[cfg(target_os = "macos")]
        "perm_input" => host::macos::open_privacy_settings(host::macos::PrivacyPane::Accessibility),
        "copy_code" => {
            if let Some(code) = app.state::<AppState>().hub.borrow().host.code {
                match clipboard::set_text(&format!("{code:06}")) {
                    Ok(()) => info!("control code copied"),
                    Err(e) => warn!("copying failed: {e}"),
                }
            }
        }
        "copy_pairing" => {
            let pairing = app.state::<AppState>().hub.borrow().host.pairing.clone();
            if let Some(p) = pairing {
                match clipboard::set_text(&p) {
                    Ok(()) => info!("pairing code copied"),
                    Err(e) => warn!("copying failed: {e}"),
                }
            }
        }
        "quit" => {
            let state = app.state::<AppState>();
            // Windows 安装版：退出 = 被控端也停止（和直接运行时一样，退出后本机不能再被控制）
            #[cfg(windows)]
            if state.service_mode
                && let Err(e) = winsvc::stop()
            {
                warn!("can't stop the Airhand service: {e:#}");
            }
            let mut sv = state.services.lock().unwrap();
            sv.host = None;
            sv.controller = None;
            drop(sv);
            app.exit(0);
        }
        _ => {}
    }
}

pub(crate) fn on_tray_left_click(app: &AppHandle) {
    if app.state::<AppState>().hub.borrow().mode == Mode::Controller {
        open_console(app);
    }
}

// ───────────────────────────── 前端命令 ─────────────────────────────

/// 控制台的 `appInfo`（经 WebSocket，见 [`console`]）
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    server: String,
    pinned: bool,
    hostname: String,
    lang: Lang,
    /// 配置文件（也保存着我的设备）的路径，控制台的菜单里显示
    config_path: Option<String>,
}

fn app_info() -> AppInfo {
    AppInfo {
        version: config::VERSION,
        server: config::settings().server.clone(),
        pinned: config::pinned(),
        hostname: sysinfo::host_identity().0,
        lang: i18n::current(),
        config_path: config::path().map(|p| p.display().to_string()),
    }
}

/// 我的设备（配置文件里本账户的）
#[tauri::command]
async fn devices_load() -> Vec<devices::Device> {
    tauri::async_runtime::spawn_blocking(devices::load)
        .await
        .unwrap_or_default()
}

/// 保存我的设备（控制台传来整个列表）
#[tauri::command]
#[cfg_attr(not(windows), allow(unused_variables))]
async fn devices_save(
    state: tauri::State<'_, AppState>,
    devices: Vec<devices::Device>,
) -> Result<(), Problem> {
    let list = tauri::async_runtime::spawn_blocking(move || devices::for_file(devices))
        .await
        .map_err(Problem::failed)?
        .map_err(|e| Problem::failed(format!("{e:#}")))?;
    // Windows 安装版：配置文件在安装目录里，由服务里的被控端维护（托盘没有写权限），经命名管道交给它写
    #[cfg(windows)]
    if state.service_mode {
        let connected = state.hub.borrow().service.is_some_and(|s| s.connected);
        let sv = state.services.lock().unwrap();
        return match &sv.host {
            Some(Hosting::Service(link)) if connected => {
                link.send(winsvc::ipc::ToAgent::SaveDevices { devices: list });
                Ok(())
            }
            _ => Err(Problem::failed(
                "the Airhand service isn't reachable from this session",
            )),
        };
    }
    tauri::async_runtime::spawn_blocking(move || config::save_devices(&list))
        .await
        .map_err(Problem::failed)?
        .map_err(|e| Problem::failed(format!("{e:#}")))
}

/// 控制台切换界面语言
#[tauri::command]
fn set_language(app: AppHandle, lang: Lang) {
    set_lang(&app, lang);
}

/// 控制台和控制端服务之间的 WebSocket 地址：会话、画面、声音都走它，不走 Tauri 的 IPC（见 [`console`]）
#[tauri::command]
async fn console_url(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<String, Problem> {
    state
        .console
        .get_or_try_init(|| console::listen(app.clone()))
        .await
        .cloned()
        .map_err(Problem::failed)
}

/// 读取本机剪贴板文本（用于"粘贴到远端"）
#[tauri::command]
fn clipboard_text() -> Result<String, Problem> {
    clipboard::get_text().map_err(|_| Problem::NoClipboardText)
}

/// 控制台窗口进入 / 退出全屏（各系统要额外处理的地方见 [`fullscreen`]）
#[tauri::command]
fn set_fullscreen(window: tauri::WebviewWindow, on: bool) -> Result<(), Problem> {
    fullscreen::set(&window, on).map_err(Problem::failed)
}

/// 控制台窗口是不是全屏（包括 macOS 上自己铺满屏幕的全屏，见 [`fullscreen`]）
#[tauri::command]
fn window_fullscreen(window: tauri::WebviewWindow) -> Result<bool, Problem> {
    fullscreen::is_on(&window).map_err(Problem::failed)
}

/// 弹出窗口进入 / 退出全屏（控制台调用，按 label 找窗口，见 [`popout`]）
#[tauri::command]
fn popout_set_fullscreen(app: AppHandle, label: String, on: bool) -> Result<(), Problem> {
    match popout::window(&app, &label) {
        Some(w) => w.set_fullscreen(on).map_err(Problem::failed),
        None => Ok(()),
    }
}

/// 弹出窗口是不是全屏（已经关掉了的算不是）
#[tauri::command]
fn popout_fullscreen(app: AppHandle, label: String) -> Result<bool, Problem> {
    match popout::window(&app, &label) {
        Some(w) => w.is_fullscreen().map_err(Problem::failed),
        None => Ok(false),
    }
}

/// 把弹出窗口拿到前面（最小化了的先还原）
#[tauri::command]
fn popout_focus(app: AppHandle, label: String) -> Result<(), Problem> {
    if let Some(w) = popout::window(&app, &label) {
        let _ = w.unminimize();
        w.set_focus().map_err(Problem::failed)?;
    }
    Ok(())
}

/// 关掉弹出窗口
#[tauri::command]
fn popout_close(app: AppHandle, label: String) -> Result<(), Problem> {
    match popout::window(&app, &label) {
        Some(w) => w.destroy().map_err(Problem::failed),
        None => Ok(()),
    }
}

#[tauri::command]
async fn switch_to_host(app: AppHandle) -> Result<(), Problem> {
    let a = app.clone();
    app.run_on_main_thread(move || set_mode(&a, Mode::Host))
        .map_err(Problem::failed)
}

// ───────────────────────────── 入口 ─────────────────────────────

pub fn run() {
    // Windows：安装程序给卸载程序的环境变量不往下传（见 winsvc::forget_installer_env）
    #[cfg(windows)]
    winsvc::forget_installer_env();
    // Windows：安装包和服务管理器用的命令行（--install、--service、--agent……）
    #[cfg(windows)]
    if let Some(code) = winsvc::cli() {
        std::process::exit(code);
    }
    init_logging();
    // Windows：为所有用户安装的 Airhand 由系统服务运行被控端；仅为当前用户安装的（没有系统服务）和调试构建直接运行时
    // 一样，被控端在本进程里运行。发布版没有安装时不能运行，提示运行安装程序
    #[cfg(windows)]
    let service_mode = winsvc::installed_here();
    #[cfg(windows)]
    let user_install = !service_mode && winsvc::user::installed_here();
    #[cfg(all(windows, not(debug_assertions)))]
    if !service_mode && !user_install {
        winsvc::show_not_installed();
        return;
    }
    // 这台电脑（被别的用户）为所有用户安装了 Airhand：被控端在它的系统服务里运行，仅为当前用户安装的这一份让开
    // （否则两个被控端同时运行）。在占用单实例之前检查：登录时两份一起启动的话，不能挡住为所有用户安装的那份托盘
    #[cfg(windows)]
    if user_install && winsvc::service_present() {
        winsvc::user::step_aside();
        return;
    }
    // 被控端在本进程里运行：双显卡电脑上桌面复制要找对显卡，在用到 DXGI 之前（见 host::capture::ignore_gpu_preference）
    #[cfg(windows)]
    if !service_mode {
        host::capture::ignore_gpu_preference();
    }
    #[cfg(not(windows))]
    let service_mode = false;
    let Some(instance) = single_instance::acquire() else {
        // 已经有一个实例在运行（托盘里），直接退出
        return;
    };
    // 配置文件由本进程维护（Windows 安装版由服务里的被控端维护，见 winsvc::agent）：
    // 不存在时按默认配置生成，运行中被删掉了也会重新生成
    if !service_mode {
        if let Err(e) = config::ensure_file() {
            warn!("can't create the configuration file: {e:#}");
        }
        config::keep_file();
    }
    // 仅为当前用户安装：按 config.toml 设置登录时自动启动（手动改过配置文件的话以文件为准，也顺带更新程序的位置）
    #[cfg(windows)]
    let user_autostart = user_install.then(|| {
        let on = config::settings().autostart;
        if let Err(e) = winsvc::user::apply_autostart(on) {
            warn!("can't apply the autostart setting: {e:#}");
        }
        on
    });
    enable_dpi_awareness();
    // 先清理上次残留的目录，再创建本次的（同一个前缀）
    ephemeral::sweep_stale();
    #[cfg(target_os = "macos")]
    if let Err(e) = host::mic::cleanup_stale() {
        warn!("removing a stale virtual microphone: {e}");
    }
    #[cfg(target_os = "linux")]
    ephemeral::isolate_caches();
    // Linux：上次被强制结束时留下的剪贴板文件挂载点（FUSE）
    #[cfg(target_os = "linux")]
    clipfiles::linux::sweep_stale();

    let hub = status::new_hub();
    #[cfg(windows)]
    hub.send_modify(|s| s.user_autostart = user_autostart);
    #[cfg(target_os = "macos")]
    clipfiles::macos::init(&hub);
    let app = tauri::Builder::default()
        .manage(AppState {
            hub: hub.clone(),
            services: Mutex::default(),
            host_code: HostCode::default(),
            console_opening: AtomicBool::new(false),
            webview_dirs: Mutex::default(),
            service_mode,
            console: Default::default(),
        })
        // 控制台的麦克风（把本机的麦克风发给对方，见 client/src/lib/mic.ts）：用户在连接里打开了才会用到，
        // 直接允许，不再弹 WebView 自己的询问（控制台的数据目录是临时的，不然每次打开控制台都要问一遍）
        .on_permission_request(|_, kind| match kind {
            PermissionKind::Microphone => PermissionResponse::Allow,
            _ => PermissionResponse::Default,
        })
        .invoke_handler(tauri::generate_handler![
            console_url,
            set_language,
            clipboard_text,
            set_fullscreen,
            window_fullscreen,
            popout_set_fullscreen,
            popout_fullscreen,
            popout_focus,
            popout_close,
            switch_to_host,
            devices_load,
            devices_save,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            if tray::available() {
                tray::create(&handle)?;
            }
            #[cfg(target_os = "linux")]
            exit_on_signals(handle.clone());
            // Windows：安装包升级、卸载这一份 Airhand 时让托盘退出（见 winsvc::watch_quit）
            #[cfg(windows)]
            {
                let h = handle.clone();
                winsvc::watch_quit(move || h.exit(0));
            }
            // 默认以被控端启动
            let hosting = if service_mode {
                start_service_link(&handle, &hub)
            } else {
                let code = app.state::<AppState>().host_code.clone();
                Hosting::InProcess(HostService::start(hub.clone(), code))
            };
            app.state::<AppState>().services.lock().unwrap().host = Some(hosting);
            // 仅调试构建：方便开发时直接进入控制端
            let dev_controller =
                cfg!(debug_assertions) && std::env::var_os("AIRHAND_DEV_CONTROLLER").is_some();
            if dev_controller {
                set_mode(&handle, Mode::Controller);
            }
            #[cfg(target_os = "macos")]
            {
                // 直接进入控制端时用不到被控端的权限，切回被控端时再请求
                if !dev_controller {
                    host::macos::request_permissions();
                }
                watch_permissions(hub.clone());
                login_item_startup(hub.clone());
            }

            // 状态（或界面语言）变化 → 刷新托盘 / 通知控制台
            let mut rx = hub.subscribe();
            tauri::async_runtime::spawn(async move {
                let mut lang = None;
                loop {
                    let st = rx.borrow_and_update().clone();
                    tray::refresh(&handle, &st);
                    let cur = i18n::current();
                    if lang.replace(cur) != Some(cur) {
                        if let Some(w) = handle.get_webview_window(CONSOLE) {
                            let _ = w.set_title(&console_title());
                        }
                        let _ = handle.emit_to(CONSOLE, "lang-changed", cur);
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == CONSOLE && matches!(event, WindowEvent::Destroyed) {
                on_console_destroyed(window.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("can't initialize Tauri");
    // 单实例一直占到进程退出。仅为当前用户安装时交给下面的检查：运行中别的用户为所有用户安装了 Airhand 的话，
    // 这一份让开，那时提前放开
    #[cfg(windows)]
    let instance = if user_install {
        watch_service_installed(app.handle().clone(), instance);
        None
    } else {
        Some(instance)
    };
    let _instance = instance;
    // macOS：和其它平台一样启动后只有托盘（菜单栏）图标，程序坞里没有图标；打开控制台时才显示。
    // 打包的 .app 在 Info.plist 里声明了 LSUIElement，这里是给直接运行的可执行文件用的
    #[cfg(target_os = "macos")]
    let app = {
        let mut app = app;
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);
        app
    };

    app.run(|app, event| match event {
        // 托盘程序：关闭所有窗口时不退出
        RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        RunEvent::Exit => {
            EXITING.store(true, Ordering::Release);
            let state = app.state::<AppState>();
            {
                let mut sv = state.services.lock().unwrap();
                sv.host = None;
                sv.controller = None;
            }
            // 还有远程控制就当它结束了：先锁屏（见 host::ending），
            // 再让虚拟显示器恢复原样（被控端在本进程里运行时；没用过它就什么也不做）
            host::ending::shutdown();
            host::virtual_display::shutdown();
            popout::close_all(app);
            if let Some(w) = app.get_webview_window(CONSOLE) {
                let _ = w.destroy();
            }
            let dirs: Vec<PathBuf> = std::mem::take(&mut *state.webview_dirs.lock().unwrap());
            for d in dirs {
                ephemeral::remove_with_retry(&d, Duration::from_secs(3));
            }
            #[cfg(target_os = "macos")]
            if let Err(e) = clipfiles::macos::shutdown() {
                warn!("cleaning up received files: {e}");
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Err(e) = host::mic::shutdown() {
                warn!("stopping the virtual microphone: {e}");
            }
            // Linux：对方复制的文件所在的 FUSE 文件系统。挂载在静态变量里，退出时不会自己卸
            #[cfg(target_os = "linux")]
            clipfiles::linux::shutdown();
            // Linux：托盘图标、GStreamer 缓存所在的目录（内存盘 XDG_RUNTIME_DIR 里），退出时删掉
            #[cfg(target_os = "linux")]
            ephemeral::remove_with_retry(&ephemeral::process_dir(), Duration::from_secs(1));
        }
        _ => {}
    });
}

/// Windows 安装版：被控端在系统服务里运行。打开 Airhand 时服务没有运行就启动它；服务停止后托盘也退出。
#[cfg(windows)]
fn start_service_link(app: &AppHandle, hub: &StatusHub) -> Hosting {
    hub.send_modify(|s| {
        s.service = Some(status::ServiceStatus {
            connected: false,
            running: false,
            elsewhere: false,
            autostart: config::settings().autostart,
        })
    });
    std::thread::spawn(|| {
        if let Err(e) = winsvc::start() {
            warn!("can't start the Airhand service: {e:#}");
        }
    });
    // 剪贴板里的文件：替服务里的被控端（SYSTEM）放进剪贴板、以登录的用户的身份读文件（见 winsvc::clip）
    // 任务一直跑到进程退出（不等它）
    drop(winsvc::clip::start_tray());
    let app = app.clone();
    Hosting::Service(winsvc::ipc::AgentLink::start(hub.clone(), move || {
        app.exit(0)
    }))
}

/// Windows 仅为当前用户安装：每隔一会儿看看这台电脑是不是（被别的用户）为所有用户安装了 Airhand。
/// 装了的话这一份让开（不能两个被控端同时运行），先把占着的、看得到的东西都放掉，再问用户要不要卸载这一份，然后退出：
/// * 停下本进程里的被控端和控制端，不再写配置文件；
/// * 关掉控制台、弹出窗口，拿掉托盘图标：被控端停了，菜单里的控制码、连接状态都不对了；
/// * 放开单实例（`instance`）：问的时候为所有用户安装的那份托盘就能打开了（例如那边安装程序的完成页上点"运行 Airhand"）
#[cfg(windows)]
fn watch_service_installed(app: AppHandle, instance: single_instance::Guard) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(15));
            if winsvc::service_present() {
                break;
            }
        }
        info!("Airhand got installed for all users, this copy steps aside");
        // 关掉控制台时不要切回被控端（见 on_console_destroyed）
        EXITING.store(true, Ordering::Release);
        {
            let state = app.state::<AppState>();
            let mut sv = state.services.lock().unwrap();
            sv.host = None;
            sv.controller = None;
        }
        config::freeze();
        // 窗口和托盘图标要在主线程上关，关完了再往下走（主线程卡住的话最多等几秒）
        let (closed, wait) = std::sync::mpsc::channel();
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            popout::close_all(&handle);
            if let Some(w) = handle.get_webview_window(CONSOLE) {
                let _ = w.destroy();
            }
            tray::remove(&handle);
            let _ = closed.send(());
        });
        let _ = wait.recv_timeout(Duration::from_secs(5));
        instance.release();
        winsvc::user::step_aside();
        app.exit(0);
    });
}

#[cfg(not(windows))]
fn start_service_link(_app: &AppHandle, _hub: &StatusHub) -> Hosting {
    unreachable!("the host only runs as a service on Windows")
}

/// macOS：用户在系统设置里授予 / 收回权限、打开 / 关掉登录时自动启动时刷新托盘菜单
/// （菜单里有未授权的提示、登录时自动启动的勾选）。
#[cfg(target_os = "macos")]
fn watch_permissions(hub: StatusHub) {
    tauri::async_runtime::spawn(async move {
        let mut last = host::macos::permissions();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let now = host::macos::permissions();
            if now != last {
                info!(
                    screen = now.screen,
                    input = now.input,
                    "macOS permissions changed"
                );
                last = now;
                hub.send_modify(|_| {});
            }
            // 问系统要几毫秒到几十毫秒（XPC），不占用异步运行时的线程
            let Ok(item) = tokio::task::spawn_blocking(login_item::state).await else {
                continue;
            };
            let before = hub.borrow().login_item;
            // 还没查过（启动时由 login_item_startup 查）或者没变：不动
            if before.is_none() || item == before {
                continue;
            }
            info!(state = ?item, "open at login changed in System Settings");
            if let Some(s) = item {
                record_login_item(s);
            }
            hub.send_modify(|st| st.login_item = item);
        }
    });
}

/// macOS：启动时查一下登录时自动启动的状态。第一次从"应用程序"文件夹运行时（config.toml 里还没有 `autostart`）
/// 和 Windows 一样默认打开，只做这一次：之后用户在菜单或者系统设置里关掉了就一直关着（见 [`login_item`]）。
#[cfg(target_os = "macos")]
fn login_item_startup(hub: StatusHub) {
    use login_item::State;
    std::thread::spawn(move || {
        let mut state = login_item::state();
        if config::autostart_default_pending() {
            match state {
                Some(State::Off) => match login_item::set(true, false) {
                    Ok(s) => {
                        info!(state = ?s, "open at login: on by default (first run from the Applications folder)");
                        record_login_item(s);
                        state = Some(s);
                    }
                    // 没记下来：下次启动再试
                    Err(e) => warn!("{e}"),
                },
                // 已经登记过（例如删掉重装）：照现在的记下来，不改
                Some(s @ (State::On | State::NeedsApproval)) => record_login_item(s),
                // 不是 App、还不在"应用程序"文件夹里：放进去之后第一次运行时再做
                Some(State::NotInstalled) | None => {}
            }
        }
        hub.send_modify(|st| st.login_item = state);
    });
}

/// macOS：菜单里的"卸载 Airhand…"。确认之后停掉被控端和控制端、配置文件不再写入，再按顺序清理（见 [`uninstall`]），
/// 最后告诉用户结果并退出
#[cfg(target_os = "macos")]
fn uninstall_app(app: &AppHandle) {
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    if !uninstall::confirm(mtm) {
        return;
    }
    info!("uninstalling");
    EXITING.store(true, Ordering::Release);
    let state = app.state::<AppState>();
    {
        // 正在控制本机的连接断开，控制台的会话也断开
        let mut sv = state.services.lock().unwrap();
        sv.host = None;
        sv.controller = None;
    }
    if let Some(w) = app.get_webview_window(CONSOLE) {
        let _ = w.destroy();
    }
    // 还没完全停下的被控端不能再把配置文件写回去，删掉也不再重新生成
    config::freeze();
    // 菜单栏图标要在删偏好设置之前拿掉（见 tray::remove）
    tray::remove(app);
    let plan = uninstall::Plan {
        bundle: login_item::bundle(),
        bundle_id: config::BUNDLE_ID.into(),
        exe_name: std::env::current_exe()
            .ok()
            .and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "airhand".into()),
        config_dir: config::path().and_then(|p| Some(p.parent()?.to_path_buf())),
    };
    let problems = uninstall::run(&plan);
    uninstall::done(mtm, &problems);
    app.exit(0);
}

/// macOS：把登录时自动启动的状态记进 config.toml（只是记录，以系统设置为准）
#[cfg(target_os = "macos")]
fn record_login_item(s: login_item::State) {
    let on = match s {
        login_item::State::On => true,
        login_item::State::Off | login_item::State::NeedsApproval => false,
        login_item::State::NotInstalled => return,
    };
    if let Err(e) = config::save_autostart(on) {
        warn!("can't save the open at login setting: {e:#}");
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AIRHAND_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn,quinn_proto=warn"));
    // 只输出到 stderr，绝不写日志文件
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Linux：收到 SIGTERM（注销/关机）、SIGINT（终端里按 Ctrl+C）、SIGHUP 时正常退出，照样清理临时目录。
///
/// 信号处理函数里只做一件事：往管道里写一个字节（异步信号安全），由单独的线程读到后通知事件循环退出。
#[cfg(target_os = "linux")]
fn exit_on_signals(app: AppHandle) {
    use std::sync::atomic::{AtomicI32, Ordering};
    static PIPE: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_signal(_: libc::c_int) {
        let fd = PIPE.load(Ordering::Relaxed);
        if fd >= 0 {
            unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
        }
    }

    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    PIPE.store(fds[1], Ordering::Relaxed);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        loop {
            match unsafe { libc::read(fds[0], b.as_mut_ptr().cast(), 1) } {
                1 => break,
                -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                    continue;
                }
                _ => return,
            }
        }
        info!("received an exit signal");
        app.exit(0);
    });
}

pub(crate) fn enable_dpi_awareness() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        // 清单中已声明；这里再设置一次以防万一（已设置时会返回错误，忽略即可）
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
