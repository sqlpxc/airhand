//! 托盘 ⇄ 系统服务里的被控端：命名管道，每行一条 JSON 消息。
//!
//! * 管道名带会话编号（`\\.\pipe\airhand-host-<会话>`），每个会话的托盘只连自己会话里的被控端；
//!   被控端还会核对连上来的进程是不是在同一个会话里；
//! * 只有交互登录的用户（和 SYSTEM、管理员）能连接，而且不能另开同名管道冒充被控端（见 [`PIPE_SDDL`]）；
//!   托盘还会核对管道的所有者是 SYSTEM（或管理员），普通用户的程序抢先开了同名管道也骗不了托盘；
//! * 被控端 → 托盘：[`AgentStatus`]，连上时发一次，之后有变化就发；
//! * 托盘 → 被控端：[`ToAgent`]。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::async_runtime::JoinHandle;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_NONE, FILE_WRITE_DATA,
    OPEN_EXISTING,
};
use windows::Win32::System::Pipes::GetNamedPipeClientSessionId;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::HSTRING;

use crate::host::HostCmd;
use crate::host::gate::DesktopGate;
use crate::status::{HostStatus, ServiceStatus, StatusHub};

/// 管道的访问权限：SYSTEM、管理员和创建者（OW，所有者）完全控制；交互登录的用户（IU）可以读写，
/// 但没有 `FILE_CREATE_PIPE_INSTANCE`（0x12019b = 读 + 写数据，不含 FILE_APPEND_DATA），不能再开一个同名的
/// 管道实例冒充被控端
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12019b;;;IU)";

/// 本进程所在的会话
fn my_session() -> u32 {
    let mut s = 0;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut s);
    }
    s
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\airhand-host-{session}")
}

/// 被控端 → 托盘
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub host: HostStatus,
    pub autostart: bool,
}

/// 托盘 → 被控端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToAgent {
    KickAll,
    NewCode,
    SetAutostart { on: bool },
    /// 控制台的"我的设备"：整个列表写进 config.toml（托盘没有写安装目录的权限）。
    /// 已经由托盘整理好、`code` 用那个用户的 DPAPI 加密过（见 [`crate::devices::for_file`]）
    SaveDevices { devices: Vec<crate::devices::Device> },
    /// 换一个配对码（暂停了的配对随之恢复）
    NewPairingCode,
    /// 忘掉所有配对过的控制端
    ForgetPaired,
    /// 控制台的身份（那个用户的 DPAPI 加密过，见 [`crate::config::controller_identity`]）写进 config.toml
    SaveControllerKey { key: String },
}

async fn send_line<W: AsyncWrite + Unpin>(w: &mut W, msg: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    Ok(())
}

// ───────────────────────────── 被控端一侧 ─────────────────────────────

/// 管道的安全属性（整个进程期间都要用，不释放）
struct Attrs(SECURITY_ATTRIBUTES);
unsafe impl Send for Attrs {}
unsafe impl Sync for Attrs {}

fn attrs() -> Result<&'static Attrs> {
    static ATTRS: std::sync::OnceLock<Attrs> = std::sync::OnceLock::new();
    if let Some(a) = ATTRS.get() {
        return Ok(a);
    }
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(PIPE_SDDL),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .context("ConvertStringSecurityDescriptorToSecurityDescriptor")?;
    Ok(ATTRS.get_or_init(|| {
        Attrs(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        })
    }))
}

fn create(name: &str, first: bool) -> Result<NamedPipeServer> {
    let attrs = attrs()?;
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, &attrs.0 as *const _ as *mut _)
    }
    .with_context(|| format!("can't create {name}"))?;
    Ok(server)
}

/// 被控端那一侧：发给被控端的命令、开机自动启动的设置、配对
#[derive(Clone)]
pub struct Agent {
    pub cmds: mpsc::UnboundedSender<HostCmd>,
    pub autostart: Arc<watch::Sender<bool>>,
    pub gate: Arc<DesktopGate>,
}

/// 在本会话的管道上接受托盘的连接
pub async fn serve(hub: StatusHub, agent: Agent) {
    serve_on(pipe_name(my_session()), hub, agent).await
}

async fn serve_on(name: String, hub: StatusHub, agent: Agent) {
    let session = my_session();
    let mut server = match create(&name, true) {
        Ok(s) => s,
        Err(e) => {
            warn!("the tray can't reach the host: {e:#}");
            return;
        }
    };
    info!("listening on {name}");
    loop {
        if let Err(e) = server.connect().await {
            warn!("pipe connect failed: {e}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        // 先开好下一个实例给后面的托盘，再处理这一个
        let next = loop {
            match create(&name, false) {
                Ok(s) => break s,
                Err(e) => {
                    warn!("{e:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let client = std::mem::replace(&mut server, next);
        let mut client_session = u32::MAX;
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ =
                GetNamedPipeClientSessionId(HANDLE(client.as_raw_handle()), &mut client_session);
        }
        if client_session != session {
            warn!(client_session, "rejected a tray from another session");
            continue;
        }
        let (hub, agent) = (hub.clone(), agent.clone());
        tokio::spawn(async move {
            if let Err(e) = serve_tray(client, hub, agent).await {
                debug!("tray disconnected: {e:#}");
            }
        });
    }
}

async fn serve_tray(pipe: NamedPipeServer, hub: StatusHub, agent: Agent) -> Result<()> {
    let (r, mut w) = tokio::io::split(pipe);
    let mut lines = BufReader::new(r).lines();
    let mut hub_rx = hub.subscribe();
    let mut auto_rx = agent.autostart.subscribe();
    let mut last: Option<AgentStatus> = None;
    loop {
        let status = AgentStatus {
            host: hub_rx.borrow_and_update().host.clone(),
            autostart: *auto_rx.borrow_and_update(),
        };
        if last.as_ref() != Some(&status) {
            send_line(&mut w, &status).await?;
            last = Some(status);
        }
        tokio::select! {
            r = hub_rx.changed() => r?,
            r = auto_rx.changed() => r?,
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                match serde_json::from_str::<ToAgent>(&line) {
                    Ok(msg) => handle(msg, &agent).await,
                    Err(e) => warn!("invalid message from the tray: {e}"),
                }
            }
        }
    }
}

async fn handle(msg: ToAgent, agent: &Agent) {
    let (cmds, autostart) = (&agent.cmds, &agent.autostart);
    match msg {
        ToAgent::KickAll => {
            let _ = cmds.send(HostCmd::KickAll);
        }
        ToAgent::NewCode => {
            let _ = cmds.send(HostCmd::NewCode);
        }
        ToAgent::NewPairingCode => {
            let gate = agent.gate.clone();
            let _ = tokio::task::spawn_blocking(move || gate.new_pairing_code()).await;
        }
        ToAgent::ForgetPaired => {
            let gate = agent.gate.clone();
            let _ = tokio::task::spawn_blocking(move || gate.forget_paired()).await;
        }
        ToAgent::SaveControllerKey { key } => {
            let r = tokio::task::spawn_blocking(move || crate::config::save_controller_key(&key)).await;
            match r {
                Ok(Ok(())) => debug!("saved a console identity"),
                Ok(Err(e)) => warn!("can't save the console identity: {e:#}"),
                Err(e) => warn!("can't save the console identity: {e}"),
            }
        }
        ToAgent::SetAutostart { on } => {
            let r = tokio::task::spawn_blocking(move || {
                super::apply_autostart(on)?;
                crate::config::save_autostart(on)
            })
            .await;
            match r {
                Ok(Ok(())) => {
                    info!(on, "autostart changed");
                    autostart.send_replace(on);
                }
                Ok(Err(e)) => warn!("can't change autostart: {e:#}"),
                Err(e) => warn!("can't change autostart: {e}"),
            }
        }
        ToAgent::SaveDevices { devices } => {
            // 只改 [[device]]，每一项写之前再检查一遍（见 devices::to_table）
            let n = devices.len();
            let r = tokio::task::spawn_blocking(move || crate::config::save_devices(&devices)).await;
            match r {
                Ok(Ok(())) => debug!(n, "saved the devices"),
                Ok(Err(e)) => warn!("can't save the devices: {e:#}"),
                Err(e) => warn!("can't save the devices: {e}"),
            }
        }
    }
}

// ───────────────────────────── 托盘一侧 ─────────────────────────────

/// 托盘到服务里被控端的连接：断了自动重连，状态写进 [`AppStatus`](crate::status::AppStatus)
pub struct AgentLink {
    tx: mpsc::UnboundedSender<ToAgent>,
    task: JoinHandle<()>,
}

impl AgentLink {
    /// `on_stopped`：服务运行过、后来停止了（托盘"退出"、卸载、升级）时调用，托盘随之退出
    pub fn start(hub: StatusHub, on_stopped: impl FnOnce() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tauri::async_runtime::spawn(run(hub, rx, on_stopped));
        Self { tx, task }
    }

    /// 发给被控端（没连上时丢弃）
    pub fn send(&self, msg: ToAgent) {
        let _ = self.tx.send(msg);
    }
}

impl Drop for AgentLink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    hub: StatusHub,
    mut rx: mpsc::UnboundedReceiver<ToAgent>,
    on_stopped: impl FnOnce() + Send + 'static,
) {
    let name = pipe_name(my_session());
    let mut seen_running = false;
    loop {
        match open(&name) {
            Ok(pipe) => {
                if let Err(e) = talk(pipe, &hub, &mut rx, &mut seen_running).await {
                    debug!("lost the connection to the host: {e:#}");
                }
                hub.send_modify(|s| {
                    s.host = HostStatus::default();
                    if let Some(svc) = &mut s.service {
                        svc.connected = false;
                    }
                });
            }
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(_) => {}
        }
        // 断开期间的命令作废
        while rx.try_recv().is_ok() {}
        let (state, console) = tokio::task::spawn_blocking(|| {
            (super::state(), unsafe { WTSGetActiveConsoleSessionId() })
        })
        .await
        .unwrap_or((super::State::Running, my_session()));
        if seen_running && matches!(state, super::State::Missing | super::State::Stopped) {
            info!(?state, "the Airhand service stopped");
            on_stopped();
            return;
        }
        let running = matches!(state, super::State::Running | super::State::Starting);
        hub.send_if_modified(|s| {
            let Some(svc) = &mut s.service else {
                return false;
            };
            let before = *svc;
            svc.running = running;
            // 被控端只在控制台会话（物理屏幕前的那个）里运行
            svc.elsewhere = console != my_session();
            *svc != before
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 管道的所有者是不是 SYSTEM 或管理员（被控端以 SYSTEM 运行；普通用户开的管道属于这个用户）
fn owned_by_system(handle: HANDLE) -> bool {
    unsafe {
        let mut owner = PSID::default();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let r = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut sd),
        );
        if r.is_err() {
            return false;
        }
        let ok = IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool();
        LocalFree(Some(HLOCAL(sd.0)));
        ok
    }
}

/// 连接被控端的管道。只要读和写数据的权限（`GENERIC_WRITE` 里还有管道没有给的 `FILE_CREATE_PIPE_INSTANCE`），
/// 并核对管道的所有者
fn open(name: &str) -> std::io::Result<NamedPipeClient> {
    open_pipe(name, true)
}

fn open_pipe(name: &str, check_owner: bool) -> std::io::Result<NamedPipeClient> {
    let handle = unsafe {
        CreateFileW(
            &HSTRING::from(name),
            FILE_GENERIC_READ.0 | FILE_WRITE_DATA.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
    if check_owner && !owned_by_system(handle) {
        unsafe {
            let _ = CloseHandle(handle);
        }
        warn!("{name} isn't served by Airhand, ignoring it");
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    unsafe { NamedPipeClient::from_raw_handle(handle.0) }
}

async fn talk(
    pipe: NamedPipeClient,
    hub: &StatusHub,
    rx: &mut mpsc::UnboundedReceiver<ToAgent>,
    seen_running: &mut bool,
) -> Result<()> {
    let (r, mut w) = tokio::io::split(pipe);
    let mut lines = BufReader::new(r).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                let st: AgentStatus = serde_json::from_str(&line)?;
                *seen_running = true;
                hub.send_modify(|s| {
                    s.host = st.host;
                    s.service = Some(ServiceStatus {
                        connected: true,
                        running: true,
                        elsewhere: false,
                        autostart: st.autostart,
                    });
                });
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                send_line(&mut w, &msg).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status;

    /// 被控端一侧和托盘一侧的消息往返。管道的所有者是运行测试的用户：托盘本来会拒绝它（不是 SYSTEM），这里不检查
    #[tokio::test]
    async fn status_and_commands() {
        let name = format!(r"\\.\pipe\airhand-test-{:016x}", rand::random::<u64>());
        let hub = status::new_hub();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let autostart = Arc::new(watch::Sender::new(true));
        let agent = Agent {
            cmds: cmd_tx,
            autostart,
            gate: DesktopGate::load(hub.clone()),
        };
        tokio::spawn(serve_on(name.clone(), hub.clone(), agent));

        // 只用托盘的权限（读 + 写数据）就能收发消息
        let pipe = loop {
            match open_pipe(&name, false) {
                Ok(p) => break p,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        let (r, mut w) = tokio::io::split(pipe);
        let mut lines = BufReader::new(r).lines();
        let mut next = async || -> AgentStatus {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(&line).unwrap()
        };
        let first = next().await;
        assert_eq!((first.host.code, first.host.controllers.len()), (None, 0));
        assert!(first.autostart);

        hub.send_modify(|s| s.host.code = Some(123_456));
        assert_eq!(next().await.host.code, Some(123_456));

        send_line(&mut w, &ToAgent::NewCode).await.unwrap();
        let cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .unwrap();
        assert!(matches!(cmd, Some(HostCmd::NewCode)));
    }
}
