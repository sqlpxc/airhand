//! 虚拟显示器（Windows）：被控端的显示器关了电源也能远程控制；防窥模式。macOS 上补屏（合上盖子、
//! 没接显示器时，见 [`mac`]），防窥模式不用虚拟显示器（见 [`curtain`]），状态和 Windows 一样在这里管。
//!
//! # 显示器关了电源
//!
//! 很多显示器（尤其是 DP 接口的）关了电源，线上的热插拔信号也就断了，Windows 把它从桌面上移走。一个显示器都不剩时
//! 没有桌面可以采集，控制端看到的是"没有显示器"、"屏幕采集失败"。有会话、本机却没有显示器在桌面上时，这里启用一块
//! 虚拟显示器补上（"补屏"）；显示器回来了就停用它。会话开始时没有显示器的话，会话等它出来再开始
//! （见 `host::session`）。
//!
//! # 防窥模式
//!
//! 控制端打开防窥模式时（见 `airhand_proto::session` 的"防窥模式"），启用虚拟显示器，只留它在桌面上、撤下本机所有的
//! 显示器（显示器没有信号，什么也看不到：开始菜单、UAC 确认框、锁屏、鼠标指针都在虚拟显示器上），挡住本机的键盘
//! 鼠标（[`block`]）。控制端看到、操作的是虚拟显示器。macOS 上控制端看到、操作的还是本机的显示器，本机的显示器
//! 调黑、盖上采集时去掉的黑窗口，挡住本机的键盘鼠标（见 [`curtain`]）。
//!
//! * 整台被控端一份：哪个控制端打开的都一样，哪一个都能关；
//! * 打开它的控制端都走了：接着开着，远程控制结束了才关掉（[`release_privacy`]，见 `host::ending`）——
//!   控制端断开了马上关（一个会话都不剩的话先锁屏），断线了等它重连一会儿（重连时在介绍里带着它，接着开着，
//!   本机屏幕不会在重连的间隙里露出来）。控制端断线之后本机有人按键、点鼠标（或者按了之后不久控制端
//!   断线了：手机上划掉了应用）就不等它重连，马上锁屏、关掉防窥（[`block`]、[`curtain`] 告诉 `host::ending`）。
//!   本机的人想结束还连着的控制端开的防窥：断开网络，过一会儿会话断了，再按一下键（锁着屏）；或者重启电脑；
//! * 关掉时停用虚拟显示器，按 Windows 记着的设置恢复本机的显示器（改设置时从不存进 Windows 的显示设置数据库，
//!   见 [`topology`]）。进程意外退出、断电重启之后，下次启动时恢复（[`recover`]）。macOS 上进程退出时系统自己
//!   恢复（gamma 表、窗口、事件监听都是这个进程的）；
//! * 被控端的 `config.toml` 里 `privacy_mode = false` 时不允许（[`PrivacyState::NotAllowed`]）。
//!
//! # 驱动
//!
//! 用的是 MttVDD（Virtual Display Driver），随安装包带着，为所有用户安装时问用户要不要装（见 [`device`]）。
//! 没装、或者 Airhand 不是为所有用户安装的（没有权限启用、停用设备）时都用不了：补屏不做，防窥是
//! [`PrivacyState::Unavailable`]。macOS 用系统自带的（私有的）`CGVirtualDisplay`，不用装什么；macOS 的防窥
//! 要"辅助功能"权限（挡住本机的键盘鼠标），没有时是 [`PrivacyState::Unavailable`]。Linux 没有防窥（不发防窥的
//! 状态，控制端不显示这个开关）。
//!
//! 别的地方用的是 [`join`] 得到的 [`Lease`]：每个会话一个，会话结束时丢掉。

#[cfg(windows)]
mod block;
#[cfg(target_os = "macos")]
mod curtain;
#[cfg(windows)]
pub(crate) mod device;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod manager;
#[cfg_attr(not(windows), allow(dead_code))]
mod plan;
#[cfg(windows)]
mod topology;

use std::collections::HashSet;
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use airhand_proto::session::PrivacyState;
use tokio::sync::watch;

/// 这个平台有防窥模式（告诉控制端它的状态，控制端才显示这个开关）
const HAS_PRIVACY: bool = cfg!(any(windows, target_os = "macos"));

/// 打开防窥的控制端都走了之后，防窥最多再开这么久。平时不到这么久就关了（[`release_privacy`]），这是兜底：
/// 万一没人叫它，本机的显示器也不会一直黑着、键盘鼠标一直不能用
const PRIVACY_HOLD_MAX: Duration = Duration::from_secs(90);
/// 最后一个会话结束之后，补屏的虚拟显示器再留这么久（等断线重连）
#[cfg(any(windows, target_os = "macos"))]
const SESSION_LINGER: Duration = Duration::from_secs(30);
/// 会话开始时没有显示器：最多等虚拟显示器这么久
pub const APPEAR_WAIT: Duration = Duration::from_secs(15);

/// 一个显示模式：虚拟显示器用和本机主显示器一样的（控制端看到的画面大小不变）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

impl Default for Mode {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            hz: 60,
        }
    }
}

/// 防窥模式现在怎么样（整台被控端一份，发给每个控制端：`HostMsg::Privacy`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Privacy {
    pub state: PrivacyState,
    pub detail: Option<String>,
}

struct Hub {
    wants: Mutex<Wanted>,
    /// 叫醒管理线程（[`Wanted::poked`]）、等它退出
    wake: Condvar,
    privacy: watch::Sender<Privacy>,
    /// 改过显示设置、启用停用过虚拟显示器就加一：会话据此马上重新列出显示器
    displays: watch::Sender<u64>,
}

#[derive(Default)]
struct Wanted {
    /// 有哪些会话
    sessions: HashSet<u64>,
    /// 打开了防窥的会话
    holders: HashSet<u64>,
    /// 打开防窥的会话都结束了：接着开着，直到 [`release_privacy`]（最多到这个时刻，见模块说明）
    linger: Option<Instant>,
    /// 最后一个会话结束的时刻
    idle_since: Option<Instant>,
    /// 被控端在运行、开着"不自动睡眠"（macOS）：没有会话也预备着虚拟显示器（见 `set_ready`）
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    ready: bool,
    /// 上次没能打开防窥的原因（再打开、关掉时清掉）
    failed: Option<String>,
    /// 防窥开着（Windows：只留着虚拟显示器；macOS：调黑了、盖上了，由管理线程设置）
    on: bool,
    /// 能用虚拟显示器（每个会话加入时重新看一次：可能刚装上驱动）
    usable: bool,
    /// macOS 能开防窥：有"辅助功能"权限（会话加入时、管理线程每一轮重新看）
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    curtain: bool,
    /// 有事了，管理线程马上看一眼
    poked: bool,
    /// 管理线程在运行
    running: bool,
    /// 进程要退出了：管理线程恢复原样之后结束
    stopping: bool,
}

impl Wanted {
    fn privacy(&self, now: Instant) -> bool {
        !self.holders.is_empty() || self.linger.is_some_and(|t| now < t)
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn sessions(&self, now: Instant) -> bool {
        !self.sessions.is_empty() || self.idle_since.is_some_and(|t| now < t + SESSION_LINGER)
    }

    /// 这台被控端现在能开防窥（不管设置允不允许）：Windows 要能用虚拟显示器，macOS 要有"辅助功能"权限
    fn privacy_possible(&self) -> bool {
        if cfg!(windows) {
            self.usable
        } else {
            cfg!(target_os = "macos") && self.curtain
        }
    }

    fn poke(&mut self) {
        self.poked = true;
        HUB.wake.notify_all();
    }
}

static HUB: LazyLock<Hub> = LazyLock::new(|| Hub {
    wants: Mutex::default(),
    wake: Condvar::new(),
    privacy: watch::Sender::new(Privacy {
        state: PrivacyState::Unavailable,
        detail: None,
    }),
    displays: watch::Sender::new(0),
});

/// 被控端的设置允许防窥模式
fn allowed() -> bool {
    crate::config::settings().privacy_mode
}

/// 按现在想要的、管理线程报告的，更新发给控制端的状态
fn publish(w: &Wanted) {
    let now = Instant::now();
    let (state, detail) = if !w.privacy_possible() {
        (PrivacyState::Unavailable, None)
    } else if !allowed() {
        (PrivacyState::NotAllowed, None)
    } else if w.privacy(now) {
        (
            if w.on {
                PrivacyState::On
            } else {
                PrivacyState::Starting
            },
            None,
        )
    } else if let Some(d) = &w.failed {
        (PrivacyState::Failed, Some(d.clone()))
    } else {
        (PrivacyState::Off, None)
    };
    let p = Privacy { state, detail };
    HUB.privacy.send_if_modified(|old| {
        let changed = *old != p;
        *old = p;
        changed
    });
}

/// 防窥没能打开（由管理线程调用）：不再要它，告诉控制端为什么
#[cfg(any(windows, target_os = "macos"))]
fn fail(detail: String) {
    tracing::warn!("privacy mode failed: {detail}");
    let mut w = HUB.wants.lock().unwrap();
    w.holders.clear();
    w.linger = None;
    w.failed = Some(detail);
    publish(&w);
}

/// 一个会话对虚拟显示器的需要（见模块说明）。丢掉时离开
pub struct Lease {
    session_id: u64,
}

/// 会话开始：有会话就要有显示器可看（补屏）
pub fn join(session_id: u64) -> Lease {
    #[cfg(windows)]
    let usable = device::usable();
    #[cfg(target_os = "macos")]
    let usable = mac::usable();
    #[cfg(not(any(windows, target_os = "macos")))]
    let usable = false;
    #[cfg(target_os = "macos")]
    let privacy = curtain::usable();
    let mut w = HUB.wants.lock().unwrap();
    w.sessions.insert(session_id);
    w.idle_since = None;
    w.usable = usable;
    #[cfg(target_os = "macos")]
    {
        w.curtain = privacy;
    }
    #[cfg(windows)]
    if usable && !w.running {
        w.running = true;
        w.stopping = false;
        manager::spawn();
    }
    // 补屏、防窥都在 mac 的管理线程上
    #[cfg(target_os = "macos")]
    if (usable || privacy) && !w.running {
        w.running = true;
        w.stopping = false;
        mac::spawn();
    }
    publish(&w);
    w.poke();
    Lease { session_id }
}

impl Lease {
    /// 控制端打开 / 关闭防窥模式。不能用、不允许时什么也不做（状态里已经说了为什么）
    pub fn set_privacy(&self, on: bool) {
        let mut w = HUB.wants.lock().unwrap();
        if !w.privacy_possible() || !allowed() || !w.sessions.contains(&self.session_id) {
            return;
        }
        w.failed = None;
        w.linger = None;
        if on {
            w.holders.insert(self.session_id);
        } else {
            w.holders.clear();
        }
        tracing::info!(
            session_id = self.session_id,
            on,
            "the controller switched privacy mode"
        );
        publish(&w);
        w.poke();
    }

    /// 防窥模式的状态；这个平台没有防窥模式时为 `None`（不告诉控制端，它就不显示这个开关）
    pub fn privacy(&self) -> Option<Privacy> {
        HAS_PRIVACY.then(|| HUB.privacy.borrow().clone())
    }

    /// 防窥模式的状态变了
    pub fn privacy_changes(&self) -> watch::Receiver<Privacy> {
        HUB.privacy.subscribe()
    }

    /// 改过显示设置了（会话马上重新列出显示器）
    pub fn display_changes(&self) -> watch::Receiver<u64> {
        HUB.displays.subscribe()
    }

    /// 能补一块虚拟显示器（会话开始时没有显示器的话值得等它，见 [`APPEAR_WAIT`]）
    pub fn can_fill_in(&self) -> bool {
        HUB.wants.lock().unwrap().usable
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut w = HUB.wants.lock().unwrap();
        w.sessions.remove(&self.session_id);
        if w.holders.remove(&self.session_id) && w.holders.is_empty() {
            // 打开防窥的都走了：接着开着，远程控制结束了才关（先锁屏，或者给断线的重连的时间，见 host::ending）
            w.linger = Some(now + PRIVACY_HOLD_MAX);
        }
        if w.sessions.is_empty() {
            w.idle_since = Some(now);
        }
        publish(&w);
        w.poke();
    }
}

/// 远程控制结束了（或者打开防窥的控制端走了、不会回来了，见 `host::ending`）：打开防窥的会话都不在的话，
/// 防窥马上关掉。还有会话开着它（例如断线的控制端带着它重连上了）时什么也不做
pub fn release_privacy() {
    let mut w = HUB.wants.lock().unwrap();
    if !w.holders.is_empty() || w.linger.is_none() {
        return;
    }
    w.linger = None;
    tracing::info!("privacy mode ends with the remote control");
    publish(&w);
    w.poke();
}

/// 被控端在运行、开着"不自动睡眠"（macOS，见 `power`）：没有会话时也预备着镜像的虚拟显示器，合上盖子之后
/// 连上来也有画面（虚拟显示器得在合盖之前就建好，见 [`mac`]）。笔记本才需要，台式机什么也不做
#[cfg(target_os = "macos")]
pub fn set_ready(on: bool) {
    let mut w = HUB.wants.lock().unwrap();
    if w.ready == on {
        return;
    }
    w.ready = on;
    if on && !w.running && crate::host::macos::lid_state().is_some() && mac::usable() {
        w.running = true;
        w.stopping = false;
        mac::spawn();
    }
    w.poke();
}

/// 本进程补屏的虚拟显示器的像素大小（macOS，见 [`mac`]）；`id` 不是它时为 `None`
#[cfg(target_os = "macos")]
pub fn pixels(id: u32) -> Option<(u32, u32)> {
    mac::pixels(id)
}

/// 防窥模式盖在本机显示器上的黑窗口（macOS，窗口编号，见 [`curtain`]）：采集时去掉它们
#[cfg(target_os = "macos")]
pub fn cover_windows() -> Vec<u32> {
    curtain::cover_windows()
}

/// 黑窗口变了就加一（macOS）：在这之前打开的采集要重新打开
#[cfg(target_os = "macos")]
pub fn cover_generation() -> u64 {
    curtain::cover_generation()
}

/// 补屏的虚拟显示器在桌面上的名字（和 `DisplayInfo::name` 一样，例如 `DISPLAY19`）；没在补屏（没有它、开着防窥）时
/// 为 `None`。本机的显示器回到桌面上之后它马上就要停用：会话不再跟着它（见 `host::session`）。要查显示设置，
/// 放在不挡异步任务的线程上调用
pub fn filling_in() -> Option<String> {
    #[cfg(windows)]
    {
        if HUB.wants.lock().unwrap().privacy(Instant::now()) {
            return None;
        }
        topology::virtual_name()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 进程启动时（还没有会话）、卸载时：上次意外退出没恢复的恢复掉（见模块说明）
pub fn recover() {
    #[cfg(windows)]
    if device::usable() {
        manager::recover();
    }
}

/// 进程退出：恢复原样（停用虚拟显示器、放回本机的显示器、放开键盘鼠标；macOS 撤掉补屏的虚拟显示器），最多等几秒
pub fn shutdown() {
    let mut w = HUB.wants.lock().unwrap();
    if !w.running {
        return;
    }
    w.stopping = true;
    w.poke();
    let _ = HUB
        .wake
        .wait_timeout_while(w, Duration::from_secs(5), |w| w.running)
        .unwrap();
}

/// 测试用：现在有哪些会话、各自打开了防窥没有
#[cfg(test)]
fn sessions() -> std::collections::HashMap<u64, bool> {
    let w = HUB.wants.lock().unwrap();
    w.sessions
        .iter()
        .map(|s| (*s, w.holders.contains(s)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开不了防窥（Windows 没装驱动、没有权限，macOS 没有"辅助功能"权限）：开关防窥什么也不做，会话进出照常记着
    #[test]
    fn without_the_driver_nothing_happens() {
        let a = join(9001);
        let b = join(9002);
        if HUB.wants.lock().unwrap().privacy_possible() {
            // 这台电脑装了驱动、测试又有管理员权限（macOS：终端有辅助功能权限）：不在这里黑屏、挡住键盘鼠标
            return;
        }
        a.set_privacy(true);
        assert_eq!(sessions().get(&9001), Some(&false));
        assert_eq!(
            a.privacy().map(|p| p.state),
            HAS_PRIVACY.then_some(PrivacyState::Unavailable)
        );
        drop(a);
        assert!(!sessions().contains_key(&9001));
        assert!(sessions().contains_key(&9002));
        drop(b);
    }
}
