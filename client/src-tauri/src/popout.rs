//! 弹出窗口：控制台把对方的一个显示器放到单独的窗口里（多屏，见界面的 `client/src/lib/popout.ts`）。
//! 本机也有好几个显示器时，一个窗口放一个显示器，各自全屏。
//!
//! 界面用 `window.open("about:blank#popout-…")` 打开。Tauri 默认不让网页开新窗口，控制台窗口的
//! `on_new_window`（[`on_new_window`]）把它建成一个普通的 Tauri 窗口，和控制台共用同一个 WebView 环境：
//! 两边的网页互相能访问，窗口里的东西都由控制台建、控制台管（画面也还是控制台的 Worker 画的，
//! 不用另外连一次对方）。
//!
//! 网址 # 后面是界面选好的窗口 label：之后全屏、关闭都由控制台按它调这里的命令
//! （网页全屏不会让 WebView2 的窗口变大；`window.close()` 在 macOS 上没有反应，在 Linux 上只关掉网页、留下空窗口）。
//!
//! 弹出的窗口带系统的标题栏（不像控制台是无边框的），全屏就是窗口本身的全屏：没有控制台那样被拖动边缘
//! 改大小拦住顶端的问题（见 [`crate::fullscreen`]）。macOS 上是系统全屏（单独的桌面空间）。

use tauri::webview::{Color, NewWindowFeatures, NewWindowResponse};
use tauri::{
    AppHandle, Manager, Monitor, PhysicalPosition, PhysicalSize, Url, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use tracing::warn;

/// 弹出窗口的 label 都以它开头（界面在网址的 # 后面给出完整的 label）
const PREFIX: &str = "popout-";
/// 新窗口占它所在显示器可用区域的多少
const SHARE: f64 = 0.8;

/// 控制台窗口里的网页要开新窗口：只认界面开的弹出窗口（`about:blank#popout-…`），别的一律不开
pub fn on_new_window(
    app: &AppHandle,
    url: Url,
    features: NewWindowFeatures,
) -> NewWindowResponse<tauri::Wry> {
    let Some(label) = label_of(&url) else {
        warn!(%url, "refusing to open a window");
        return NewWindowResponse::Deny;
    };
    if app.get_webview_window(&label).is_some() {
        warn!(label, "popout window already exists");
        return NewWindowResponse::Deny;
    }
    // 先不显示：摆好位置再显示，免得先在控制台上面闪一下
    let builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(blank()))
        // 和控制台共用 WebView 的环境（Windows）、配置（macOS）、关联的 webview（Linux），界面才能访问它
        .window_features(features)
        .title("Airhand")
        .min_inner_size(480.0, 300.0)
        .visible(false)
        .disable_drag_drop_handler()
        .background_color(Color(5, 6, 10, 255))
        .theme(Some(tauri::Theme::Dark))
        // 标题由界面写在网页的 <title> 里（哪台电脑的第几个显示器，跟着界面语言）
        .on_document_title_changed(|window, title| {
            let _ = window.set_title(&title);
        });
    // 控制台是隐私模式（InPrivate）：弹出窗口要和它在同一个配置里，WebView2 才肯把它当作 window.open 的结果
    #[cfg(windows)]
    let builder = builder.incognito(true);
    match builder.build() {
        Ok(window) => {
            place(app, &window);
            let _ = window.show();
            let _ = window.set_focus();
            NewWindowResponse::Create { window }
        }
        Err(e) => {
            warn!("can't open a popout window: {e}");
            NewWindowResponse::Deny
        }
    }
}

fn blank() -> Url {
    Url::parse("about:blank").expect("about:blank is a valid URL")
}

/// `about:blank#popout-xxx` 里的 label（只允许小写字母、数字和 -，不太长）
fn label_of(url: &Url) -> Option<String> {
    if url.scheme() != "about" || url.path() != "blank" {
        return None;
    }
    let label = url.fragment()?;
    let rest = label.strip_prefix(PREFIX)?;
    let ok = !rest.is_empty()
        && label.len() <= 64
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then(|| label.to_owned())
}

/// 界面给的 label 对应的弹出窗口（不是弹出窗口的 label 一律当作没有：这些命令不能拿来操作控制台）
pub fn window(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    if !label.starts_with(PREFIX) {
        return None;
    }
    app.get_webview_window(label)
}

/// 关掉所有弹出窗口（控制台关掉了、程序退出）
pub fn close_all(app: &AppHandle) {
    for (label, window) in app.webview_windows() {
        if label.starts_with(PREFIX) {
            let _ = window.destroy();
        }
    }
}

/// 新窗口放在哪：本机有好几个显示器时放到控制台不在、也还没有弹出窗口的那个显示器上（都有了就是控制台不在的那个），
/// 只有一个显示器时放在控制台所在的显示器上。占那个显示器可用区域的 [`SHARE`]，居中
fn place(app: &AppHandle, window: &WebviewWindow) {
    let monitors = window.available_monitors().unwrap_or_default();
    let console = app
        .get_webview_window(crate::CONSOLE)
        .and_then(|c| c.current_monitor().ok().flatten());
    let taken: Vec<Monitor> = app
        .webview_windows()
        .into_iter()
        .filter(|(label, _)| label.starts_with(PREFIX) && label != window.label())
        .filter_map(|(_, w)| w.current_monitor().ok().flatten())
        .collect();
    let same = |a: &Monitor, b: &Monitor| a.position() == b.position() && a.size() == b.size();
    let not_console = |m: &&Monitor| console.as_ref().is_none_or(|c| !same(m, c));
    let target = monitors
        .iter()
        .filter(not_console)
        .find(|m| !taken.iter().any(|t| same(m, t)))
        .or_else(|| monitors.iter().find(not_console))
        .or(console.as_ref());
    let Some(target) = target else {
        return;
    };
    let area = target.work_area();
    let w = (f64::from(area.size.width) * SHARE).round() as u32;
    let h = (f64::from(area.size.height) * SHARE).round() as u32;
    let x = area.position.x + ((area.size.width - w) / 2) as i32;
    let y = area.position.y + ((area.size.height - h) / 2) as i32;
    // 先挪过去再改大小：两个显示器的缩放不一样时，挪过去之后系统会按新的缩放改一次大小
    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.set_size(PhysicalSize::new(w, h));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_popout_labels_are_accepted() {
        let label = |s: &str| label_of(&Url::parse(s).unwrap());
        assert_eq!(
            label("about:blank#popout-k2x9-3").as_deref(),
            Some("popout-k2x9-3")
        );
        assert_eq!(label("about:blank"), None);
        assert_eq!(label("about:blank#popout-"), None);
        assert_eq!(label("about:blank#main"), None);
        assert_eq!(label("about:blank#popout-A"), None);
        assert_eq!(label("about:blank#popout-a/b"), None);
        assert_eq!(label("https://example.com/#popout-a"), None);
        assert_eq!(
            label(&format!("about:blank#popout-{}", "a".repeat(64))),
            None
        );
    }
}
