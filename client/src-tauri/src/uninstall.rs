//! macOS：菜单里的"卸载 Airhand…"。
//!
//! macOS 的 App 没有卸载程序，拖进废纸篓只删掉了程序本身。这里按使用指南里"卸载"一节的步骤一次做完，
//! 顺序有讲究（都是在真机上试出来的）：
//! 1. 注销登录项：要由 App 自己做；先删 App 的话，系统设置的"登录时打开"和"后台 App 活动"里都会留下失效的条目；
//! 2. 清除权限记录（`tccutil reset All <bundle ID>`）：要在 App 还在的时候做，App 删掉之后系统就找不到这个 bundle ID 了；
//! 3. 删除配置文件所在的目录、WebKit / Metal 以 bundle ID 在系统临时 / 缓存目录里建的目录（受系统保护的跳过，
//!    见 [`is_protected`]）、Airhand 的崩溃报告、
//!    AppKit 记下的菜单栏图标位置 / 是否显示（App 自己的偏好设置，见 [`remove_preferences`]）；
//! 4. 把 App 移到废纸篓（放错了还能放回来，清空废纸篓就彻底删掉了）。
//!
//! 某一步没做成不影响后面的步骤，最后的提示里列出来，可以按使用指南手动清理。
//! 调用之前先停掉被控端和控制端、让配置文件不再写入（见 `lib.rs` 的 `uninstall`）。

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication, NSModalPanelWindowLevel};
use objc2_foundation::{NSFileManager, NSString, NSURL};
use tracing::{info, warn};

/// 要清理的东西
pub struct Plan {
    /// 程序所在的 App 包（移到废纸篓）
    pub bundle: Option<PathBuf>,
    /// 清除权限记录、找系统为它建的目录用（`app.airhand.client`）
    pub bundle_id: String,
    /// 可执行文件名：崩溃报告以它开头
    pub exe_name: String,
    /// 配置文件所在的目录（目录名必须就是 bundle ID，防止删错）
    pub config_dir: Option<PathBuf>,
}

/// 确认对话框。卸载 = `true`
pub fn confirm(mtm: MainThreadMarker) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(&tr!(
        "卸载 Airhand？",
        "Uninstall Airhand?"
    )));
    alert.setInformativeText(&NSString::from_str(&tr!(
        "会关掉登录时自动启动，清除 Airhand 的“屏幕与系统录音”“辅助功能”等权限，删除配置文件（本机的控制码、“我的设备”里保存的电脑）和系统为它生成的缓存、记下的菜单栏图标设置，然后把 Airhand 移到废纸篓并退出。\n\n正在控制本机的连接会断开。",
        "This turns off open at login, clears Airhand's permissions (Screen & System Audio Recording, Accessibility and so on), deletes the configuration file (this computer's control code and the computers saved in My devices) and the caches and menu bar icon settings macOS keeps for it, then moves Airhand to the Trash and quits.\n\nAnyone controlling this computer will be disconnected."
    )));
    // 第一个按钮是默认按钮（回车）：默认是"取消"，不会一按回车就卸载了
    alert.addButtonWithTitle(&NSString::from_str(&tr!("取消", "Cancel")));
    let uninstall = alert.addButtonWithTitle(&NSString::from_str(&tr!("卸载", "Uninstall")));
    uninstall.setHasDestructiveAction(true);
    show(mtm, &alert) == objc2_app_kit::NSAlertSecondButtonReturn
}

/// 卸载完了：说明程序在废纸篓里；没做成的步骤列出来
pub fn done(mtm: MainThreadMarker, problems: &[String]) {
    let alert = NSAlert::new(mtm);
    let text = if problems.is_empty() {
        alert.setMessageText(&NSString::from_str(&tr!(
            "Airhand 已卸载",
            "Airhand has been uninstalled"
        )));
        tr!(
            "Airhand 已经移到废纸篓，清空废纸篓就彻底删掉了。",
            "Airhand is in the Trash now; empty the Trash to delete it for good."
        )
    } else {
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(&tr!(
            "Airhand 没有完全卸载",
            "Airhand wasn't completely uninstalled"
        )));
        let list = problems
            .iter()
            .map(|p| format!("• {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        tr!(
            "下面几项没有做成，可以按使用指南里“卸载”一节手动清理：\n\n{list}",
            "These steps didn't work out; see \"Uninstalling\" in the usage guide to clean them up by hand:\n\n{list}"
        )
    };
    alert.setInformativeText(&NSString::from_str(&text));
    alert.addButtonWithTitle(&NSString::from_str(&tr!("好", "OK")));
    show(mtm, &alert);
}

fn show(mtm: MainThreadMarker, alert: &NSAlert) -> objc2_app_kit::NSModalResponse {
    // 平时只有菜单栏图标（没有窗口、不在程序坞里）：先把自己带到前面。macOS 14 起"激活"是协商式的，
    // 系统可能不让（例如用户刚才没有点过本程序），所以再把对话框放到模态面板那一层，
    // 不会躲在别的程序的窗口后面（实测：只靠激活的话，对话框可能被挡在后面，看起来像没反应）
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    alert.window().setLevel(NSModalPanelWindowLevel);
    alert.runModal()
}

/// 按顺序做完所有步骤，返回没做成的（给用户看的说明）
pub fn run(plan: &Plan) -> Vec<String> {
    let mut problems = Vec::new();
    let mut step = |what: String, r: Result<(), String>| {
        if let Err(e) = r {
            warn!("uninstall: {what}: {e}");
            problems.push(what);
        }
    };
    // 1、2 都要在 App 还在的时候做
    step(
        tr!("关掉登录时自动启动", "Turn off open at login"),
        crate::login_item::unregister(),
    );
    step(
        tr!(
            "清除权限记录（系统设置 → 隐私与安全性）",
            "Clear the permissions (System Settings → Privacy & Security)"
        ),
        reset_permissions(&plan.bundle_id),
    );
    if let Some(dir) = &plan.config_dir {
        let shown = dir.display();
        step(
            tr!(
                "删除配置文件（{shown}）",
                "Delete the configuration ({shown})"
            ),
            remove_config(dir, &plan.bundle_id),
        );
    }
    step(
        tr!(
            "删除系统为它生成的缓存",
            "Delete the caches macOS created for it"
        ),
        remove_caches(&plan.bundle_id),
    );
    step(
        tr!("删除崩溃报告", "Delete the crash reports"),
        remove_crash_reports(&plan.exe_name),
    );
    // 菜单栏图标已经拿掉了（见 lib.rs 的 uninstall_app）：移除时 AppKit 对偏好设置的改写在这之前早就写完了
    step(
        tr!(
            "删除菜单栏图标的设置（~/Library/Preferences/{id}.plist）",
            "Delete the menu bar icon settings (~/Library/Preferences/{id}.plist)",
            id = plan.bundle_id
        ),
        remove_preferences(&plan.bundle_id),
    );
    if let Some(bundle) = &plan.bundle {
        let shown = bundle.display();
        step(
            tr!(
                "把 Airhand 移到废纸篓（{shown}）",
                "Move Airhand to the Trash ({shown})"
            ),
            trash(bundle),
        );
    }
    info!(problems = problems.len(), "uninstalled");
    problems
}

/// 清除这个 App 的所有权限记录（屏幕与系统录音、辅助功能……），不影响别的 App，不需要管理员权限
fn reset_permissions(bundle_id: &str) -> Result<(), String> {
    let out = std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", "All", bundle_id])
        .output()
        .map_err(|e| format!("can't run tccutil: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "tccutil: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn remove_config(dir: &Path, bundle_id: &str) -> Result<(), String> {
    if dir.file_name().and_then(|n| n.to_str()) != Some(bundle_id) {
        return Err(format!(
            "unexpected configuration directory {}",
            dir.display()
        ));
    }
    remove(dir)
}

/// WebKit、Metal 以 bundle ID 在系统的临时 / 缓存目录（/var/folders/…/T、C）里建的目录。
/// 受系统保护的删不掉，跳过，不算没做成（见 [`is_protected`]）
fn remove_caches(bundle_id: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for dir in [
        user_dir(libc::_CS_DARWIN_USER_CACHE_DIR),
        user_dir(libc::_CS_DARWIN_USER_TEMP_DIR),
    ]
    .into_iter()
    .flatten()
    {
        // 只列出这一层的名字，不进别的 App 受保护的目录
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| is_cache_of(n, bundle_id))
                && !is_protected(&path)
                && let Err(e) = remove(&path)
            {
                errors.push(e);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// 受系统完整性保护（SIP）、谁都删不掉的：WebKit 在临时目录里建的 `com.apple.WebKit.*+<bundle ID>`
/// 带 `sunlnk`（实测 macOS 27，用到 WebKit 的 App 都一样，包括 Safari；里面是空的）。
/// 删除只会得到 Operation not permitted，用户手动也删不掉，所以不算没做成（使用指南的"卸载"里有说明）
fn is_protected(path: &Path) -> bool {
    use std::os::macos::fs::MetadataExt;
    // <sys/stat.h>：要有权利（entitlement）才能写 / 不能删除、改名
    const SF_RESTRICTED: u32 = 0x0008_0000;
    const SF_NOUNLINK: u32 = 0x0010_0000;
    std::fs::symlink_metadata(path).is_ok_and(|m| m.st_flags() & (SF_RESTRICTED | SF_NOUNLINK) != 0)
}

/// `app.airhand.client`（Metal）、`com.apple.WebKit.GPU+app.airhand.client`（WebKit 的各个进程）
fn is_cache_of(name: &str, bundle_id: &str) -> bool {
    name == bundle_id
        || name
            .strip_suffix(bundle_id)
            .is_some_and(|rest| rest.ends_with('+'))
}

/// macOS 记下的崩溃报告：`~/Library/Logs/DiagnosticReports/airhand-<时间>.ips`、
/// `~/Library/Application Support/CrashReporter/airhand_<UUID>.plist`
fn remove_crash_reports(exe_name: &str) -> Result<(), String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let mut errors = Vec::new();
    for dir in [
        home.join("Library/Logs/DiagnosticReports"),
        home.join("Library/Application Support/CrashReporter"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| is_crash_report_of(n, exe_name))
                && let Err(e) = remove(&entry.path())
            {
                errors.push(e);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// App 自己的偏好设置（`~/Library/Preferences/<bundle ID>.plist`）。Airhand 不用它，平时也没有这个文件；
/// 用户按住 ⌘ 拖动过菜单栏图标（`NSStatusItem Preferred Position Item-0`）、或者在“系统设置 → 菜单栏”里
/// 关掉过它（macOS 26 起，`NSStatusItem VisibleCC Item-0`）时，AppKit 把这些记在这里。不删的话，重新安装后
/// 菜单栏图标还是藏着的，而菜单栏图标是 Airhand 唯一的界面。
/// 先经过 cfprefsd 删（`defaults delete`，不靠 cfprefsd 自己发现文件没了、清掉缓存），它会留下一个空文件，再把文件删掉。
/// 调用之前菜单栏图标要已经拿掉：AppKit 移除图标时会改写偏好设置，删完之后再移除会重新生成一个空文件
fn remove_preferences(bundle_id: &str) -> Result<(), String> {
    let out = std::process::Command::new("/usr/bin/defaults")
        .args(["delete", bundle_id])
        .output()
        .map_err(|e| format!("can't run defaults: {e}"))?;
    // 平时没有这个域：defaults 报 “Domain … not found”，不算错
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() && !stderr.contains("not found") {
        return Err(format!("defaults: {} {}", out.status, stderr.trim()));
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    remove(&home.join(format!("Library/Preferences/{bundle_id}.plist")))
}

fn is_crash_report_of(name: &str, exe_name: &str) -> bool {
    let Some(rest) = name.strip_prefix(exe_name) else {
        return false;
    };
    (rest.starts_with('-') && rest.ends_with(".ips"))
        || (rest.starts_with('_') && rest.ends_with(".plist"))
}

/// 移到废纸篓（和在访达里拖进去一样，还能放回来）
fn trash(path: &Path) -> Result<(), String> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|e| {
            format!(
                "can't move {} to the Trash: {} {} ({})",
                path.display(),
                e.domain(),
                e.code(),
                e.localizedDescription()
            )
        })
}

fn remove(path: &Path) -> Result<(), String> {
    let r = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// 本用户的系统临时 / 缓存目录（`getconf DARWIN_USER_TEMP_DIR` / `DARWIN_USER_CACHE_DIR`）
fn user_dir(name: libc::c_int) -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    let n = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    Some(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_its_own_caches_only() {
        let id = "app.airhand.client";
        for ours in [
            "app.airhand.client",
            "com.apple.WebKit.GPU+app.airhand.client",
            "com.apple.WebKit.Networking+app.airhand.client",
            "com.apple.WebKit.WebContent+app.airhand.client",
        ] {
            assert!(is_cache_of(ours, id), "{ours}");
        }
        for theirs in [
            "com.apple.WebKit.GPU+com.example.app",
            "com.apple.WebKit.GPU.Sandbox",
            "myapp.airhand.client",
            "app.airhand.client.other",
            "app.airhand.android",
            "com.apple.metal",
        ] {
            assert!(!is_cache_of(theirs, id), "{theirs}");
        }
    }

    #[test]
    fn finds_its_own_crash_reports_only() {
        for ours in [
            "airhand-2026-11-08-231352.ips",
            "airhand_CB7C7C2B-7CB9-5D42-99A3-7CE19346F1B6.plist",
        ] {
            assert!(is_crash_report_of(ours, "airhand"), "{ours}");
        }
        for theirs in [
            "airhandx-2026-11-08-231352.ips",
            "airhand-2026-11-08.txt",
            "Safari-2026-11-08-231352.ips",
            "airhand_x.ips",
            "airhand.ips",
        ] {
            assert!(!is_crash_report_of(theirs, "airhand"), "{theirs}");
        }
    }

    /// 配置目录的名字不是 bundle ID 时不删（防止路径出了岔子删错东西）
    #[test]
    fn refuses_an_unexpected_config_dir() {
        let dir =
            std::env::temp_dir().join(format!("airhand-uninstall-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(remove_config(&dir, "app.airhand.client").is_err());
        assert!(dir.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 偏好设置：有就删干净（`defaults delete` 留下的空文件也删掉），没有（平时就是这样）不算错
    #[test]
    fn removes_preferences() {
        let id = format!("app.airhand.uninstall-test-{}", std::process::id());
        let file = PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(format!("Library/Preferences/{id}.plist"));
        let written = std::process::Command::new("/usr/bin/defaults")
            .args([
                "write",
                &id,
                "NSStatusItem VisibleCC Item-0",
                "-bool",
                "false",
            ])
            .status()
            .unwrap()
            .success();
        assert!(written && file.exists());
        remove_preferences(&id).unwrap();
        assert!(!file.exists());
        remove_preferences(&id).unwrap();
        assert!(!file.exists());
    }

    /// 普通的目录、不存在的路径都不算受保护（受保护的要 root 才能设置，测试里造不出来）
    #[test]
    fn ordinary_dirs_are_not_protected() {
        let dir =
            std::env::temp_dir().join(format!("airhand-protected-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_protected(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!is_protected(&dir));
    }

    #[test]
    fn system_dirs() {
        let temp = user_dir(libc::_CS_DARWIN_USER_TEMP_DIR).unwrap();
        let cache = user_dir(libc::_CS_DARWIN_USER_CACHE_DIR).unwrap();
        assert!(temp.is_dir() && cache.is_dir(), "{temp:?} {cache:?}");
        assert_ne!(temp, cache);
    }
}
