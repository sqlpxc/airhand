//! 被控端服务：保持与服务器的连接（自动重连、取回原控制码），并为每个控制端请求启动会话。

pub(crate) mod audio;
pub mod capture;
pub(crate) mod clock;
mod cursor;
#[cfg(windows)]
pub(crate) mod desktop;
pub mod encoder;
pub(crate) mod ending;
pub mod gate;
mod input;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
pub(crate) mod mic;
mod pace;
mod pipeline;
mod power;
#[cfg(target_os = "macos")]
pub(crate) mod sck;
mod session;
pub mod virtual_display;
#[cfg(target_os = "linux")]
pub(crate) mod x11;

use std::sync::{Arc, Mutex, MutexGuard};

use airhand_link::ServerState;
use airhand_link::files::LocalFs;
use airhand_link::host::HostEvents;
use airhand_proto::signal::Reclaim;

use gate::DesktopGate;
use tauri::async_runtime::JoinHandle;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub use airhand_link::host::HostCmd;

use crate::config;
use crate::status::StatusHub;

/// 在"每显示器 DPI 感知"上下文中执行（线程级）。
///
/// DXGI 返回的是物理像素坐标，而 DPI 未感知的线程调用 `GetSystemMetrics` 等 API 会得到
/// 缩放后的逻辑坐标，两者混用会导致鼠标位置错位。所有涉及坐标的调用都包在这里面。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn dpi_aware<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
        };
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let r = f();
        if !old.is_invalid() {
            SetThreadDpiAwarenessContext(old);
        }
        r
    }
    #[cfg(not(windows))]
    f()
}

/// 被控端缺少的系统权限：`(屏幕与系统录音, 辅助功能)`，true = 缺少。目前只有 macOS 需要用户授权。
pub(crate) fn missing_permissions() -> (bool, bool) {
    #[cfg(target_os = "macos")]
    {
        let p = macos::permissions();
        (!p.screen, !p.input)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (false, false)
    }
}

/// 走 Wi-Fi、AWDL 开着（只有 macOS 看，见 `crate::awdl::hint`）：告诉控制端画面可能周期性地卡顿
pub(crate) fn awdl_hint() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::awdl::hint()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// 本机复制的文件交给谁读（公告给控制端，它粘贴时来取）：一般是本进程直接读（[`LocalFs`]）；
/// Windows 服务里的被控端是 SYSTEM，交给托盘以登录的用户的身份读（见 `winsvc::clip`），没有托盘就不公告。
/// 一个都不在了（剪贴板里的路径都没了）时也不公告
pub(crate) async fn clip_provider(
    paths: Vec<std::path::PathBuf>,
) -> Option<Arc<dyn airhand_link::files::Provider>> {
    #[cfg(windows)]
    if crate::winsvc::is_agent() {
        return crate::winsvc::clip::provider(paths).await;
    }
    let local = tokio::task::spawn_blocking(move || LocalFs::new(&paths))
        .await
        .ok()?;
    (!local.is_empty()).then(|| Arc::new(local) as Arc<dyn airhand_link::files::Provider>)
}

/// 托盘菜单里切换"结束远程后锁屏"（见 [`ending`]）：写进 config.toml，马上生效，托盘里跟着变。
/// 写不进去就不改（不然重新启动之后又变回去，和菜单里看到的不一样）。要写文件：不在主线程、异步任务里调用
pub(crate) fn set_lock_on_end(hub: &StatusHub, on: bool) {
    match config::save_lock_on_end(on) {
        Ok(()) => {
            ending::set_lock_on_end(on);
            info!(
                on,
                "locking the screen when the remote control ends: changed"
            );
        }
        Err(e) => warn!("can't change locking the screen when the remote control ends: {e:#}"),
    }
    // 勾选项被点击时系统已经翻转了勾选状态：没改成也要按现在的刷新一次
    let on = ending::lock_on_end();
    hub.send_modify(|s| s.host.lock_on_end = Some(on));
}

/// 菜单栏图标的菜单里切换"不自动睡眠"（macOS，见 [`power`]）：写进 config.toml，马上生效，菜单里跟着变。
/// 写不进去就不改（理由同 [`set_lock_on_end`]）。要写文件：不在主线程、异步任务里调用
pub(crate) fn set_prevent_sleep(hub: &StatusHub, on: bool) {
    match config::save_prevent_sleep(on) {
        Ok(()) => {
            power::set_prevent_sleep(on);
            info!(on, "preventing automatic sleep: changed");
        }
        Err(e) => warn!("can't change preventing automatic sleep: {e:#}"),
    }
    let on = power::prevent_sleep();
    hub.send_modify(|s| s.host.prevent_sleep = Some(on));
}

/// 采集失败是不是因为缺少系统权限（这种情况由会话用 `Notice::MissingPermissions` 单独说明）。
pub(crate) fn missing_permission(e: &anyhow::Error) -> bool {
    #[cfg(target_os = "macos")]
    {
        e.chain().any(|c| c.is::<macos::PermissionDenied>())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = e;
        false
    }
}

/// 本机的控制码和取回它用的密钥（服务器分配的，固定不变，见 `airhand_proto::signal` 的"固定控制码"）。克隆出来的是同一份。
///
/// 同一个进程里被控端会停下再启动：macOS / Linux 打开控制台时被控端停下，关掉控制台再启动（见
/// [`crate::status::HOST_WITH_CONSOLE`]）。再启动时要接着用现在的控制码，所以服务器分配、更换控制码、换密钥时都记在这里，
/// 不能再从 [`config::settings`] 拿：那是进程启动时读到的内容，之后的变化都不在里面——拿着它去取回，
/// 刚装好（还没有控制码）时每次都会领到一个新的，更换过控制码时又会把换掉的旧控制码取回来。
/// 只在内存里：配置文件写不进去时，这次运行里也不会丢
#[derive(Clone, Default)]
pub struct HostCode(Arc<Mutex<Option<Option<Reclaim>>>>);

impl HostCode {
    /// 现在的控制码，`None`：还没有（让服务器分配一个）。第一次用时从配置文件读。不在创建时读：
    /// Windows 安装版的托盘也有一份但用不到，它解不开服务保存的密钥
    fn current(&self) -> Option<Reclaim> {
        *self.lock().get_or_insert_with(|| config::settings().code())
    }

    fn set(&self, code: Option<Reclaim>) {
        *self.lock() = Some(code);
    }

    fn lock(&self) -> MutexGuard<'_, Option<Option<Reclaim>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct HostService {
    task: JoinHandle<()>,
    cmd: mpsc::UnboundedSender<HostCmd>,
    gate: Arc<DesktopGate>,
    /// 被控端运行期间按"不自动睡眠"不让电脑空闲睡眠（macOS，见 [`power`]）
    _hosting: power::Hosting,
}

impl HostService {
    /// 启动被控端，状态写进 `hub` 的 [`AppStatus::host`](crate::status::AppStatus::host)。
    /// `code`：取回哪个控制码；同一个进程里停下再启动的被控端要用同一个 [`HostCode`]
    pub fn start(hub: StatusHub, code: HostCode) -> Self {
        clock::init();
        let lock_on_end = Some(ending::lock_on_end());
        // 不自动睡眠只有 macOS 有（菜单里只在知道现在的设置时显示）
        let prevent_sleep = cfg!(target_os = "macos").then(power::prevent_sleep);
        hub.send_if_modified(|s| {
            let changed =
                s.host.lock_on_end != lock_on_end || s.host.prevent_sleep != prevent_sleep;
            s.host.lock_on_end = lock_on_end;
            s.host.prevent_sleep = prevent_sleep;
            changed
        });
        let gate = DesktopGate::load(hub.clone());
        let (cmd, rx) = mpsc::unbounded_channel();
        let task = tauri::async_runtime::spawn(run(hub, gate.clone(), code, rx));
        Self {
            task,
            cmd,
            gate,
            _hosting: power::hosting(),
        }
    }

    pub fn send(&self, c: HostCmd) {
        let _ = self.cmd.send(c);
    }

    /// 本机的身份、配对码、配对过的控制端（托盘菜单里换配对码、忘掉配对过的控制端）
    pub fn gate(&self) -> &Arc<DesktopGate> {
        &self.gate
    }

    /// 向被控端发命令的另一个入口（系统服务里的被控端经命名管道收到托盘的命令）
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn commands(&self) -> mpsc::UnboundedSender<HostCmd> {
        self.cmd.clone()
    }
}

impl Drop for HostService {
    fn drop(&mut self) {
        // 中止任务会依次释放会话、QUIC 连接（自动向服务器发送关闭帧）
        self.task.abort();
    }
}

async fn run(
    hub: StatusHub,
    gate: Arc<DesktopGate>,
    code: HostCode,
    cmds: mpsc::UnboundedReceiver<HostCmd>,
) {
    // 上次意外退出时虚拟显示器还启用着、防窥撤下的本机显示器还没放回来：先恢复，再接会话
    let _ = tokio::task::spawn_blocking(virtual_display::recover).await;
    // 现在的控制码（第一次启动时是 config.toml 里保存的）：第一次连接就取回它
    let reclaim = code.current();
    let events = Arc::new(TrayStatus {
        hub,
        gate: gate.clone(),
        code,
    });
    airhand_link::host::run(
        config::link(),
        config::persistent(),
        reclaim,
        events,
        move |inc| {
            let gate = gate.clone();
            async move {
                let _ = session::run(inc, gate).await;
            }
        },
        cmds,
    )
    .await;
}

/// 被控端的状态写进托盘的 [`StatusHub`]，控制码记在 [`HostCode`] 里，并保存在 config.toml 里
struct TrayStatus {
    hub: StatusHub,
    gate: Arc<DesktopGate>,
    code: HostCode,
}

/// 保存控制码（找不到配置文件的位置时什么也不做）。写不进去只是下次启动时换一个控制码，不影响这次运行
async fn save_code(code: Option<Reclaim>) {
    let r = tokio::task::spawn_blocking(move || config::save_code(code)).await;
    if let Ok(Err(e)) = r {
        warn!("can't save the control code: {e:#}");
    }
}

impl HostEvents for TrayStatus {
    fn server(&self, state: ServerState) {
        self.hub.send_if_modified(|st| {
            let changed = st.host.server != state;
            st.host.server = state;
            changed
        });
    }

    async fn registered(&self, code: Reclaim, changed: bool) {
        // 先记下来再保存：保存的时候被控端被停下（任务在这里被中止）也不丢
        self.code.set(Some(code));
        if changed {
            save_code(Some(code)).await;
        }
        self.hub.send_modify(|s| s.host.code = Some(code.code));
    }

    async fn released(&self) {
        self.code.set(None);
        save_code(None).await;
    }

    fn ended(&self, session_id: u64) {
        self.hub
            .send_modify(|s| s.host.controllers.retain(|c| c.session_id != session_id));
    }

    fn offline(&self) {
        self.hub.send_modify(|s| {
            s.host.code = None;
            s.host.controllers.clear();
        });
    }

    fn kicked(&self, cmd: HostCmd) {
        // 更换控制码：配对码也换一个，配对过的控制端全部忘掉（被断开的人不能拿着旧的配对连回来）
        if cmd == HostCmd::NewCode {
            self.gate.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务器分配、换了密钥、放弃（更换控制码）都记进 [`HostCode`]：被控端停下再启动时取回的是现在的控制码，
    /// 不是进程启动时配置文件里的（测试里没有配置文件：一开始没有控制码）
    #[tokio::test]
    async fn restarted_host_reclaims_the_current_code() {
        let code = HostCode::default();
        assert_eq!(code.current(), None);
        let hub = crate::status::new_hub();
        let events = TrayStatus {
            hub: hub.clone(),
            gate: DesktopGate::load(hub.clone()),
            code: code.clone(),
        };
        // 托盘里只有一份，被控端每次启动拿的是它的克隆
        let next_start = code.clone();

        let first = Reclaim {
            code: 123_456,
            secret: [1; 16],
        };
        events.registered(first, true).await;
        assert_eq!(next_start.current(), Some(first));
        assert_eq!(hub.borrow().host.code, Some(123_456));

        // 断开所有控制端之后服务器换了密钥
        let rekeyed = Reclaim {
            secret: [2; 16],
            ..first
        };
        events.registered(rekeyed, true).await;
        assert_eq!(next_start.current(), Some(rekeyed));

        // 更换控制码：先放弃旧的（这时停下的话，再启动时领一个新的，不会取回旧的），再领到新的
        events.released().await;
        assert_eq!(next_start.current(), None);
        let second = Reclaim {
            code: 654_321,
            secret: [3; 16],
        };
        events.registered(second, true).await;
        assert_eq!(next_start.current(), Some(second));
    }
}
