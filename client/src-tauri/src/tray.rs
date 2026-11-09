//! 系统托盘：图标 + 右键菜单（模式、服务器状态、控制码、切换模式、开机自动启动 / macOS 的登录时自动启动、
//! 语言、退出）。

use std::sync::OnceLock;

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Wry};

use crate::i18n::{self, Lang};
use crate::status::{AppStatus, Mode, ServiceStatus};
use airhand_link::{Problem, ServerState};

const TRAY_ID: &str = "airhand";
static ICON_PNG: &[u8] = include_bytes!("../icons/tray.png");

fn icon_online() -> Image<'static> {
    static IMG: OnceLock<Image<'static>> = OnceLock::new();
    IMG.get_or_init(|| Image::from_bytes(ICON_PNG).expect("tray icon").to_owned())
        .clone()
}

/// 未连接时使用灰色图标（运行时由彩色图标去色生成，不需要额外资源文件）。
fn icon_offline() -> Image<'static> {
    static IMG: OnceLock<Image<'static>> = OnceLock::new();
    IMG.get_or_init(|| {
        let src = icon_online();
        let mut rgba = src.rgba().to_vec();
        for px in rgba.as_chunks_mut::<4>().0 {
            let l = (px[0] as u32 * 30 + px[1] as u32 * 59 + px[2] as u32 * 11) / 100;
            let l = (l * 3 / 4 + 40).min(255) as u8;
            px[0] = l;
            px[1] = l;
            px[2] = l;
            px[3] = (px[3] as u32 * 85 / 100) as u8;
        }
        Image::new_owned(rgba, src.width(), src.height())
    })
    .clone()
}

/// 能否显示托盘图标。
///
/// Linux 的托盘图标依赖 AppIndicator 库（运行时动态加载），没有安装时 tray-icon 会直接 panic
/// （发布版 panic 即退出），所以先检查一下：没有的话不显示托盘，被控端照常工作，控制码写在日志里。
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        let names = [
            c"libayatana-appindicator3.so.1",
            c"libappindicator3.so.1",
            c"libayatana-appindicator3.so",
            c"libappindicator3.so",
        ];
        let found = names.iter().any(|n| unsafe {
            let h = libc::dlopen(n.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if !h.is_null() {
                libc::dlclose(h);
            }
            !h.is_null()
        });
        if !found {
            tracing::warn!(
                "AppIndicator library not found, can't show the tray icon (install libayatana-appindicator3-1); the control code is logged below"
            );
        }
        found
    }
    #[cfg(not(target_os = "linux"))]
    true
}

pub fn create(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let st = AppStatus::default();
    let builder = TrayIconBuilder::with_id(TRAY_ID);
    // Linux 的 AppIndicator 只接受图标文件：放到单独的临时目录里，退出时整个删掉
    #[cfg(target_os = "linux")]
    let builder = builder.temp_dir_path(crate::ephemeral::process_dir().join("tray"));
    builder
        .icon(icon_offline())
        .tooltip("Airhand")
        .menu(&build_menu(app, &st)?)
        // macOS 的菜单栏图标习惯上左键就弹出菜单；Windows / Linux 左键在控制端模式下打开控制台
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| crate::on_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            // macOS 上左键弹出的是菜单（菜单里有"打开控制台"），不再另外打开控制台
            if cfg!(target_os = "macos") {
                return;
            }
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::on_tray_left_click(tray.app_handle());
            }
        })
        .build(app)
}

fn label(app: &AppHandle, text: impl AsRef<str>) -> tauri::Result<MenuItem<Wry>> {
    MenuItem::new(app, text, false, None::<&str>)
}

fn server_line(server: &ServerState) -> String {
    match server {
        ServerState::Connecting => tr!("服务器：连接中…", "Server: connecting…"),
        ServerState::Connected => tr!("服务器：已连接 ●", "Server: connected ●"),
        // 只显示原因的种类；详细的技术信息在日志里
        ServerState::Disconnected { reason } => server_problem(reason),
    }
}

fn build_menu(app: &AppHandle, st: &AppStatus) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    menu.append(&label(
        app,
        format!("Airhand  v{}", crate::config::VERSION),
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    match st.service {
        Some(svc) => service_items(app, &menu, st, svc)?,
        None => {
            let mode = match st.mode {
                Mode::Host => tr!("当前模式：被控端", "Mode: Host"),
                Mode::Controller => tr!("当前模式：控制端", "Mode: Controller"),
            };
            menu.append(&label(app, mode)?)?;
            if st.mode == Mode::Host {
                host_items(app, &menu, st)?;
            } else {
                menu.append(&label(app, server_line(&st.controller))?)?;
            }
            menu.append(&PredefinedMenuItem::separator(app)?)?;
            match st.mode {
                Mode::Host => {
                    menu.append(&item(
                        app,
                        "to_controller",
                        tr!("切换为控制端", "Switch to controller"),
                    )?)?;
                }
                Mode::Controller => {
                    menu.append(&item(app, "console", tr!("打开控制台", "Open console"))?)?;
                    menu.append(&item(
                        app,
                        "to_host",
                        tr!("切换为被控端", "Switch to host"),
                    )?)?;
                }
            }
            #[cfg(target_os = "macos")]
            if let Some(s) = st.login_item {
                menu.append(&login_item_entry(app, s)?)?;
            }
        }
    }
    menu.append(&language_menu(app)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    // macOS 的 App 没有卸载程序：在这里一次清理干净（见 crate::uninstall）。直接运行可执行文件（开发时）不提供
    #[cfg(target_os = "macos")]
    if crate::login_item::bundle().is_some() {
        menu.append(&item(
            app,
            "uninstall",
            tr!("卸载 Airhand…", "Uninstall Airhand…"),
        )?)?;
    }
    menu.append(&item(app, "quit", tr!("退出", "Quit"))?)?;
    Ok(menu)
}

/// Windows 安装版：被控端在系统服务里一直运行，控制台随时可以打开
fn service_items(
    app: &AppHandle,
    menu: &Menu<Wry>,
    st: &AppStatus,
    svc: ServiceStatus,
) -> tauri::Result<()> {
    if svc.connected {
        host_items(app, menu, st)?;
    } else if !svc.running {
        menu.append(&label(
            app,
            tr!("被控端服务：未运行", "Host service: not running"),
        )?)?;
        menu.append(&item(
            app,
            "start_service",
            tr!("启动被控端服务", "Start the host service"),
        )?)?;
    } else if svc.elsewhere {
        menu.append(&label(
            app,
            tr!(
                "被控端在屏幕前的另一个用户那里运行",
                "The host runs for the other user at the screen"
            ),
        )?)?;
    } else {
        menu.append(&label(
            app,
            tr!("被控端服务：正在启动…", "Host service: starting…"),
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&item(app, "console", tr!("打开控制台", "Open console"))?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        "autostart",
        tr!("开机自动启动", "Start with Windows"),
        svc.connected,
        svc.autostart,
        None::<&str>,
    )?)?;
    Ok(())
}

/// macOS：登录时自动启动（见 [`crate::login_item`]）。不在"应用程序"文件夹里时不能点，说明怎么做；
/// 要在系统设置里允许时，点一下打开系统设置
#[cfg(target_os = "macos")]
fn login_item_entry(
    app: &AppHandle,
    s: crate::login_item::State,
) -> tauri::Result<CheckMenuItem<Wry>> {
    use crate::login_item::State;
    let text = match s {
        State::NotInstalled => tr!(
            "登录时自动启动（先把 Airhand 拖进“应用程序”文件夹）",
            "Open at login (move Airhand to the Applications folder first)"
        ),
        State::NeedsApproval => tr!(
            "登录时自动启动（要在系统设置里允许，点击打开）",
            "Open at login (allow it in System Settings, click to open)"
        ),
        State::On | State::Off => tr!("登录时自动启动", "Open at login"),
    };
    CheckMenuItem::with_id(
        app,
        "autostart",
        text,
        s != State::NotInstalled,
        s == State::On,
        None::<&str>,
    )
}

/// 被控端：服务器状态、控制码、正在控制本机的控制端
fn host_items(app: &AppHandle, menu: &Menu<Wry>, st: &AppStatus) -> tauri::Result<()> {
    menu.append(&label(app, server_line(&st.host.server))?)?;
    {
        #[cfg(target_os = "macos")]
        permission_items(app, menu)?;
        if let (ServerState::Connected, Some(code)) = (&st.host.server, st.host.code) {
            let code = airhand_proto::format_code(code);
            menu.append(&label(app, tr!("控制码：{code}", "Control code: {code}"))?)?;
            menu.append(&item(
                app,
                "copy_code",
                tr!("复制控制码", "Copy control code"),
            )?)?;
            menu.append(&item(
                app,
                "new_code",
                tr!(
                    "更换控制码（旧码立即失效，配对也全部作废）",
                    "Change control code (old code stops working, all pairings are revoked)"
                ),
            )?)?;
            pairing_items(app, menu, st)?;
        }
        if !st.host.controllers.is_empty() {
            menu.append(&PredefinedMenuItem::separator(app)?)?;
            menu.append(&label(
                app,
                format!("⚠ {}", controlled_by(st.host.controllers.len())),
            )?)?;
            for c in st.host.controllers.iter().take(5) {
                let name = if c.peer.name.is_empty() {
                    tr!("未知设备", "Unknown device")
                } else {
                    c.peer.name.clone()
                };
                // 安全码：控制端显示的是同一个，两边的人可以对一下（见 airhand_proto::handshake）
                let line = match &c.safety {
                    Some(s) => tr!(
                        "    {name} ({ip}) · 安全码 {s}",
                        "    {name} ({ip}) · safety code {s}",
                        ip = c.peer.ip
                    ),
                    None => format!("    {name} ({})", c.peer.ip),
                };
                menu.append(&label(app, line)?)?;
            }
            menu.append(&item(
                app,
                "kick_all",
                tr!("断开所有控制端", "Disconnect all controllers"),
            )?)?;
        }
    }
    Ok(())
}

/// 配对（见 [`crate::host::gate`]）：配对码，暂停了的话提示换一个；配对过的控制端可以全部忘掉。
/// 不要配对码（config.toml 的 `access = "open"`）时什么也不显示
fn pairing_items(app: &AppHandle, menu: &Menu<Wry>, st: &AppStatus) -> tauri::Result<()> {
    let Some(pairing) = &st.host.pairing else {
        return Ok(());
    };
    if st.host.pairing_locked {
        menu.append(&item(
            app,
            "new_pairing",
            tr!(
                "⚠ 有人多次输错配对码，配对已暂停（点击换一个配对码）",
                "⚠ Pairing paused after too many wrong codes (click for a new pairing code)"
            ),
        )?)?;
    } else {
        let p = format!("{} {}", &pairing[..3], &pairing[3..]);
        menu.append(&label(
            app,
            tr!(
                "配对码：{p}（第一次连接本机时要输入）",
                "Pairing code: {p} (needed the first time a controller connects)"
            ),
        )?)?;
        menu.append(&item(app, "copy_pairing", tr!("复制配对码", "Copy pairing code"))?)?;
        menu.append(&item(
            app,
            "new_pairing",
            tr!("更换配对码", "Change pairing code"),
        )?)?;
    }
    if st.host.paired > 0 {
        let n = st.host.paired;
        menu.append(&item(
            app,
            "forget_paired",
            tr!(
                "忘掉已配对的 {n} 个控制端（下次要重新输入配对码）",
                "Forget {n} paired controller(s) (they'll need the pairing code again)"
            ),
        )?)?;
    }
    Ok(())
}

/// macOS：没有授予的权限各占一行，点击打开系统设置里对应的页面。
#[cfg(target_os = "macos")]
fn permission_items(app: &AppHandle, menu: &Menu<Wry>) -> tauri::Result<()> {
    let p = crate::host::macos::permissions();
    if !p.screen {
        menu.append(&item(
            app,
            "perm_screen",
            tr!(
                "⚠ 需要“屏幕与系统录音”权限（点击打开设置）",
                "⚠ Needs Screen & System Audio Recording permission (click to open Settings)"
            ),
        )?)?;
    }
    if !p.input {
        menu.append(&item(
            app,
            "perm_input",
            tr!(
                "⚠ 需要“辅助功能”权限才能操作本机（点击打开设置）",
                "⚠ Needs Accessibility permission to be controlled (click to open Settings)"
            ),
        )?)?;
    }
    if !p.screen || !p.input {
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    Ok(())
}

fn item(app: &AppHandle, id: &str, text: String) -> tauri::Result<MenuItem<Wry>> {
    MenuItem::with_id(app, id, text, true, None::<&str>)
}

/// 语言子菜单。标题中英并列，看不懂当前语言的人也能找到；选项各用自己的语言书写。
fn language_menu(app: &AppHandle) -> tauri::Result<Submenu<Wry>> {
    let cur = i18n::current();
    let title = if cur == Lang::Zh {
        "语言 / Language"
    } else {
        "Language / 语言"
    };
    let zh = CheckMenuItem::with_id(
        app,
        "lang_zh",
        "简体中文",
        true,
        cur == Lang::Zh,
        None::<&str>,
    )?;
    let en = CheckMenuItem::with_id(
        app,
        "lang_en",
        "English",
        true,
        cur == Lang::En,
        None::<&str>,
    )?;
    Submenu::with_items(app, title, true, &[&zh, &en])
}

fn server_problem(p: &Problem) -> String {
    match p {
        Problem::ServerUnreachable { .. } => tr!("服务器：无法连接", "Server: can't connect"),
        Problem::CertMismatch => tr!("服务器：证书指纹不匹配", "Server: certificate mismatch"),
        Problem::Incompatible { server, client } if server > client => {
            tr!(
                "服务器：版本不兼容，请更新 Airhand",
                "Server: incompatible, please update Airhand"
            )
        }
        Problem::Incompatible { .. } => tr!(
            "服务器：版本不兼容，请更新服务器",
            "Server: incompatible, please update the server"
        ),
        _ => tr!("服务器：连接已断开", "Server: disconnected"),
    }
}

fn controlled_by(n: usize) -> String {
    match n {
        1 => tr!("正在被 1 个控制端控制", "Being controlled by 1 controller"),
        _ => tr!(
            "正在被 {n} 个控制端控制",
            "Being controlled by {n} controllers"
        ),
    }
}

pub fn refresh(app: &AppHandle, st: &AppStatus) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    if let Ok(menu) = build_menu(app, st) {
        let _ = tray.set_menu(Some(menu));
    }
    // Windows 安装版：没连上服务里的被控端时，被控端的状态没有意义
    let host_known = st.service.is_none_or(|s| s.connected);
    let online = if st.shows_host() {
        host_known && st.host.server == ServerState::Connected
    } else {
        st.controller == ServerState::Connected
    };
    let _ = tray.set_icon(Some(if online {
        icon_online()
    } else {
        icon_offline()
    }));
    let code_line = |code: u32| {
        let code = airhand_proto::format_code(code);
        tr!("控制码 {code}", "Control code {code}")
    };
    let (shown, server, code) = if st.shows_host() {
        (Mode::Host, &st.host.server, st.host.code)
    } else {
        (Mode::Controller, &st.controller, None)
    };
    let controllers = st.host.controllers.len();
    let tip = match (shown, server, code) {
        (Mode::Host, _, _) if !host_known => match st.service {
            Some(s) if s.running => tr!("Airhand · 被控端", "Airhand · Host"),
            _ => tr!(
                "Airhand · 被控端服务未运行",
                "Airhand · Host service not running"
            ),
        },
        (Mode::Host, ServerState::Connected, Some(code)) if controllers > 0 => {
            format!(
                "Airhand · {}\n{}",
                controlled_by(controllers),
                code_line(code)
            )
        }
        (Mode::Host, ServerState::Connected, Some(code)) => {
            format!("Airhand · {}\n{}", tr!("被控端", "Host"), code_line(code))
        }
        (Mode::Host, ServerState::Connected, None) => tr!("Airhand · 被控端", "Airhand · Host"),
        (Mode::Controller, ServerState::Connected, _) => {
            tr!("Airhand · 控制端", "Airhand · Controller")
        }
        (_, ServerState::Connecting, _) => tr!(
            "Airhand · 正在连接服务器…",
            "Airhand · Connecting to server…"
        ),
        (_, ServerState::Disconnected { .. }, _) => tr!(
            "Airhand · 未连接服务器",
            "Airhand · Not connected to server"
        ),
    };
    let _ = tray.set_tooltip(Some(tip));
}
