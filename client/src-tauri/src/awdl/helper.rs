//! 和 root 助手（`helper.c`）打交道：装上、拿掉、连上去"拿着"它。
//!
//! 装在哪里（都属于 root，普通用户改不了）：
//! * `/Library/PrivilegedHelperTools/app.airhand.awdl`：助手本身，编译进了 Airhand（[`BINARY`]）；
//! * `/Library/LaunchDaemons/app.airhand.awdl.plist`：launchd 的配置。launchd 开机时替它监听 [`SOCKET`]，
//!   有连接来了才以 root 身份启动它（没人用的时候不运行）。
//!
//! 安装、移除各要一次管理员密码：用 `osascript` 的 `do shell script … with administrator privileges`，
//! 由系统弹出输入密码（或者触控 ID）的对话框。脚本整个作为参数交给它，助手的内容以 base64 写在脚本里，
//! 中间不经过普通用户写得了的临时文件（不然别的程序可以在 root 复制之前把它换掉）。
//!
//! Airhand 是 ad-hoc 签名的，用不了 `SMAppService.daemon`（系统只给有开发者签名的 App 注册守护进程），
//! 所以按传统的做法装进 /Library/LaunchDaemons；macOS 13 起"系统设置 → 通用 → 登录项与扩展"里的
//! "允许在后台"会列出它，在那里关掉的话它就不运行了。

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use base64::Engine;

/// launchd 里的名字
pub const LABEL: &str = "app.airhand.awdl";
/// 助手所在的目录（很多 Mac 上本来没有，安装时建、卸载时空了就删）
const TOOLS: &str = "/Library/PrivilegedHelperTools";
pub const PROGRAM: &str = "/Library/PrivilegedHelperTools/app.airhand.awdl";
pub const PLIST: &str = "/Library/LaunchDaemons/app.airhand.awdl.plist";
/// launchd 替助手监听的地方（谁都能连：能做的只有"远程控制期间暂停 AWDL"这一件事）
pub const SOCKET: &str = "/var/run/app.airhand.awdl.sock";
/// 助手的版本，和 `helper.c` 的 `GREETING` 一致（测试里检查）
pub const VERSION: u32 = 1;
/// 助手的开场白：`airhand-awdl <版本>\n`
const GREETING: &str = "airhand-awdl ";

/// 编译好的助手（见 build.rs 的 `awdl_helper`）
pub static BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/airhand-awdl"));

/// 等助手回话最多这么久（launchd 要先启动它）
const ANSWER: Duration = Duration::from_secs(3);

/// 装上了没有
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    No,
    Yes,
    /// 装着的是旧版本，要更新（再输一次管理员密码）
    Outdated,
}

pub fn installed() -> Installed {
    if !Path::new(PLIST).exists() {
        return Installed::No;
    }
    match std::fs::read(PROGRAM) {
        Ok(bytes) if bytes == BINARY => Installed::Yes,
        // 别的构建（另一台机器编译的同一个版本）、更新的 Airhand 装的：看版本
        Ok(bytes) => match version_in(&bytes) {
            Some(v) if v >= VERSION => Installed::Yes,
            _ => Installed::Outdated,
        },
        Err(_) => Installed::No,
    }
}

/// 助手里的开场白写的版本
fn version_in(binary: &[u8]) -> Option<u32> {
    let at = binary
        .windows(GREETING.len())
        .position(|w| w == GREETING.as_bytes())?;
    let digits: String = binary[at + GREETING.len()..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .map(|&b| b as char)
        .collect();
    digits.parse().ok()
}

/// 拿着助手：连接开着期间 awdl0 一直关着，drop 就放开
pub struct Lease {
    stream: UnixStream,
    /// 助手说没关上（errno），之后它照样一直在关
    pub error: Option<io::Error>,
}

impl Lease {
    /// 助手还在（没有退出、没有被拿掉）
    pub fn alive(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut b = [0u8; 1];
        // 只看一眼、不等：助手退出了是 0（对方关闭），还在是 EAGAIN（它什么也不会再说）
        let n = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                b.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        match n {
            0 => false,
            n if n > 0 => true,
            _ => matches!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        }
    }
}

/// 连上 `socket` 上的助手，让它拿着
pub fn hold(socket: &Path) -> io::Result<Lease> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(ANSWER))?;
    stream.set_write_timeout(Some(ANSWER))?;
    // 哪个版本都会 "hold"（旧了的由菜单提示更新，见 [`installed`]）：只确认对面是它
    let hello = read_line(&mut stream)?;
    if hello
        .strip_prefix(GREETING)
        .and_then(|v| v.parse::<u32>().ok())
        .is_none()
    {
        return Err(io::Error::other(format!("unexpected greeting {hello:?}")));
    }
    stream.write_all(b"hold\n")?;
    let answer = read_line(&mut stream)?;
    let error = match answer.as_str() {
        "ok" => None,
        a => match a.strip_prefix("error ").and_then(|e| e.parse::<i32>().ok()) {
            Some(errno) => Some(io::Error::from_raw_os_error(errno)),
            None => return Err(io::Error::other(format!("unexpected answer {a:?}"))),
        },
    };
    stream.set_read_timeout(None)?;
    Ok(Lease { stream, error })
}

/// 读一行（不含换行），最多 64 个字节
fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match stream.read(&mut b)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            _ if b[0] == b'\n' => break,
            _ if line.len() >= 64 => return Err(io::Error::other("line too long")),
            _ => line.push(b[0]),
        }
    }
    String::from_utf8(line).map_err(io::Error::other)
}

/// 管理员权限的操作没做成
#[derive(Debug)]
pub enum AdminError {
    /// 用户在输入密码的对话框里点了取消
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdminError::Cancelled => f.write_str("cancelled"),
            AdminError::Failed(e) => f.write_str(e),
        }
    }
}

/// 装上（或者更新）助手并让 launchd 加载它。`prompt`：输入密码的对话框里说明要做什么
pub fn install(prompt: &str) -> Result<(), AdminError> {
    run_as_admin(&install_script(BINARY), prompt)
}

/// 拿掉助手：launchd 先让它退出（拿着的话它把 AWDL 恢复了再退出），再删掉文件
pub fn uninstall(prompt: &str) -> Result<(), AdminError> {
    run_as_admin(&uninstall_script(), prompt)
}

fn plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{PROGRAM}</string>
	</array>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>{bundle}</string>
	</array>
	<key>Sockets</key>
	<dict>
		<key>Listeners</key>
		<dict>
			<key>SockPathName</key>
			<string>{SOCKET}</string>
			<key>SockPathMode</key>
			<integer>438</integer>
		</dict>
	</dict>
</dict>
</plist>
"#,
        bundle = crate::config::BUNDLE_ID,
    )
}

/// 以 root 运行的安装脚本。heredoc 的结束标记不会出现在 base64 和 plist 里。
///
/// 只动卸载时能恢复原样的东西：`/Library/PrivilegedHelperTools` 已经有了就不改它的权限；
/// 不无条件地 `launchctl enable`（它会在 launchd 的启用 / 禁用记录里写下一项，没有命令能删掉），
/// 只有被 `launchctl disable` 关掉过（那时记录本来就有了）才打开
fn install_script(binary: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    let lines = lines.join("\n");
    let plist = plist();
    format!(
        r#"set -e
umask 022
if [ ! -d {TOOLS} ]; then
  /bin/mkdir -m 755 {TOOLS}
  /usr/sbin/chown root:wheel {TOOLS}
fi
new="{TOOLS}/.{LABEL}.$$"
trap '/bin/rm -f "$new"' EXIT
/usr/bin/base64 -D -o "$new" <<'AIRHAND_EOF'
{lines}
AIRHAND_EOF
/usr/sbin/chown root:wheel "$new"
/bin/chmod 755 "$new"
/bin/mv -f "$new" {PROGRAM}
/bin/cat > {PLIST} <<'AIRHAND_EOF'
{plist}AIRHAND_EOF
/usr/sbin/chown root:wheel {PLIST}
/bin/chmod 644 {PLIST}
/bin/launchctl bootout system/{LABEL} 2>/dev/null || true
if /bin/launchctl print-disabled system | /usr/bin/grep -q '"{LABEL}" => disabled'; then
  /bin/launchctl enable system/{LABEL}
fi
i=0
while ! /bin/launchctl bootstrap system {PLIST}; do
  i=$((i + 1))
  if [ $i -ge 10 ]; then exit 1; fi
  /bin/sleep 0.5
done
"#
    )
}

/// 把安装时动过的都恢复原样：launchd 里的任务（拿着的话助手先把 AWDL 恢复了再退出）、两个文件、套接字、
/// 装到一半留下的临时文件；`/Library/PrivilegedHelperTools` 本来没有、是安装时建的话也删掉（只删空的：
/// 里面还有别的程序的助手就留着）。安装时不写 launchd 的启用 / 禁用记录（见 [`install_script`]），也就不用删
fn uninstall_script() -> String {
    format!(
        "/bin/launchctl bootout system/{LABEL} 2>/dev/null || true\n\
         /bin/rm -f {PLIST} {PROGRAM} {SOCKET} {TOOLS}/.{LABEL}.*\n\
         /bin/rmdir {TOOLS} 2>/dev/null || true\n"
    )
}

/// 以管理员权限运行 `script`（`/bin/sh`）。阻塞到用户输完密码、脚本跑完
fn run_as_admin(script: &str, prompt: &str) -> Result<(), AdminError> {
    let out = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "do shell script (item 1 of argv) with prompt (item 2 of argv) with administrator privileges",
            "-e",
            "end run",
            script,
            prompt,
        ])
        .output()
        .map_err(|e| AdminError::Failed(format!("can't run osascript: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    // execution error: User canceled. (-128)
    if stderr.contains("(-128)") {
        Err(AdminError::Cancelled)
    } else {
        Err(AdminError::Failed(stderr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Stdio};

    /// 编译进来的助手：开场白里的版本和 [`VERSION`] 一致
    #[test]
    fn embedded_helper_has_this_version() {
        assert!(BINARY.len() > 1000 && BINARY.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]));
        assert_eq!(version_in(BINARY), Some(VERSION));
        assert_eq!(version_in(b"xx airhand-awdl 12\n"), Some(12));
        assert_eq!(version_in(b"airhand-awdl x"), None);
    }

    /// 脚本：解出来的就是助手本身，plist 是合法的
    #[test]
    fn install_script_carries_the_helper() {
        let script = install_script(b"\x00binary\xffcontent");
        let b64 = script
            .split("<<'AIRHAND_EOF'\n")
            .nth(1)
            .unwrap()
            .split("\nAIRHAND_EOF")
            .next()
            .unwrap()
            .replace('\n', "");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
            b"\x00binary\xffcontent"
        );
        assert!(
            script.contains(
                "launchctl bootstrap system /Library/LaunchDaemons/app.airhand.awdl.plist"
            )
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.plist");
        std::fs::write(&file, plist()).unwrap();
        let ok = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&file)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    /// 两个脚本都是合法的 sh；系统的 base64 把脚本里的那一段解回来就是编译进来的助手
    #[test]
    fn scripts_are_valid_and_decode_with_the_system_tools() {
        let install = install_script(BINARY);
        for s in [&install, &uninstall_script()] {
            let ok = Command::new("/bin/sh")
                .args(["-n", "-c", s])
                .status()
                .unwrap()
                .success();
            assert!(ok, "{s:.200}");
        }
        let lines = install
            .split("<<'AIRHAND_EOF'\n")
            .nth(1)
            .unwrap()
            .split("\nAIRHAND_EOF")
            .next()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("helper");
        let ok = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "/usr/bin/base64 -D -o \"$0\" <<'AIRHAND_EOF'\n{lines}\nAIRHAND_EOF\n"
            ))
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert!(std::fs::read(&out).unwrap() == BINARY);
    }

    /// 在一个临时目录里跑安装、卸载脚本（绝对路径换到里面，launchctl、chown 换成只记下参数的替身）：
    /// 卸载之后文件层面和安装之前一模一样；不写 launchd 的启用记录（不调 `launchctl enable`）
    #[test]
    fn uninstall_leaves_nothing_behind() {
        use std::os::unix::fs::PermissionsExt;
        fn tree(root: &Path) -> Vec<String> {
            let mut out = Vec::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for e in std::fs::read_dir(&dir).unwrap().flatten() {
                    let p = e.path();
                    out.push(p.strip_prefix(root).unwrap().display().to_string());
                    if p.is_dir() {
                        stack.push(p);
                    }
                }
            }
            out.sort();
            out
        }
        // 两种情况：/Library/PrivilegedHelperTools 本来没有；本来就有、里面还有别的程序的助手
        for others in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            std::fs::create_dir_all(root.join("Library/LaunchDaemons")).unwrap();
            std::fs::create_dir_all(root.join("var/run")).unwrap();
            if others {
                std::fs::create_dir_all(root.join("Library/PrivilegedHelperTools")).unwrap();
                std::fs::write(
                    root.join("Library/PrivilegedHelperTools/com.example.helper"),
                    b"x",
                )
                .unwrap();
            }
            let log = dir.path().join("launchctl.log");
            let stub = dir.path().join("launchctl");
            std::fs::write(
                &stub,
                format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display()),
            )
            .unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            let sandbox = |script: String| {
                let r = root.display().to_string();
                script
                    .replace("/bin/launchctl", &stub.display().to_string())
                    .replace("/usr/sbin/chown", "/usr/bin/true")
                    .replace("/Library/", &format!("{r}/Library/"))
                    .replace("/var/run/", &format!("{r}/var/run/"))
            };
            let run = |script: String| {
                let ok = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(sandbox(script))
                    .status()
                    .unwrap()
                    .success();
                assert!(ok);
            };
            let before = tree(&root);
            run(install_script(b"helper"));
            assert_eq!(
                std::fs::read(root.join("Library/PrivilegedHelperTools/app.airhand.awdl")).unwrap(),
                b"helper"
            );
            assert!(
                root.join("Library/LaunchDaemons/app.airhand.awdl.plist")
                    .is_file()
            );
            // launchd 里开着的套接字（替身不会建，模拟一下）、装到一半留下的临时文件
            std::fs::write(root.join("var/run/app.airhand.awdl.sock"), b"").unwrap();
            std::fs::write(
                root.join("Library/PrivilegedHelperTools/.app.airhand.awdl.123"),
                b"",
            )
            .unwrap();
            run(uninstall_script());
            assert_eq!(tree(&root), before, "others = {others}");
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(!calls.contains("enable"), "{calls}");
            assert!(
                calls.contains("bootstrap system")
                    && calls.contains("bootout system/app.airhand.awdl")
            );
        }
    }

    struct Helper {
        child: Child,
        _dir: tempfile::TempDir,
        socket: std::path::PathBuf,
    }

    impl Helper {
        /// 像 launchd 那样让它退出（SIGTERM：拿着的话先恢复 AWDL）
        fn stop(&mut self) {
            if let Ok(None) = self.child.try_wait() {
                unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
                let _ = self.child.wait();
            }
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// 以普通用户运行编译进来的助手（`--socket`，不经过 launchd）
    fn spawn_helper() -> Helper {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("helper");
        std::fs::write(&program, BINARY).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = dir.path().join("s.sock");
        let child = Command::new(&program)
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Helper {
            child,
            _dir: dir,
            socket,
        }
    }

    /// 协议：开场白、拿着、回答；不是 root 关不上 awdl0（root 跑测试的话能关上，会马上恢复）
    #[test]
    fn talks_to_the_helper() {
        let h = spawn_helper();
        let lease = hold(&h.socket).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            let e = lease
                .error
                .as_ref()
                .expect("can't take awdl0 down without root");
            assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        }
        assert!(lease.alive());
        // 同时拿着的另一个会话
        let second = hold(&h.socket).unwrap();
        assert!(second.alive());
        drop(lease);
        drop(second);
        // 说别的就被断开
        let mut s = UnixStream::connect(&h.socket).unwrap();
        s.set_read_timeout(Some(ANSWER)).unwrap();
        assert!(read_line(&mut s).unwrap().starts_with(GREETING));
        s.write_all(b"down please\n").unwrap();
        assert_eq!(
            read_line(&mut s).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    /// 助手退出了（被拿掉、崩溃）：拿着的那一方看得出来
    #[test]
    fn notices_the_helper_going_away() {
        let mut h = spawn_helper();
        let lease = hold(&h.socket).unwrap();
        assert!(lease.alive());
        h.stop();
        assert!(!lease.alive());
        assert!(hold(&h.socket).is_err());
    }
}
