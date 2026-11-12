//! 控制台窗口全屏。
//!
//! 全屏时标签栏藏起来，工具栏悬浮在顶端，鼠标碰到屏幕顶端时出现（见界面的 SessionView），
//! 所以全屏期间网页必须能收到屏幕最顶端的鼠标移动。Windows / Linux 的控制台是无边框窗口，
//! Tauri 为了能拖动窗口边缘改大小，会拦下窗口边缘一圈的鼠标事件，全屏时也照拦不误：
//!
//! * Windows：窗口上盖了一层透明子窗口（`TAURI_DRAG_RESIZE_WINDOW`），挡住顶端的几个像素。
//!   窗口变为"不可调整大小"时 Tauri 会拿掉它，所以全屏期间关掉可调整大小；
//! * Linux：webview 的 `motion-notify-event` 上接了处理函数，鼠标在边缘 5px（乘以缩放）以内时
//!   换成改大小的光标并吞掉事件。全屏期间暂停这个信号上接的处理函数（WebKit 自己处理鼠标
//!   用的是类的默认处理，不受影响）。不能用关掉可调整大小的办法：GTK 会把不可调整大小的窗口
//!   固定成最小尺寸，全屏窗口可能缩小；
//! * macOS：保留了系统标题栏，没有这个问题。但系统全屏时鼠标碰到顶端，菜单栏会连同窗口标题栏
//!   一起滑下来盖住工具栏，碰到底端程序坞会弹出来，所以不用系统全屏，自己把窗口铺满屏幕，见 [`mac`]。
//!
//! 顺序：先全屏再停用边缘，先恢复边缘再退出全屏。Windows 退出全屏时如果窗口回到最大化，
//! 要靠退出全屏引起的 WM_SIZE 让 Tauri 收起拖动区，所以要先恢复。

use tauri::WebviewWindow;

#[cfg(target_os = "macos")]
pub use mac::{reset, set};

/// 窗口是不是全屏（Tauri 的 `is_fullscreen` 不知道 macOS 上自己做的全屏）
pub fn is_on(window: &WebviewWindow) -> tauri::Result<bool> {
    #[cfg(target_os = "macos")]
    if mac::ACTIVE.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(true);
    }
    window.is_fullscreen()
}

#[cfg(not(target_os = "macos"))]
pub fn set(window: &WebviewWindow, on: bool) -> tauri::Result<()> {
    if on {
        window.set_fullscreen(true)?;
        pause_edge_resize(window, true)
    } else {
        pause_edge_resize(window, false)?;
        window.set_fullscreen(false)
    }
}

#[cfg(windows)]
fn pause_edge_resize(window: &WebviewWindow, paused: bool) -> tauri::Result<()> {
    window.set_resizable(!paused)
}

#[cfg(target_os = "linux")]
fn pause_edge_resize(window: &WebviewWindow, paused: bool) -> tauri::Result<()> {
    use glib::gobject_ffi;
    use glib::prelude::*;
    use glib::translate::IntoGlib;

    // 记在 webview 上：屏蔽和解除要配对（重复全屏、控制台关掉重开都不会弄乱）
    const PAUSED: &str = "airhand-edge-resize-paused";

    window.with_webview(move |wv| {
        let view = wv.inner();
        let obj = view.upcast_ref::<glib::Object>();
        // SAFETY: 只在主线程上读写自己的 key，类型始终是 bool
        let was = unsafe { obj.data::<bool>(PAUSED).is_some_and(|p| *p.as_ref()) };
        if was == paused {
            return;
        }
        unsafe {
            obj.set_data(PAUSED, paused);
            let signal = gobject_ffi::g_signal_lookup(
                c"motion-notify-event".as_ptr(),
                obj.type_().into_glib(),
            );
            if signal == 0 {
                return;
            }
            let block_or_unblock = if paused {
                gobject_ffi::g_signal_handlers_block_matched
            } else {
                gobject_ffi::g_signal_handlers_unblock_matched
            };
            use std::ptr::null_mut;
            block_or_unblock(
                obj.as_ptr().cast(),
                gobject_ffi::G_SIGNAL_MATCH_ID,
                signal,
                0,
                null_mut(),
                null_mut(),
                null_mut(),
            );
        }
    })
}

/// macOS：从界面进入的全屏不用系统全屏，自己把窗口铺满屏幕，系统菜单栏和程序坞完全不出现。
///
/// 系统全屏（单独的桌面空间）里，鼠标碰到顶端，菜单栏连同窗口标题栏滑下来，正好盖住悬浮工具栏；
/// 碰到底端，程序坞弹出来，挡住远程画面底部的任务栏。全屏空间里能藏起菜单栏，但程序坞不听：
/// macOS 27 上实测窗口代理回答 HideDock、再直接设置 App 的显示选项、再加上 DisableProcessSwitching，
/// 碰到底端程序坞都照样弹出。不在全屏空间里时 HideDock | HideMenuBar 两样都管用。
///
/// 也不能用 tao 的简单全屏（`set_simple_fullscreen`）：它拿掉窗口的 Titled 样式，窗口换成无边框的外框视图，
/// 之后 AppKit 不再把鼠标移动交给 WKWebView 的 tracking area，网页收不到 pointermove（只有点击能到，
/// 鼠标碰到顶端也不会出工具栏），键盘焦点也丢了；退出全屏、样式改回来之后还是这样。所以保留 Titled：
///
/// * 记下窗口原来的位置大小、样式、阴影和 App 的显示选项；藏起红绿灯，去掉可调整大小、不能拖动，
///   铺满窗口所在的屏幕；显示选项改成 HideDock | HideMenuBar。退出时一样一样恢复；
/// * 带标题栏的窗口有圆角，铺满屏幕时四个角的画面会被裁掉、露出后面的东西。所以窗口比屏幕四周各大出
///   [`OVERSCAN`]，圆角落到屏幕外面；WKWebView 在窗口里往里缩同样多，网页还是正好和屏幕一样大
///   （鼠标坐标、顶端热区、刘海都不受影响）。带标题栏的窗口上边本来不许超出屏幕，见 [`allow_offscreen`]。
///   万一伸不出去、或者以后系统的圆角比这还大，后面还垫着一个黑色的无边框窗口（不接收鼠标），
///   露出来的也只是黑色。阴影关掉，不然窗口边上有一圈亮线；
/// * 显示选项只在 App 处于前台时生效：切到别的 App 时菜单栏、程序坞照常，切回来又藏起；
/// * 带刘海的屏幕：刘海在网页顶端。告诉网页刘海那一条有多高（`--fullscreen-top`），内容放到它下面、
///   留一条黑边（和系统全屏一样），不然工具栏在刘海后面；
/// * 控制台在全屏中被关掉（例如从托盘切换为被控端）时由 [`reset`] 恢复显示选项、收起黑底；
/// * 点窗口的绿色按钮进入的是系统全屏，照常显示菜单栏；这时这里什么都不改。
#[cfg(target_os = "macos")]
mod mac {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    use std::ffi::CString;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, Imp, Sel};
    use objc2::{ClassType, Encode, MainThreadMarker, MainThreadOnly, msg_send, sel};
    use objc2_app_kit::{
        NSApp, NSApplicationPresentationOptions as Options, NSBackingStoreType, NSColor, NSScreen,
        NSView, NSWindow, NSWindowButton, NSWindowCollectionBehavior, NSWindowOrderingMode,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
    use tauri::{AppHandle, WebviewWindow};
    use tracing::warn;

    /// 控制台在从界面进入的全屏里
    pub static ACTIVE: AtomicBool = AtomicBool::new(false);

    /// 进入全屏之前的样子
    struct Saved {
        /// 全屏的是哪个窗口（只用来比较）
        window: *const NSWindow,
        frame: NSRect,
        style: NSWindowStyleMask,
        movable: bool,
        shadow: bool,
        options: Options,
        /// 垫在窗口后面的黑底
        backstop: Retained<NSWindow>,
    }

    thread_local! {
        /// 只在主线程上用
        static SAVED: RefCell<Option<Saved>> = const { RefCell::new(None) };
    }

    /// 全屏时窗口比屏幕四周各大出多少（pt），要比窗口的圆角大（macOS 27 上控制台窗口的圆角实测约 16pt）
    const OVERSCAN: f64 = 24.0;

    const BUTTONS: [NSWindowButton; 3] = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ];

    pub fn set(window: &WebviewWindow, on: bool) -> tauri::Result<()> {
        let w = window.clone();
        window.run_on_main_thread(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let Ok(ptr) = w.ns_window() else {
                return;
            };
            // SAFETY: Tauri 给的是这个窗口的 NSWindow，在主线程上使用
            let Some(ns_window) = (unsafe { ptr.cast::<NSWindow>().as_ref() }) else {
                return;
            };
            if on {
                enter(&w, ns_window, mtm);
            } else {
                leave(&w, ns_window, mtm);
            }
        })
    }

    fn enter(w: &WebviewWindow, window: &NSWindow, mtm: MainThreadMarker) {
        // 已经全屏了（点绿色按钮进入的系统全屏也算）
        if SAVED.with_borrow(Option::is_some)
            || window.styleMask().contains(NSWindowStyleMask::FullScreen)
        {
            return;
        }
        let Some(screen) = window.screen() else {
            return;
        };
        let frame = screen.frame();
        let app = NSApp(mtm);
        let backstop = backstop(frame, mtm);
        let overscanned = NSRect::new(
            NSPoint::new(frame.origin.x - OVERSCAN, frame.origin.y - OVERSCAN),
            NSSize::new(
                frame.size.width + 2.0 * OVERSCAN,
                frame.size.height + 2.0 * OVERSCAN,
            ),
        );
        allow_offscreen();
        SAVED.set(Some(Saved {
            window,
            frame: window.frame(),
            style: window.styleMask(),
            movable: window.isMovable(),
            shadow: window.hasShadow(),
            options: app.presentationOptions(),
            backstop: backstop.clone(),
        }));
        ACTIVE.store(true, Ordering::Release);

        app.setPresentationOptions(Options::HideDock | Options::HideMenuBar);
        for b in BUTTONS {
            if let Some(b) = window.standardWindowButton(b) {
                b.setHidden(true);
            }
        }
        window.setStyleMask(window.styleMask() & !NSWindowStyleMask::Resizable);
        window.setMovable(false);
        window.setHasShadow(false);
        window.setFrame_display(overscanned, true);
        inset_webview(w, OVERSCAN);
        backstop.orderWindow_relativeTo(NSWindowOrderingMode::Below, window.windowNumber());

        let top = notch_height(&screen);
        let _ = w.eval(format!(
            "document.documentElement.style.setProperty('--fullscreen-top', '{top}px')"
        ));
    }

    fn leave(w: &WebviewWindow, window: &NSWindow, mtm: MainThreadMarker) {
        let Some(saved) = SAVED.take() else {
            return;
        };
        ACTIVE.store(false, Ordering::Release);
        window.setStyleMask(saved.style);
        for b in BUTTONS {
            if let Some(b) = window.standardWindowButton(b) {
                b.setHidden(false);
            }
        }
        window.setMovable(saved.movable);
        window.setHasShadow(saved.shadow);
        window.setFrame_display(saved.frame, true);
        inset_webview(w, 0.0);
        saved.backstop.close();
        NSApp(mtm).setPresentationOptions(saved.options);
    }

    /// 控制台在全屏中被关掉时恢复显示选项、收起黑底
    pub fn reset(app: &AppHandle) {
        let _ = app.run_on_main_thread(|| {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            if let Some(saved) = SAVED.take() {
                ACTIVE.store(false, Ordering::Release);
                saved.backstop.close();
                NSApp(mtm).setPresentationOptions(saved.options);
            }
        });
    }

    /// 铺满屏幕的黑色无边框窗口：不接收鼠标，不出现在窗口列表、调度中心里
    fn backstop(frame: NSRect, mtm: MainThreadMarker) -> Retained<NSWindow> {
        // SAFETY: 普通的无边框窗口；关掉后不自动释放，由 Retained 管理
        let w = unsafe {
            let w = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            );
            w.setReleasedWhenClosed(false);
            w
        };
        w.setBackgroundColor(Some(&NSColor::blackColor()));
        w.setIgnoresMouseEvents(true);
        w.setHasShadow(false);
        w.setExcludedFromWindowsMenu(true);
        w.setCollectionBehavior(
            NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle,
        );
        w
    }

    /// 给 tao 的窗口类加上 `constrainFrameRect:toScreen:`（只加一次）。带标题栏的窗口改位置大小时，
    /// 系统用它把窗口限制在屏幕里（上边不能超出屏幕顶端），全屏时窗口四周要伸出屏幕：全屏中的控制台
    /// 原样返回，其余照旧交给 NSWindow。只是添加，tao 自己有这个方法时加不上，就不管了
    fn allow_offscreen() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let Some(class) = AnyClass::get(c"TaoWindow") else {
                warn!("tao's window class not found: fullscreen keeps rounded corners");
                return;
            };
            type Constrain =
                unsafe extern "C-unwind" fn(&NSWindow, Sel, NSRect, *mut NSScreen) -> NSRect;
            let types = CString::new(format!("{0}@:{0}@", NSRect::ENCODING)).unwrap();
            // SAFETY: 参数和返回值与 NSWindow 的 constrainFrameRect:toScreen: 相同
            let added = unsafe {
                let imp = std::mem::transmute::<Constrain, Imp>(constrain);
                objc2::ffi::class_addMethod(
                    (class as *const AnyClass).cast_mut(),
                    sel!(constrainFrameRect:toScreen:),
                    imp,
                    types.as_ptr(),
                )
            };
            if !added.as_bool() {
                warn!(
                    "tao's window already constrains its frame: fullscreen keeps rounded corners"
                );
            }
        });
    }

    unsafe extern "C-unwind" fn constrain(
        this: &NSWindow,
        _cmd: Sel,
        frame: NSRect,
        screen: *mut NSScreen,
    ) -> NSRect {
        if SAVED.with_borrow(|s| s.as_ref().is_some_and(|s| std::ptr::eq(s.window, this))) {
            return frame;
        }
        // SAFETY: 原样交给 NSWindow 的实现
        unsafe {
            msg_send![super(this, NSWindow::class()), constrainFrameRect: frame, toScreen: screen.as_ref()]
        }
    }

    /// WKWebView 离窗口内容区四边的距离（它跟着窗口改大小时保持这个距离）
    fn inset_webview(w: &WebviewWindow, inset: f64) {
        let _ = w.with_webview(move |wv| {
            // SAFETY: 在主线程上；inner 是 WKWebView
            let Some(view) = (unsafe { wv.inner().cast::<NSView>().as_ref() }) else {
                return;
            };
            // SAFETY: 只是拿父视图来量大小（它是窗口的 contentView）
            let Some(parent) = (unsafe { view.superview() }) else {
                return;
            };
            let b = parent.bounds();
            view.setFrame(NSRect::new(
                NSPoint::new(b.origin.x + inset, b.origin.y + inset),
                NSSize::new(b.size.width - 2.0 * inset, b.size.height - 2.0 * inset),
            ));
        });
    }

    /// 屏幕顶端被刘海占掉的高度（没有刘海是 0）
    fn notch_height(screen: &NSScreen) -> f64 {
        // macOS 12 起才有
        if screen.respondsToSelector(sel!(safeAreaInsets)) {
            screen.safeAreaInsets().top
        } else {
            0.0
        }
    }
}
