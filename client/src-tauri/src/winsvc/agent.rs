//! `--agent`：系统服务在控制台会话里以 SYSTEM 身份启动的被控端（见 [`super::service`]）。
//!
//! 和直接运行时进程内的被控端是同一套代码（[`HostService`]），区别只是没有托盘：
//! 状态和命令经命名管道交给各个托盘（[`super::ipc`]）。服务通过继承来的事件通知它退出；
//! 服务进程意外退出时它也退出。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::{
    CreateMutexW, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForMultipleObjects,
};
use windows::core::w;

use crate::host::HostService;
use crate::{config, status};

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

pub fn run(args: &[String]) -> i32 {
    super::init_debug_logging();
    crate::enable_dpi_awareness();

    // 同一时刻只能有一个被控端（服务保证这一点；这里防止手动运行的 --agent 和服务里的同时注册）
    let _single = unsafe {
        match CreateMutexW(None, true, w!("Global\\Airhand.Host")) {
            Ok(h) if GetLastError() != ERROR_ALREADY_EXISTS => h,
            _ => {
                warn!("another Airhand host is already running");
                return 1;
            }
        }
    };

    // 服务给的退出事件、服务进程本身：任何一个有信号就退出
    let mut handles: Vec<HANDLE> = Vec::new();
    if let Some(h) = arg(args, "--stop-event").and_then(|v| v.parse::<usize>().ok()) {
        handles.push(HANDLE(h as *mut core::ffi::c_void));
    }
    if let Some(pid) = arg(args, "--parent").and_then(|v| v.parse::<u32>().ok()) {
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(h) => handles.push(h),
            Err(e) => {
                warn!("the service process is gone ({e}), exiting");
                return 1;
            }
        }
    }

    info!(version = config::VERSION, "host started");
    // 安装版的配置文件由这里维护：不存在时按默认配置生成，运行中被删掉了也会重新生成（控制码、我的设备不丢）
    if let Err(e) = config::ensure_file() {
        warn!("can't create config.toml: {e:#}");
    }
    config::keep_file();
    let hub = status::new_hub();
    let host = HostService::start(hub.clone());
    let agent = super::ipc::Agent {
        cmds: host.commands(),
        autostart: Arc::new(watch::Sender::new(config::settings().autostart)),
        gate: host.gate().clone(),
    };
    tauri::async_runtime::spawn(super::ipc::serve(hub, agent));

    if handles.is_empty() {
        // 手动运行（调试）：一直运行到进程被结束
        loop {
            std::thread::park();
        }
    }
    unsafe {
        WaitForMultipleObjects(&handles, false, INFINITE);
    }
    info!("stopping the host");
    // 中止被控端：断开所有会话、关闭到服务器的连接（给 QUIC 一点时间把关闭帧发出去）
    drop(host);
    std::thread::sleep(Duration::from_millis(300));
    for h in handles {
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    0
}
