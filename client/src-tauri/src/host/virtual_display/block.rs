//! 防窥模式时挡住本机的键盘鼠标：低级键盘、鼠标钩子（`WH_KEYBOARD_LL` / `WH_MOUSE_LL`）丢掉本机的输入，
//! 只放过被控端自己注入的（带着 [`INJECTED`] 记号的，见 `host::input`）。
//!
//! * 不用 `BlockInput`：它只放过调用它的那个线程注入的输入，按 Ctrl+Alt+Del、切换桌面时系统还会自己解除；
//! * 钩子只管装它的线程所在的桌面。UAC 确认框、锁屏在安全桌面上：输入桌面换了就卸下钩子、换过去、重新装上
//!   （线程上有钩子时不能切换桌面，见 `host::desktop`）。被控端以 SYSTEM 身份运行，安全桌面上也装得上；
//! * 松开按键、松开鼠标键的事件放过去：开启之前本机按着的键不会一直按着（别的程序看到的就是按着的）；
//! * 拦不住 Ctrl+Alt+Del（系统的安全注意序列，不经过钩子）。按了也只是切到安全桌面，本机的显示器都撤下了，看不到；
//! * 挡下了本机的人按键、点鼠标、滚滚轮（移动鼠标不算：桌子被碰一下也会动）时告诉 `host::ending`：本机有人要用电脑，
//!   断线的控制端不再等它重连（见 `ending::local_input`）；
//! * 钩子的回调在这个线程的消息循环里调用：回调很快就返回（只记一下有没有人按过，消息循环里再告诉 `ending`），
//!   系统也就不会因为超时把钩子摘掉。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::{info, warn};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WAIT_TIMEOUT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, QS_ALLINPUT, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_NULL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::host::input::INJECTED;

/// 挡下了本机的人按键、点鼠标、滚滚轮：钩子里只记一下，消息循环里再告诉 `ending`（见模块说明）
static PRESSED: AtomicBool = AtomicBool::new(false);

/// 本机的输入被挡住了（[`Blocker`] 开着期间）。Drop 时放开
pub(super) struct Blocker {
    stop: Arc<AtomicBool>,
    thread_id: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Blocker {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (s, t) = (stop.clone(), thread_id.clone());
        let thread = std::thread::Builder::new()
            .name("airhand-privacy-input".into())
            .spawn(move || run(&s, &t))
            .map_err(|e| warn!("can't block the local input: {e}"))
            .ok();
        info!("blocking the local keyboard and mouse");
        Self {
            stop,
            thread_id,
            thread,
        }
    }
}

impl Drop for Blocker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            // 叫醒它的消息循环
            let _ = unsafe { PostThreadMessageW(id, WM_NULL, WPARAM(0), LPARAM(0)) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        info!("the local keyboard and mouse work again");
    }
}

/// 装着的两个钩子
struct Hooks(Vec<HHOOK>);

impl Hooks {
    fn install() -> Self {
        let module = unsafe { GetModuleHandleW(None) }
            .ok()
            .map(|m| HINSTANCE(m.0));
        let mut hooks = Vec::new();
        for (kind, proc) in [
            (
                WH_KEYBOARD_LL,
                keyboard as unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT,
            ),
            (WH_MOUSE_LL, mouse),
        ] {
            match unsafe { SetWindowsHookExW(kind, Some(proc), module, 0) } {
                Ok(h) => hooks.push(h),
                Err(e) => warn!("can't block the local input ({kind:?}): {e}"),
            }
        }
        Self(hooks)
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            let _ = unsafe { UnhookWindowsHookEx(h) };
        }
    }
}

fn run(stop: &AtomicBool, thread_id: &AtomicU32) {
    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    // 上一次防窥留下的不算
    PRESSED.store(false, Ordering::Relaxed);
    let mut hooks: Option<Hooks> = None;
    while !stop.load(Ordering::Acquire) {
        if !crate::host::desktop::on_input_desktop() {
            // 换到安全桌面（或者从它回来）：先卸下钩子才能切换
            hooks = None;
            crate::host::desktop::follow();
        }
        if hooks.is_none() {
            hooks = Some(Hooks::install());
        }
        let r = unsafe {
            MsgWaitForMultipleObjects(
                None,
                false,
                Duration::from_millis(250).as_millis() as u32,
                QS_ALLINPUT,
            )
        };
        if r != WAIT_TIMEOUT {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { DispatchMessageW(&msg) };
            }
        }
        if PRESSED.swap(false, Ordering::Relaxed) {
            crate::host::ending::local_input();
        }
    }
    drop(hooks);
}

/// 放过：被控端自己注入的，或者松开（按着的键不会一直按着）
unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let ours = k.flags.contains(LLKHF_INJECTED) && k.dwExtraInfo == INJECTED;
        let release = matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP);
        if !ours && !release {
            if matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                PRESSED.store(true, Ordering::Relaxed);
            }
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let m = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let ours = m.flags & LLMHF_INJECTED != 0 && m.dwExtraInfo == INJECTED;
        let release = matches!(
            wparam.0 as u32,
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP
        );
        if !ours && !release {
            if matches!(
                wparam.0 as u32,
                WM_LBUTTONDOWN
                    | WM_RBUTTONDOWN
                    | WM_MBUTTONDOWN
                    | WM_XBUTTONDOWN
                    | WM_MOUSEWHEEL
                    | WM_MOUSEHWHEEL
            ) {
                PRESSED.store(true, Ordering::Relaxed);
            }
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
