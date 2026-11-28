//! 控制台移动系统指针：原始大小显示时的边缘滚动，画面推过去多少，系统指针就退回多少
//! （见界面的 client/src/lib/scroll-style.ts）。移不了时返回错误，界面改为持续滚动。
//!
//! 坐标是物理像素，相对网页的左上角。不直接用 Tauri 的 `setCursorPosition`：
//!
//! * Tauri 不管 tao 移没移成，总是报告成功，界面就不知道该改为持续滚动；
//! * Linux：Wayland 不让程序移动指针（GTK 的 warp 什么也不做），tao 照样返回成功。X11 上 tao 只是把请求排进事件循环，
//!   返回时指针还没动；缩放不是 1 时还把逻辑坐标加在物理的窗口位置上。所以 Wayland 桌面直接报错，
//!   X11 用 x11rb 移（根窗口坐标就是物理像素），等 X 服务器处理完再返回；
//! * macOS 自己做的全屏：网页在窗口里往里缩了一圈（见 [`crate::fullscreen`]），要加回去。
//!
//! Windows、macOS 移动指针不会因为权限之类的原因失败，还是交给 tao。

use anyhow::Result;
use tauri::WebviewWindow;

#[cfg(windows)]
pub fn warp(window: &WebviewWindow, x: f64, y: f64) -> Result<()> {
    // 网页铺满无边框窗口的内容区
    window.set_cursor_position(tauri::PhysicalPosition::new(x, y))?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn warp(window: &WebviewWindow, x: f64, y: f64) -> Result<()> {
    // 网页铺满窗口的内容区（透明标题栏：内容区包括标题栏那一条），自己做的全屏时往里缩了一圈
    let scale = window.scale_factor()?;
    let inset = crate::fullscreen::webview_inset();
    window.set_cursor_position(tauri::LogicalPosition::new(
        x / scale + inset,
        y / scale + inset,
    ))?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn warp(window: &WebviewWindow, x: f64, y: f64) -> Result<()> {
    use std::cell::RefCell;

    use anyhow::{Context, bail};
    use x11rb::protocol::xproto::ConnectionExt as _;

    use crate::host::x11::X11;

    thread_local! {
        /// 和 X 服务器的连接，连上了就留着（命令在主线程上执行）
        static X: RefCell<Option<X11>> = const { RefCell::new(None) };
    }

    if wayland() {
        bail!("programs can't move the pointer in a Wayland session");
    }
    // 窗口内容区（网页铺满它）的左上角：tao 记的是 GTK 的逻辑坐标乘以缩放，在 X11 上就是根窗口上的物理像素
    let origin = window.inner_position()?;
    let at = |o: i32, v: f64| {
        i16::try_from(o.saturating_add(v.round() as i32)).context("the position is off the screen")
    };
    let (to_x, to_y) = (at(origin.x, x)?, at(origin.y, y)?);
    X.with_borrow_mut(|conn| {
        if conn.is_none() {
            *conn = Some(X11::connect()?);
        }
        let x11 = conn.as_ref().expect("connected above");
        let moved = x11
            .conn
            .warp_pointer(x11rb::NONE, x11.root(), 0, 0, 0, 0, to_x, to_y)
            .map_err(anyhow::Error::from)
            // 等服务器移好了再返回：界面在这之后才推下一下
            .and_then(|_| x11.sync());
        if moved.is_err() {
            // 连接断了：下次重新连
            *conn = None;
        }
        moved
    })
}

/// Wayland 桌面。经 XWayland 运行时也算：XWayland 一般也移不了合成器的指针
#[cfg(target_os = "linux")]
fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}
