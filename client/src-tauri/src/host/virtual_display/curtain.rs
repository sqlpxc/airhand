//! 防窥模式（macOS）：本机的显示器黑屏、本机的键盘鼠标不起作用，控制端照样看得到、操作得了。
//!
//! Windows 撤下本机的显示器、只留虚拟显示器（见 `super` 的模块说明）。macOS 上停用显示器只有私有接口，进程没了
//! 显示器也不会自己回来；这里不动显示器，控制端看、操作的还是本机的显示器，只是本机的人看不到：
//!
//! * **调黑**：本机每个显示器的 gamma 表（显示器输出前最后一步的颜色映射）设成全 0。采集读的是它前面的帧缓冲，
//!   控制端看到的画面不受影响（RustDesk、Curtain 的防窥也是这样做的）。锁屏界面、菜单栏都一起黑了。
//!   gamma 表是设它的进程的：进程退出（崩溃了也一样）时系统自己恢复，不会让显示器一直黑着。系统在显示器变化、
//!   唤醒时也会把它恢复：每 [`TICK`] 读回来看一眼，不是黑的就再设一次（中间可能露出一下）；
//! * **黑窗口**：每个本机显示器上再盖一个黑色的窗口（所有窗口之上，见 `curtain.m`），本进程的采集去掉它
//!   （`SCContentFilter` 的 excludingWindows，见 `capture::sck`）。有的时候 gamma 表不起作用：M5 Pro / Max、
//!   MacBook Neo 上的 macOS 26 收下了却不显示（苹果的问题），AirPlay、随航、DisplayLink 的显示器没有 gamma 表。
//!   窗口挡不住鼠标指针（它在所有窗口上面），也不一定挡得住锁屏界面；
//! * **挡住键盘鼠标**：一个事件监听（`CGEventTap`，HID 那一层的最前面）丢掉本机硬件来的输入（事件源是
//!   `HIDSystemState` 的：键盘、鼠标、触控板，媒体键、手势也是），放过被控端自己注入的（事件源带着
//!   [`INJECTED`] 记号）和别的程序合成的；松开按键、鼠标键的放过去，开启之前本机按着的不会一直按着。要"辅助功能"
//!   权限（注入输入本来就要它，没有就开不了：[`usable`]）。系统停用了监听（回调超时、本机输入太多）马上重新启用。
//!   挡不住的：密码框有焦点时（"安全输入"）键盘事件不经过事件监听，本机的人还能往里面打字；电源键、合盖。
//!   挡下了本机的人按键、点鼠标、滚动（移动鼠标不算）时告诉 `host::ending`：本机有人要用电脑，断线的控制端不再等它
//!   重连（见 `ending::local_input`；回调里只记一下，监听的线程每 [`TICK`] 看一眼）；
//! * 补屏的虚拟显示器（见 [`super::mac`]）不用调黑、不用盖：本机的人看不到它。
//!
//! 两个线程：事件监听在 `airhand-privacy-input` 的 run loop 上，这个线程不做别的（注入的输入也要经过监听，
//! 和 WindowServer 打交道慢了会耽误它）；`airhand-privacy-screens` 每 [`TICK`] 看一次 gamma 表、显示器有没有变化，
//! 每 [`COVER_REFRESH`] 请主线程把黑窗口放回最前面。关掉时先放开键盘鼠标，再恢复 gamma 表
//! （`CGDisplayRestoreColorSyncSettings`：所有显示器按 ColorSync 的设置恢复），最后撤掉黑窗口。

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, CGRect, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayRestoreColorSyncSettings, CGError, CGEvent,
    CGEventField, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGGammaValue, CGGetDisplayTransferByTable,
    CGSetDisplayTransferByTable,
};
use tracing::{debug, info, warn};

use crate::host::input::{INJECTED, modifier_mask};
use crate::host::macos;

/// 多久看一次 gamma 表、显示器有没有变化（事件监听的 run loop 也是每次跑这么久就看一眼要不要停）
const TICK: Duration = Duration::from_millis(100);
/// 多久把黑窗口重新放到最前面、对一下大小（显示器变了的话马上）
const COVER_REFRESH: Duration = Duration::from_secs(1);
/// 等线程装好事件监听最多这么久
const START_WAIT: Duration = Duration::from_secs(5);
/// 设的 gamma 表的大小
const GAMMA_SIZE: usize = 256;
/// 连着这么多次（每 [`TICK`] 一次）看到不是黑的就先不设了（不然一直在设，可能一直在闪）……
const GAMMA_TRIES: u32 = 10;
/// ……过这么久再试
const GAMMA_RETRY: Duration = Duration::from_secs(10);
/// 黑窗口最多几个（`curtain.m` 的 `MAX_COVERS`）
const MAX_COVERS: usize = 32;

/// 挡下了本机的人按键、点鼠标、滚动（见模块说明）：回调里只记一下，监听的线程再告诉 `ending`
static PRESSED: AtomicBool = AtomicBool::new(false);

/// 挡的事件：键盘、鼠标、触控板。`CGEventType` 里有的，加上只有 `NSEvent` 才有名字的几种
const KINDS: [u32; 26] = [
    CGEventType::LeftMouseDown.0,
    CGEventType::LeftMouseUp.0,
    CGEventType::RightMouseDown.0,
    CGEventType::RightMouseUp.0,
    CGEventType::MouseMoved.0,
    CGEventType::LeftMouseDragged.0,
    CGEventType::RightMouseDragged.0,
    CGEventType::KeyDown.0,
    CGEventType::KeyUp.0,
    CGEventType::FlagsChanged.0,
    CGEventType::ScrollWheel.0,
    CGEventType::TabletPointer.0,
    CGEventType::TabletProximity.0,
    CGEventType::OtherMouseDown.0,
    CGEventType::OtherMouseUp.0,
    CGEventType::OtherMouseDragged.0,
    // NX_SYSDEFINED：媒体键、亮度、音量、调度中心这些功能键
    14,
    // 触控板：旋转、手势开始、手势结束、手势、捏合、轻扫、智能缩放、压感、直接触摸
    18,
    19,
    END_GESTURE,
    29,
    30,
    31,
    32,
    34,
    37,
];
/// `NSEventTypeEndGesture`
const END_GESTURE: u32 = 20;
const MASK: u64 = mask(&KINDS);

const fn mask(kinds: &[u32]) -> u64 {
    let mut m = 0u64;
    let mut i = 0;
    while i < kinds.len() {
        m |= 1 << kinds[i];
        i += 1;
    }
    m
}

unsafe extern "C" {
    fn ah_cover(displays: *const u32, count: usize) -> u64;
    fn ah_cover_done() -> u64;
    fn ah_cover_generation() -> u64;
    fn ah_cover_windows(windows: *mut u32, capacity: usize) -> usize;
}

/// 能开防窥：有"辅助功能"权限（事件监听要它）
pub(super) fn usable() -> bool {
    macos::input_allowed()
}

/// 盖在本机显示器上的黑窗口（窗口编号）：采集时去掉它们
pub(super) fn cover_windows() -> Vec<u32> {
    let mut buf = [0u32; MAX_COVERS];
    let n = unsafe { ah_cover_windows(buf.as_mut_ptr(), buf.len()) };
    buf[..n.min(buf.len())].to_vec()
}

/// 黑窗口变了就加一：在这之前打开的采集要重新打开（去掉新的黑窗口）
pub(super) fn cover_generation() -> u64 {
    unsafe { ah_cover_generation() }
}

/// 请主线程盖上这几个显示器（空的：都撤掉），返回这次请求的编号
fn cover(displays: &[CGDirectDisplayID]) -> u64 {
    unsafe { ah_cover(displays.as_ptr(), displays.len()) }
}

/// 防窥开着（见模块说明）。丢掉时恢复原样：先放开键盘鼠标，再恢复显示器
pub(super) struct Curtain {
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    /// 事件监听的线程（只跑它的 run loop：注入的输入也要经过监听，不能让别的事耽误它）
    input: Option<JoinHandle<()>>,
    /// 调黑、盖黑窗口的线程
    screens: Option<JoinHandle<()>>,
}

impl Curtain {
    /// 挡住本机的键盘鼠标、调黑、盖上黑窗口。装不上事件监听（没有"辅助功能"权限）时失败，什么也不改
    pub fn start() -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let s = stop.clone();
        let input = std::thread::Builder::new()
            .name("airhand-privacy-input".into())
            .spawn(move || block_input(&s, tx))?;
        match rx.recv_timeout(START_WAIT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = input.join();
                return Err(e);
            }
            Err(_) => {
                // 装好了也马上撤掉
                stop.store(true, Ordering::Release);
                return Err(anyhow!(
                    "blocking the local keyboard and mouse took too long"
                ));
            }
        }
        let mut curtain = Self {
            stop: stop.clone(),
            ready: ready.clone(),
            input: Some(input),
            screens: None,
        };
        // 起不来的话丢掉 curtain：放开键盘鼠标
        curtain.screens = Some(
            std::thread::Builder::new()
                .name("airhand-privacy-screens".into())
                .spawn(move || blank_screens(&stop, &ready))?,
        );
        Ok(curtain)
    }

    /// 黑窗口盖上了（主线程处理了第一次的请求）：到这时才算开着
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

impl Drop for Curtain {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for t in [self.input.take(), self.screens.take()]
            .into_iter()
            .flatten()
        {
            let _ = t.join();
        }
    }
}

/// 要调黑、要盖上的显示器：接着的（睡着的、镜像的也算），补屏的虚拟显示器 `ours` 除外
fn targets(
    online: &[CGDirectDisplayID],
    ours: Option<CGDirectDisplayID>,
) -> Vec<CGDirectDisplayID> {
    online
        .iter()
        .copied()
        .filter(|&id| Some(id) != ours)
        .collect()
}

/// 事件监听的线程：装好了告诉 `started`，之后只跑 run loop，直到 `stop`
fn block_input(stop: &AtomicBool, started: mpsc::Sender<Result<()>>) {
    let tap = match Tap::install() {
        Ok(t) => t,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let _ = started.send(Ok(()));
    info!("privacy mode: the local keyboard and mouse are blocked");
    // 上一次防窥留下的不算
    PRESSED.store(false, Ordering::Relaxed);
    while !stop.load(Ordering::Acquire) {
        tap.keep_enabled();
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, TICK.as_secs_f64(), false);
        if PRESSED.swap(false, Ordering::Relaxed) {
            crate::host::ending::local_input();
        }
    }
    drop(tap);
    info!("privacy mode: the local keyboard and mouse work again");
}

/// 调黑、盖黑窗口的线程：每 [`TICK`] 看一次，直到 `stop`；之后恢复 gamma 表、撤掉黑窗口
fn blank_screens(stop: &AtomicBool, ready: &AtomicBool) {
    // 上次盖上的显示器和它们的位置大小：变了就马上重新调黑、重新盖
    let mut layout: Vec<(CGDirectDisplayID, CGRect)> = Vec::new();
    let mut first: Option<u64> = None;
    let mut covered_at: Option<Instant> = None;
    let mut gamma = Gamma::default();
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let ids = targets(&macos::online_display_ids(), super::mac::current());
        let now_layout: Vec<_> = ids.iter().map(|&id| (id, CGDisplayBounds(id))).collect();
        let changed = now_layout != layout;
        gamma.blacken(&ids, changed, now);
        if changed || covered_at.is_none_or(|t| now.duration_since(t) >= COVER_REFRESH) {
            if changed {
                info!(displays = ?ids, "privacy mode: blanking and covering the local displays");
            }
            let ticket = cover(&ids);
            first.get_or_insert(ticket);
            covered_at = Some(now);
            layout = now_layout;
        }
        if !ready.load(Ordering::Relaxed) && first.is_some_and(|t| unsafe { ah_cover_done() } >= t)
        {
            ready.store(true, Ordering::Release);
        }
        std::thread::sleep(TICK);
    }
    CGDisplayRestoreColorSyncSettings();
    cover(&[]);
    info!("privacy mode: the local displays are back");
}

// ───────────────────────────── gamma 表 ─────────────────────────────

/// 把显示器调黑、看着它们一直黑着
#[derive(Default)]
struct Gamma {
    /// 连着几次看到不是黑的、又设了一次（看到黑的就清零）
    tries: HashMap<CGDirectDisplayID, u32>,
    /// 设了也一直不是黑的（读回来的不是设的，或者系统一直在恢复它）：先不管它（有黑窗口盖着），过一会儿再试
    given_up: HashMap<CGDirectDisplayID, Instant>,
    /// 调黑出过错的（只记一次日志）
    failed: HashSet<CGDirectDisplayID>,
}

impl Gamma {
    /// `ids` 里不是黑的设成黑的；`force`：不看，都设一次（显示器变了）
    fn blacken(&mut self, ids: &[CGDirectDisplayID], force: bool, now: Instant) {
        if force {
            self.given_up.clear();
        }
        self.given_up
            .retain(|_, t| now.duration_since(*t) < GAMMA_RETRY);
        for &id in ids {
            if self.given_up.contains_key(&id) {
                continue;
            }
            if !force && is_black(id) {
                self.tries.remove(&id);
                continue;
            }
            let tries = self.tries.entry(id).or_default();
            *tries += 1;
            if *tries > GAMMA_TRIES {
                warn!(
                    id,
                    "the display doesn't stay blank, only the black window covers it"
                );
                self.tries.remove(&id);
                self.given_up.insert(id, now);
                continue;
            }
            let r = set_black(id);
            if r != CGError::Success {
                if self.failed.insert(id) {
                    warn!(id, "can't blank the display: CGError {}", r.0);
                }
            } else if !force && *tries == 1 {
                debug!(
                    id,
                    "the system put the display's gamma back, blanking it again"
                );
            }
        }
    }
}

/// 把显示器调黑：gamma 表全是 0
fn set_black(id: CGDirectDisplayID) -> CGError {
    let zeros = [0.0 as CGGammaValue; GAMMA_SIZE];
    unsafe {
        CGSetDisplayTransferByTable(
            id,
            GAMMA_SIZE as u32,
            zeros.as_ptr(),
            zeros.as_ptr(),
            zeros.as_ptr(),
        )
    }
}

/// 显示器现在的 gamma 表是黑的（读不出来不算）
fn is_black(id: CGDirectDisplayID) -> bool {
    let mut t = [[1.0 as CGGammaValue; GAMMA_SIZE]; 3];
    let mut n = 0u32;
    let [r, g, b] = &mut t;
    let e = unsafe {
        CGGetDisplayTransferByTable(
            id,
            GAMMA_SIZE as u32,
            r.as_mut_ptr(),
            g.as_mut_ptr(),
            b.as_mut_ptr(),
            &mut n,
        )
    };
    let n = (n as usize).min(GAMMA_SIZE);
    e == CGError::Success && n > 0 && t.iter().all(|c| c[..n].iter().all(|&v| v <= 0.001))
}

// ───────────────────────────── 键盘鼠标 ─────────────────────────────

/// 事件监听看到的一个事件里要紧的几项
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    kind: u32,
    /// 被控端自己注入的（带着 [`INJECTED`] 记号）
    ours: bool,
    /// 本机硬件来的（事件源状态是 `HIDSystemState`）
    local: bool,
    keycode: u16,
    flags: u64,
}

/// 挡不挡：本机硬件来的挡，松开的除外
fn blocks(e: &Seen) -> bool {
    e.local && !e.ours && !releases(e)
}

/// 本机的人想用电脑：按键、按下鼠标键、滚动（挡下了才算，见模块说明）。移动鼠标不算：桌子被碰一下也会动
fn presses(e: &Seen) -> bool {
    matches!(
        CGEventType(e.kind),
        CGEventType::KeyDown
            | CGEventType::LeftMouseDown
            | CGEventType::RightMouseDown
            | CGEventType::OtherMouseDown
            | CGEventType::ScrollWheel
    )
}

/// 松开：按键、鼠标键、手势结束、修饰键松开（它的那一位清掉了）
fn releases(e: &Seen) -> bool {
    match CGEventType(e.kind) {
        CGEventType::KeyUp
        | CGEventType::LeftMouseUp
        | CGEventType::RightMouseUp
        | CGEventType::OtherMouseUp => true,
        CGEventType::FlagsChanged => modifier_mask(e.keycode).is_some_and(|m| e.flags & m == 0),
        _ => e.kind == END_GESTURE,
    }
}

/// 装在线程的 run loop 上的事件监听。丢掉时撤掉
struct Tap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    run_loop: CFRetained<CFRunLoop>,
    /// 回调里要重新启用监听：回调拿到的是指向这一格的指针，监听撤掉之前一直在
    _cell: Box<AtomicPtr<CFMachPort>>,
}

impl Tap {
    fn install() -> Result<Self> {
        let cell = Box::new(AtomicPtr::new(null_mut()));
        let info = (&*cell as *const AtomicPtr<CFMachPort>)
            .cast_mut()
            .cast::<c_void>();
        let create = |at| unsafe {
            CGEvent::tap_create(
                at,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                MASK,
                Some(filter),
                info,
            )
        };
        // HID 那一层最早（注入的输入也从这里进来，带着记号放过去）；装不上的话装在会话那一层
        let port = create(CGEventTapLocation::HIDEventTap)
            .or_else(|| create(CGEventTapLocation::SessionEventTap))
            .ok_or_else(|| {
                anyhow!("can't block the local keyboard and mouse (the Accessibility permission isn't granted?)")
            })?;
        cell.store(CFRetained::as_ptr(&port).as_ptr(), Ordering::Release);
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0).ok_or_else(|| {
            anyhow!("can't block the local keyboard and mouse (no run loop source)")
        })?;
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| anyhow!("can't block the local keyboard and mouse (no run loop)"))?;
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        CGEvent::tap_enable(&port, true);
        Ok(Self {
            port,
            source,
            run_loop,
            _cell: cell,
        })
    }

    /// 被系统停用了（回调里没来得及重新启用的话）：重新启用
    fn keep_enabled(&self) {
        if !CGEvent::tap_is_enabled(&self.port) {
            warn!("blocking the local input was turned off by the system, turning it back on");
            CGEvent::tap_enable(&self.port, true);
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        CGEvent::tap_enable(&self.port, false);
        self.run_loop
            .remove_source(Some(&self.source), unsafe { kCFRunLoopDefaultMode });
        self.port.invalidate();
    }
}

/// 事件监听的回调（在装它的线程的 run loop 里调用）：返回原来的事件就放过去，返回空就丢掉
unsafe extern "C-unwind" fn filter(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    let pass = event.as_ptr();
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        // 回调太慢、本机的输入太多时系统会停用监听：马上重新启用
        let port = unsafe { &*info.cast::<AtomicPtr<CFMachPort>>() }.load(Ordering::Acquire);
        if let Some(port) = unsafe { port.as_ref() } {
            CGEvent::tap_enable(port, true);
        }
        return pass;
    }
    let ev = unsafe { event.as_ref() };
    let field = |f| CGEvent::integer_value_field(Some(ev), f);
    let seen = Seen {
        kind: kind.0,
        ours: field(CGEventField::EventSourceUserData) == INJECTED,
        local: field(CGEventField::EventSourceStateID)
            == CGEventSourceStateID::HIDSystemState.0 as i64,
        keycode: field(CGEventField::KeyboardEventKeycode) as u16,
        flags: CGEvent::flags(Some(ev)).0,
    };
    if !blocks(&seen) {
        return pass;
    }
    if presses(&seen) {
        PRESSED.store(true, Ordering::Relaxed);
    }
    null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(kind: CGEventType) -> Seen {
        Seen {
            kind: kind.0,
            local: true,
            ..Seen::default()
        }
    }

    /// 本机硬件来的按下、移动、滚动、手势挡住；松开的放过去；被控端注入的、别的程序合成的放过去
    #[test]
    fn blocks_local_input_only() {
        for kind in [
            CGEventType::KeyDown,
            CGEventType::LeftMouseDown,
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::ScrollWheel,
            CGEventType(14),
            CGEventType(29),
        ] {
            assert!(blocks(&local(kind)), "{kind:?}");
            let ours = Seen {
                ours: true,
                ..local(kind)
            };
            assert!(!blocks(&ours), "注入的 {kind:?}");
            let synthetic = Seen {
                local: false,
                ..local(kind)
            };
            assert!(!blocks(&synthetic), "合成的 {kind:?}");
        }
        for kind in [
            CGEventType::KeyUp,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseUp,
            CGEventType(END_GESTURE),
        ] {
            assert!(!blocks(&local(kind)), "松开 {kind:?}");
        }
    }

    /// 按键、按下鼠标键、滚动算本机有人想用电脑；移动、拖动、松开不算
    #[test]
    fn presses_are_someone_at_the_computer() {
        for kind in [
            CGEventType::KeyDown,
            CGEventType::LeftMouseDown,
            CGEventType::RightMouseDown,
            CGEventType::OtherMouseDown,
            CGEventType::ScrollWheel,
        ] {
            assert!(presses(&local(kind)), "{kind:?}");
        }
        for kind in [
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::KeyUp,
            CGEventType::LeftMouseUp,
            CGEventType::FlagsChanged,
        ] {
            assert!(!presses(&local(kind)), "{kind:?}");
        }
    }

    /// 修饰键：按下（它的那一位在）挡住，松开（那一位清掉了）放过去；不是修饰键的挡住
    #[test]
    fn modifier_presses_are_blocked_releases_pass() {
        let shift = modifier_mask(0x38).unwrap();
        let press = Seen {
            keycode: 0x38,
            flags: shift,
            ..local(CGEventType::FlagsChanged)
        };
        assert!(blocks(&press));
        // 右 Shift 还按着，左 Shift 松开了：设备无关的那一位还在，算按着（挡住也没关系）
        let release = Seen { flags: 0, ..press };
        assert!(!blocks(&release));
        let command = Seen {
            keycode: 0x37,
            flags: shift,
            ..press
        };
        assert!(!blocks(&command), "Command 松开了，Shift 还按着");
        let unknown = Seen {
            keycode: 0x00,
            flags: 0,
            ..press
        };
        assert!(blocks(&unknown));
    }

    #[test]
    fn the_mask_covers_every_kind() {
        for k in KINDS {
            assert!(k < 64);
            assert_ne!(MASK & (1 << k), 0, "{k}");
        }
        // 只有输入事件：不挡别的（例如 Null = 0）
        assert_eq!(MASK & 1, 0);
        assert_eq!(MASK.count_ones() as usize, KINDS.len());
    }

    /// 补屏的虚拟显示器不调黑、不盖
    #[test]
    fn our_virtual_display_is_left_alone() {
        assert_eq!(targets(&[1, 2, 3], Some(2)), [1, 3]);
        assert_eq!(targets(&[1, 2], None), [1, 2]);
        assert!(targets(&[2], Some(2)).is_empty());
    }

    /// 真的调黑本机的主显示器 1 秒：读回来是黑的，恢复之后不再是黑的（本机屏幕会黑 1 秒，测试进程崩溃了系统也会
    /// 恢复）。采集不受影响见 `capture::sck` 的 `gamma_is_not_captured`。
    /// `cargo test --lib blanks_and_restores -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn blanks_and_restores() {
        let Some(main) = macos::displays().into_iter().next() else {
            println!("没有显示器");
            return;
        };
        assert!(!is_black(main.id), "开始之前就是黑的");
        assert_eq!(set_black(main.id), CGError::Success);
        std::thread::sleep(Duration::from_secs(1));
        let black = is_black(main.id);
        CGDisplayRestoreColorSyncSettings();
        std::thread::sleep(Duration::from_millis(200));
        assert!(black, "设了读回来不是黑的");
        assert!(!is_black(main.id), "恢复之后还是黑的");
    }
}
