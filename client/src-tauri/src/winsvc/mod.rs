//! Windows 安装版：被控端作为系统服务运行。
//!
//! 安装包（NSIS，钩子见 `windows/hooks.nsh`）把 airhand.exe 装进 Program Files，然后运行 `airhand.exe --install`
//! 注册系统服务 `Airhand`。同一个 airhand.exe 按命令行参数扮演不同的角色：
//!
//! | 参数 | 角色 | 身份 / 会话 |
//! |---|---|---|
//! | （无） | 托盘和控制台 | 登录的用户，用户自己的会话 |
//! | `--service` | 系统服务：在控制台会话里启动被控端并看护它（[`service`]） | SYSTEM，会话 0 |
//! | `--agent` | 被控端：连接服务器、采集画面和声音、注入键鼠、同步剪贴板（[`agent`]） | SYSTEM，控制台会话 |
//! | `--install` / `--uninstall` / `--stop-service` | 安装、卸载、升级时由安装包调用 | 管理员 |
//!
//! * 被控端以 SYSTEM 身份运行在用户的会话里，才能采集和操作 UAC 确认框、锁屏和登录界面（它们在安全桌面上，
//!   见 `host::desktop`）。服务跟着"控制台会话"走：切换用户、注销时把被控端挪到新的控制台会话里；
//! * 托盘经命名管道（[`ipc`]）显示被控端的状态、发送命令（断开控制端、更换控制码、开机自动启动）；
//! * 托盘和服务同进同退：打开 Airhand 时启动服务，托盘"退出"时停止服务；服务停止（或者被卸载）时托盘也退出；
//! * 开机自动启动（`config.toml` 的 `autostart`，托盘里可以切换）= 服务的启动类型为"自动" + 登录时启动托盘
//!   （`HKLM\...\Run` 里的 `Airhand`）；
//! * 卸载（`--uninstall`）删除服务和这个启动项，安装目录里的文件（包括 config.toml）由卸载程序删除。
//!
//! 服务和被控端没有控制台，日志写到 Windows 调试输出（`OutputDebugString`），用 Sysinternals 的 DebugView
//! （勾选 Capture Global Win32）查看；日志级别同样由 `AIRHAND_LOG` 控制。

mod agent;
pub mod clip;
pub mod ipc;
mod service;

/// 本进程是服务里的被控端（`--agent`，SYSTEM）
static AGENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 本进程是服务里的被控端（`--agent`）：以 SYSTEM 身份运行，剪贴板里的文件交给托盘（见 [`clip`]）
pub fn is_agent() -> bool {
    AGENT.load(std::sync::atomic::Ordering::Relaxed)
}

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE,
    HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, ENUM_SERVICE_TYPE, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfigW, QueryServiceStatus, SC_ACTION, SC_ACTION_RESTART,
    SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_ERROR,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START, SERVICE_START_PENDING,
    SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_WIN32_OWN_PROCESS, SetServiceObjectSecurity, StartServiceW,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

/// 系统服务的名称
pub const SERVICE_NAME: &str = "Airhand";
/// 开机启动托盘的注册表位置（所有用户）
const RUN_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
/// 服务的访问权限：在系统默认的基础上，允许交互登录的用户（IU）启动（RP）和停止（WP）服务——
/// 打开 Airhand 时启动被控端、在托盘里"退出"时停止它，都不需要管理员权限
const SERVICE_SDDL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";
/// `DELETE` 访问权限（删除服务）
const DELETE: u32 = 0x0001_0000;

/// 处理安装包和服务管理器使用的命令行参数。返回 `Some(退出码)` 表示已经处理完，进程应该退出；
/// `None` 表示普通启动（托盘）。
pub fn cli() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first()?.as_str();
    let result = match first {
        "--service" => return Some(service::run()),
        "--agent" => {
            AGENT.store(true, std::sync::atomic::Ordering::Relaxed);
            return Some(agent::run(&args[1..]));
        }
        "--install" => {
            init_console_logging();
            install()
        }
        "--uninstall" => {
            init_console_logging();
            uninstall()
        }
        "--stop-service" => {
            init_console_logging();
            stop_for_upgrade()
        }
        _ => return None,
    };
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{first} failed: {e:#}");
            1
        }
    })
}

// ───────────────────────────── 日志 ─────────────────────────────

fn init_console_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// 服务和被控端没有控制台：日志写到 Windows 调试输出，一条日志一次 `OutputDebugString`
fn init_debug_logging() {
    struct DebugOut(Vec<u8>);
    impl std::io::Write for DebugOut {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for DebugOut {
        fn drop(&mut self) {
            if self.0.is_empty() {
                return;
            }
            let text = format!("Airhand: {}", String::from_utf8_lossy(&self.0));
            unsafe {
                windows::Win32::System::Diagnostics::Debug::OutputDebugStringW(&HSTRING::from(text))
            };
        }
    }
    let filter = tracing_subscriber::EnvFilter::try_from_env("AIRHAND_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,quinn=warn,quinn_proto=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(|| DebugOut(Vec::new()))
        .with_ansi(false)
        .without_time()
        .try_init();
}

// ───────────────────────────── 服务管理器 ─────────────────────────────

/// 服务管理器的句柄，Drop 时关闭
struct Sc(SC_HANDLE);

impl Drop for Sc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn open_scm(access: u32) -> Result<Sc> {
    Ok(Sc(unsafe {
        OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), access)
    }
    .context("OpenSCManager")?))
}

/// 打开 Airhand 服务；不存在时返回 `Ok(None)`
fn open_service(access: u32) -> Result<Option<Sc>> {
    let scm = open_scm(SC_MANAGER_CONNECT)?;
    match unsafe { OpenServiceW(scm.0, &HSTRING::from(SERVICE_NAME), access) } {
        Ok(h) => Ok(Some(Sc(h))),
        Err(e)
            if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult()
                || e.code() == ERROR_SERVICE_MARKED_FOR_DELETE.to_hresult() =>
        {
            Ok(None)
        }
        Err(e) => Err(e).context("OpenService"),
    }
}

/// 服务的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 没有安装（或者已经被卸载）
    Missing,
    Stopped,
    Starting,
    Running,
    Stopping,
}

pub fn state() -> State {
    let Ok(Some(svc)) = open_service(SERVICE_QUERY_STATUS) else {
        return State::Missing;
    };
    let mut st = SERVICE_STATUS::default();
    if unsafe { QueryServiceStatus(svc.0, &mut st) }.is_err() {
        return State::Missing;
    }
    match st.dwCurrentState {
        SERVICE_RUNNING => State::Running,
        SERVICE_START_PENDING => State::Starting,
        SERVICE_STOP_PENDING => State::Stopping,
        SERVICE_STOPPED => State::Stopped,
        // 暂停（不支持）等
        _ => State::Running,
    }
}

/// 服务登记的程序路径（去掉引号和参数）
fn service_exe(svc: &Sc) -> Result<PathBuf> {
    unsafe {
        let mut needed = 0u32;
        match QueryServiceConfigW(svc.0, None, 0, &mut needed) {
            Err(e) if e.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {}
            Err(e) => return Err(e).context("QueryServiceConfig"),
            Ok(()) => {}
        }
        // 按 8 字节对齐分配
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let cfg = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        QueryServiceConfigW(svc.0, Some(cfg), (buf.len() * 8) as u32, &mut needed)
            .context("QueryServiceConfig")?;
        let cmd = (*cfg).lpBinaryPathName.to_string()?;
        Ok(PathBuf::from(command_exe(&cmd)))
    }
}

/// 从命令行里取出程序路径：`"C:\Program Files\Airhand\airhand.exe" --service` → `C:\Program Files\Airhand\airhand.exe`
fn command_exe(cmd: &str) -> &str {
    let cmd = cmd.trim();
    match cmd.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => cmd.split(" --").next().unwrap_or(cmd).trim(),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        _ => false,
    }
}

/// 这个 airhand.exe 是不是已经安装好了（Airhand 服务运行的就是它）。
/// 是的话被控端在服务里运行；不是的话（例如开发时直接运行的调试构建）被控端在本进程里运行。
pub fn installed_here() -> bool {
    let check = || -> Result<bool> {
        let Some(svc) = open_service(SERVICE_QUERY_CONFIG)? else {
            return Ok(false);
        };
        Ok(same_file(&service_exe(&svc)?, &std::env::current_exe()?))
    };
    check().unwrap_or_else(|e| {
        warn!("can't check the Airhand service: {e:#}");
        false
    })
}

/// 启动服务（已经在运行时什么也不做）
pub fn start() -> Result<()> {
    let svc = open_service(SERVICE_START)?.context("the Airhand service isn't installed")?;
    match unsafe { StartServiceW(svc.0, None) } {
        Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => Ok(()),
        r => r.context("StartService"),
    }
}

/// 请求停止服务（不等它停下来）
pub fn stop() -> Result<()> {
    let Some(svc) = open_service(SERVICE_STOP)? else {
        return Ok(());
    };
    let mut st = SERVICE_STATUS::default();
    match unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) } {
        Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => Ok(()),
        r => r.context("ControlService(STOP)"),
    }
}

/// 停止服务并等它停下来
fn stop_and_wait(svc: &Sc, timeout: Duration) -> Result<()> {
    let mut st = SERVICE_STATUS::default();
    match unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) } {
        Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => return Ok(()),
        r => r.context("ControlService(STOP)")?,
    }
    let deadline = Instant::now() + timeout;
    loop {
        unsafe { QueryServiceStatus(svc.0, &mut st) }.context("QueryServiceStatus")?;
        if st.dwCurrentState == SERVICE_STOPPED {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "the service didn't stop in time");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 开机自动启动：服务的启动类型（自动 / 手动）+ 登录时启动托盘。需要管理员或 SYSTEM 权限
pub fn apply_autostart(on: bool) -> Result<()> {
    let svc =
        open_service(SERVICE_CHANGE_CONFIG)?.context("the Airhand service isn't installed")?;
    let start_type = if on {
        SERVICE_AUTO_START
    } else {
        SERVICE_DEMAND_START
    };
    unsafe {
        ChangeServiceConfigW(
            svc.0,
            ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
            start_type,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    }
    .context("ChangeServiceConfig")?;
    set_run_key(on)
}

/// 登录时启动托盘（所有用户）
fn set_run_key(on: bool) -> Result<()> {
    let key = HSTRING::from(RUN_KEY);
    let name = HSTRING::from(SERVICE_NAME);
    unsafe {
        if on {
            let exe = std::env::current_exe()?;
            let value: Vec<u16> = format!("\"{}\"", exe.display())
                .encode_utf16()
                .chain([0])
                .collect();
            RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                &key,
                &name,
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            )
            .ok()
            .context("can't write the Run key")?;
        } else {
            let r = RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, &key, &name);
            if r.is_err() && r != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                r.ok().context("can't delete the Run key")?;
            }
        }
    }
    Ok(())
}

// ───────────────────────────── 安装 / 卸载 ─────────────────────────────

/// `--install`：注册（或更新）服务，按 config.toml 设置开机自动启动，然后启动服务。安装包在复制完文件后调用
fn install() -> Result<()> {
    let exe = std::env::current_exe()?;
    let bin = HSTRING::from(format!("\"{}\" --service", exe.display()));
    let display = HSTRING::from("Airhand");
    crate::config::ensure_file().context("can't create config.toml")?;
    let autostart = crate::config::settings().autostart;
    let start_type: SERVICE_START_TYPE = if autostart {
        SERVICE_AUTO_START
    } else {
        SERVICE_DEMAND_START
    };

    let scm = open_scm(SC_MANAGER_ALL_ACCESS)?;
    let name = HSTRING::from(SERVICE_NAME);
    let svc = unsafe {
        match OpenServiceW(scm.0, &name, SERVICE_ALL_ACCESS) {
            Ok(h) => {
                let svc = Sc(h);
                // 升级：服务已经存在，更新程序路径等配置
                ChangeServiceConfigW(
                    svc.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    start_type,
                    SERVICE_ERROR_NORMAL,
                    &bin,
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    w!("LocalSystem"),
                    PCWSTR::null(),
                    &display,
                )
                .context("ChangeServiceConfig")?;
                svc
            }
            Err(_) => Sc(CreateServiceW(
                scm.0,
                &name,
                &display,
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                start_type,
                SERVICE_ERROR_NORMAL,
                &bin,
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                // 不指定账户 = LocalSystem
                PCWSTR::null(),
                PCWSTR::null(),
            )
            .context("CreateService")?),
        }
    };

    unsafe {
        let mut desc: Vec<u16> = "Airhand remote desktop host: lets this computer be controlled remotely, including the sign-in screen and UAC prompts."
            .encode_utf16()
            .chain([0])
            .collect();
        let d = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(desc.as_mut_ptr()),
        };
        ChangeServiceConfig2W(svc.0, SERVICE_CONFIG_DESCRIPTION, Some(&d as *const _ as _))
            .context("ChangeServiceConfig2(DESCRIPTION)")?;

        // 服务进程意外退出时 5 秒后重启（一天之内的失败计数）
        let mut actions = [SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5000,
        }; 3];
        let fa = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 24 * 3600,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
        };
        ChangeServiceConfig2W(
            svc.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&fa as *const _ as _),
        )
        .context("ChangeServiceConfig2(FAILURE_ACTIONS)")?;

        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(SERVICE_SDDL),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .context("ConvertStringSecurityDescriptorToSecurityDescriptor")?;
        let r = SetServiceObjectSecurity(svc.0, DACL_SECURITY_INFORMATION, sd);
        LocalFree(Some(HLOCAL(sd.0)));
        r.context("SetServiceObjectSecurity")?;
    }

    set_run_key(autostart)?;
    match unsafe { StartServiceW(svc.0, None) } {
        Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => {}
        r => r.context("StartService")?,
    }
    info!(
        "the Airhand service is installed and running ({}, autostart {})",
        exe.display(),
        if autostart { "on" } else { "off" }
    );
    Ok(())
}

/// `--uninstall`：停止并删除服务、删除开机启动项，等托盘全部退出。安装目录里的文件由卸载程序删除
fn uninstall() -> Result<()> {
    if let Some(svc) = open_service(SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE)? {
        if let Err(e) = stop_and_wait(&svc, Duration::from_secs(20)) {
            warn!("{e:#}");
        }
        match unsafe { DeleteService(svc.0) } {
            Err(e) if e.code() == ERROR_SERVICE_MARKED_FOR_DELETE.to_hresult() => {}
            r => r.context("DeleteService")?,
        }
        info!("the Airhand service is removed");
    }
    set_run_key(false)?;
    wait_for_other_instances(Duration::from_secs(8));
    Ok(())
}

/// `--stop-service`：升级前停止服务（并等托盘退出），否则 airhand.exe 正在使用，无法覆盖
fn stop_for_upgrade() -> Result<()> {
    if let Some(svc) = open_service(SERVICE_STOP | SERVICE_QUERY_STATUS)? {
        stop_and_wait(&svc, Duration::from_secs(20))?;
    }
    wait_for_other_instances(Duration::from_secs(8));
    Ok(())
}

/// 等同一个 airhand.exe 的其它进程（各个会话里的托盘、被控端）退出：服务停止后它们会自己退出
fn wait_for_other_instances(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let n = other_instances();
        if n == 0 {
            return;
        }
        if Instant::now() >= deadline {
            warn!("{n} other Airhand process(es) are still running");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn other_instances() -> usize {
    let Ok(me) = std::env::current_exe() else {
        return 0;
    };
    let file_name = me
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let my_pid = unsafe { GetCurrentProcessId() };
    let mut n = 0;
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return 0;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
            if e.th32ProcessID != my_pid
                && name == file_name
                // 看不到路径（别的用户的进程）时也算上，宁可多等一会儿
                && process_path(e.th32ProcessID).is_none_or(|p| same_file(&p, &me))
            {
                n += 1;
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    n
}

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

/// 发布版没有安装时：提示用安装包安装
#[cfg_attr(debug_assertions, allow(dead_code))]
pub fn show_not_installed() {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    let text = HSTRING::from(tr!(
        "Airhand 需要安装后才能使用：请运行 Airhand 安装程序。\n\n安装后 Airhand 作为系统服务运行，锁屏、登录界面和 UAC 确认框也能被远程控制。",
        "Airhand needs to be installed: please run the Airhand installer.\n\nOnce installed, Airhand runs as a system service, so the lock screen, the sign-in screen and UAC prompts can be controlled remotely too."
    ));
    unsafe {
        MessageBoxW(None, &text, w!("Airhand"), MB_OK | MB_ICONINFORMATION);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn command_exe() {
        assert_eq!(
            super::command_exe(r#""C:\Program Files\Airhand\airhand.exe" --service"#),
            r"C:\Program Files\Airhand\airhand.exe"
        );
        assert_eq!(
            super::command_exe(r"C:\Airhand\airhand.exe --service"),
            r"C:\Airhand\airhand.exe"
        );
        assert_eq!(
            super::command_exe(r"C:\Airhand\airhand.exe"),
            r"C:\Airhand\airhand.exe"
        );
    }
}
