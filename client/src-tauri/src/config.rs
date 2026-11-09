//! 配置。
//!
//! * 中转服务器地址和证书指纹的**默认值编译进程序**：构建时用环境变量覆盖，或者修改下面的默认值：
//!
//!   ```powershell
//!   $env:AIRHAND_SERVER="relay.example.com:4433"
//!   $env:AIRHAND_SERVER_FP="<服务器日志中打印的证书 SHA-256 指纹>"
//!   npm run tauri build
//!   ```
//!
//! * **配置文件 `config.toml`**（见 [`Settings`]）：可以手动改服务器地址和指纹；还保存着服务器分配的控制码
//!   （固定不变，见 `airhand_proto::signal` 的"固定控制码"）、控制台的"我的设备"（见 [`crate::devices`]），
//!   Windows 上还有开机自动启动（macOS 上是登录时自动启动的记录，见 [`autostart_default_pending`]）。
//!   所有数据都只在这一个文件里。位置（见 [`path`]）：
//!   * Windows：程序所在目录（安装目录）。文件由系统服务里的被控端维护（见 [`crate::winsvc`]），
//!     托盘进程没有写权限，控制台保存设备时经命名管道交给它写；
//!   * macOS：`~/Library/Application Support/app.airhand.client/`；Linux：`~/.config/airhand/`。只有本账户能读写（600）。
//! * 文件由 Airhand 维护，手动修改的内容和注释都会保留；改了设置要重新启动 Airhand 才生效。
//! * 文件不存在时（第一次运行、被删掉了）按默认配置重新生成：启动时检查一次，运行中每隔几秒检查一次（[`keep_file`]），
//!   控制码、开机自动启动、我的设备、记下的服务器指纹从删掉之前的内容接着用，不会丢。
//! * 没有设置服务器证书指纹时，第一次连上服务器时记下它的指纹（`[server_trust]`，见 [`FileTrust`]），
//!   之后证书变了就拒绝连接。
//! * 端到端加密（见 `airhand_proto::handshake`）：本机作为被控端的身份密钥和配对码（`[host]`）、配对过的控制端
//!   （`[[paired]]`）、控制台的身份密钥（`[controller]`，Windows 上每个账户一把），也都在这个文件里，
//!   密钥和配对码加密保存（见 [`secret`]）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use airhand_link::LinkConfig;
use airhand_link::net::{MemoryTrust, Trust, TrustStore, format_fingerprint, parse_fingerprint};
use airhand_link::secure::{Access, Identity};
use airhand_proto::signal::Reclaim;
use anyhow::Result;
use toml_edit::{DocumentMut, Item};
use tracing::{error, info, warn};

/// 编译进程序的中转服务器地址 `host:port`（UDP），`config.toml` 里没有设置时使用。
pub const DEFAULT_SERVER: &str = match option_env!("AIRHAND_SERVER") {
    Some(v) => v,
    None => "127.0.0.1:4433",
};

/// 编译进程序的服务器证书 SHA-256 指纹（64 位十六进制，允许用 `:` 或空白分隔），`config.toml` 里没有设置时使用。
///
/// * 非空：严格校验，指纹不符直接拒绝连接（防中间人）。
/// * 为空：第一次连上服务器时记下它的指纹（`[server_trust]`），之后只认它。
pub const DEFAULT_SERVER_FP: &str = match option_env!("AIRHAND_SERVER_FP") {
    Some(v) => v,
    None => "",
};

// 指纹写错了在编译时就报错，而不是等到运行时连不上服务器（规则与 airhand_link::net::parse_fingerprint 相同）
const _: () = assert!(
    fingerprint_is_valid(DEFAULT_SERVER_FP),
    "AIRHAND_SERVER_FP must be the 64 hex digit fingerprint printed in the server log (`:` and spaces are allowed)"
);

const fn fingerprint_is_valid(s: &str) -> bool {
    let b = s.as_bytes();
    let (mut i, mut digits) = (0, 0);
    while i < b.len() {
        if b[i].is_ascii_hexdigit() {
            digits += 1;
        } else if !(b[i] == b':' || b[i].is_ascii_whitespace()) {
            return false;
        }
        i += 1;
    }
    digits == 0 || digits == 64
}

/// 产品版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 配置文件名
#[cfg_attr(test, allow(dead_code))]
pub const FILE_NAME: &str = "config.toml";

/// 与 tauri.conf.json 的 `identifier` 相同：macOS 上配置目录以它命名（Apple 的建议）
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const BUNDLE_ID: &str = "app.airhand.client";

/// 卸载中（见 [`freeze`]）：不再写配置文件，删掉了也不再重新生成
static FROZEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 运行中多久检查一次配置文件还在不在
const KEEP_CHECK: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(3)
};

/// 进程启动时读到的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// 中转服务器地址（`server`）
    pub server: String,
    /// 中转服务器证书指纹（`server_fp`），空字符串表示第一次连接时记下（见 [`FileTrust`]）
    pub server_fp: String,
    /// 开机自动启动（`autostart`，Windows 安装版）。macOS 上以系统设置为准，这一项只是记录（见 [`autostart_default_pending`]）
    pub autostart: bool,
    /// 没配对过的控制端怎么办（`access`）：要配对码（默认），或者知道控制码就能连
    pub access: Access,
    /// 服务器分配的控制码和加密过的密钥（`[code]`），用 [`Settings::code`] 解开
    saved_code: Option<(i64, String)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: DEFAULT_SERVER.to_string(),
            server_fp: DEFAULT_SERVER_FP.to_string(),
            autostart: true,
            access: Access::Pairing,
            saved_code: None,
        }
    }
}

impl Settings {
    /// 保存的控制码和取回它用的密钥。密钥只有保存它的账户（安装版是服务里的被控端，SYSTEM）解得开，
    /// 解不开（例如配置文件是从别的电脑拷来的）时返回 `None`：让服务器重新分配一个
    pub fn code(&self) -> Option<Reclaim> {
        let (code, secret) = self.saved_code.as_ref()?;
        match (u32::try_from(*code), secret::open(secret)) {
            (Ok(code), Ok(secret)) => Some(Reclaim { code, secret }),
            (_, Err(e)) => {
                warn!("can't use the saved control code ({e:#}), getting a new one");
                None
            }
            _ => {
                warn!("invalid saved control code {code}, getting a new one");
                None
            }
        }
    }
}

/// 配置文件的路径：
/// * Windows：程序所在目录（安装目录）下的 `config.toml`，被控端（系统服务）和所有账户共用；
/// * macOS：`~/Library/Application Support/app.airhand.client/config.toml`（Apple 的建议：
///   Application Support 下以 bundle ID 命名的目录）；
/// * Linux：`$XDG_CONFIG_HOME/airhand/config.toml`，默认 `~/.config/airhand/config.toml`（XDG 基础目录规范）。
///
/// 找不到用户目录时（极少见）返回 `None`：这次运行不保存任何东西
pub fn path() -> Option<PathBuf> {
    #[cfg(all(windows, not(test)))]
    {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join(FILE_NAME))
    }
    #[cfg(all(not(windows), not(test)))]
    {
        let base = dirs::config_dir()?;
        #[cfg(target_os = "macos")]
        let dir = base.join(BUNDLE_ID);
        #[cfg(not(target_os = "macos"))]
        let dir = base.join("airhand");
        Some(dir.join(FILE_NAME))
    }
    // 测试默认不读写配置文件（每次都是新的控制码）；测试配置文件本身时在那个测试的线程里指定一个临时路径
    #[cfg(test)]
    TEST_PATH.with(|p| p.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}
/// 测试结束时让 [`keep_file`] 的线程退出
#[cfg(test)]
static STOP_KEEPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 配置文件现在的内容（不存在、读不了时为 `None`）
pub fn read_text() -> Option<String> {
    std::fs::read_to_string(path()?).ok()
}

/// 控制码会不会保存下来（有配置文件）：保存的话，注册时请服务器一直为它保留（`Register::keep`）
pub fn persistent() -> bool {
    path().is_some()
}

/// 进程启动时读到的配置（之后的修改，例如更换控制码，由各自的模块记在内存里）。
pub fn settings() -> &'static Settings {
    static SETTINGS: LazyLock<Settings> = LazyLock::new(|| {
        let Some(path) = path() else {
            return Settings::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text).unwrap_or_else(|e| {
                error!("{}: {e:#}; using the defaults", path.display());
                Settings::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => {
                error!("can't read {}: {e}; using the defaults", path.display());
                Settings::default()
            }
        }
    });
    &SETTINGS
}

fn parse(text: &str) -> Result<Settings> {
    let doc: DocumentMut = text.parse()?;
    let mut s = Settings::default();
    if let Some(v) = doc.get("server").and_then(|v| v.as_str())
        && !v.trim().is_empty()
    {
        s.server = v.trim().to_string();
    }
    if let Some(v) = doc.get("server_fp").and_then(|v| v.as_str()) {
        s.server_fp = v.to_string();
    }
    if let Some(v) = doc.get("autostart").and_then(|v| v.as_bool()) {
        s.autostart = v;
    }
    // 只有写明了 "open" 才不要配对码：写错了的按更安全的来
    if let Some(v) = doc.get("access").and_then(|v| v.as_str()) {
        s.access = match v.trim() {
            "open" => Access::Open,
            "pairing" => Access::Pairing,
            other => {
                warn!("config.toml: unknown access {other:?}, using \"pairing\"");
                Access::Pairing
            }
        };
    }
    if let Some(t) = doc.get("code").and_then(|v| v.as_table_like()) {
        let code = t.get("code").and_then(|v| v.as_integer());
        let secret = t.get("secret").and_then(|v| v.as_str());
        if let (Some(code), Some(secret)) = (code, secret) {
            s.saved_code = Some((code, secret.to_string()));
        }
    }
    Ok(s)
}

/// 连接中转服务器用的配置：服务器地址与指纹，加上本机名称（被控端和控制端共用）
pub fn link() -> Arc<LinkConfig> {
    static LINK: LazyLock<Arc<LinkConfig>> = LazyLock::new(|| {
        let s = settings();
        let (name, os) = crate::sysinfo::host_identity();
        Arc::new(
            LinkConfig::new(s.server.clone(), &s.server_fp, name.clone(), os.clone())
                .map(|l| l.with_store(Arc::new(FileTrust::default())))
                .unwrap_or_else(|e| {
                    // 指纹写错了：不能因此就不校验证书。用一个不可能对得上的指纹，连接时报"证书指纹不匹配"
                    error!("config.toml: invalid server_fp ({e:#}), refusing to connect");
                    LinkConfig {
                        server: s.server.clone(),
                        trust: Trust::Pinned([0; 32]),
                        name,
                        os,
                    }
                }),
        )
    });
    LINK.clone()
}

/// 是否配置了固定的服务器证书指纹（否则第一次连接时记下，控制台据此说明）
pub fn pinned() -> bool {
    link().pinned()
}

/// 第一次连上服务器时记下的证书指纹：存在 config.toml 的 `[server_trust]` 里（服务器地址 + 指纹，
/// 换了服务器就重新记）。每次连接前重新读文件：Windows 上配置文件由系统服务里的被控端维护，
/// 控制台（用户进程）写不进去，它自己记下的只在内存里，之后用被控端记下的那一个
#[derive(Default)]
struct FileTrust(MemoryTrust);

impl TrustStore for FileTrust {
    fn load(&self, server: &str) -> Option<[u8; 32]> {
        let fp = read_text()
            .as_deref()
            .and_then(|t| saved_trust(t, server))
            .or_else(|| self.0.load(server))?;
        self.0.save(server, fp);
        Some(fp)
    }

    fn save(&self, server: &str, fp: [u8; 32]) {
        self.0.save(server, fp);
        if let Err(e) = edit(|doc| {
            put_trust(doc, server, &fp);
            Ok(())
        }) {
            // Windows 的控制台进程：配置文件在安装目录里，由服务里的被控端记下
            info!("can't save the server certificate fingerprint ({e:#}), keeping it in memory");
        }
    }
}

/// `[server_trust]` 里为 `server` 记下的指纹
fn saved_trust(text: &str, server: &str) -> Option<[u8; 32]> {
    let doc: DocumentMut = text.parse().ok()?;
    let t = doc.get("server_trust")?.as_table_like()?;
    if t.get("server")?.as_str()?.trim() != server {
        return None;
    }
    parse_fingerprint(t.get("fp")?.as_str()?).ok()?
}

fn put_trust(doc: &mut DocumentMut, server: &str, fp: &[u8; 32]) {
    let item = doc.entry("server_trust").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.decor_mut().set_prefix(format!("\n{}", trust_comment()));
        Item::Table(t)
    });
    if !item.is_table() {
        *item = Item::Table(toml_edit::Table::new());
    }
    item["server"] = toml_edit::value(server);
    item["fp"] = toml_edit::value(format_fingerprint(fp));
}

/// 写在 `[server_trust]` 前面的说明
fn trust_comment() -> String {
    tr!(
        "# 第一次连上中转服务器时记下的证书指纹（没有设置 server_fp 时使用）：之后服务器的证书变了就拒绝连接，防止有人冒充服务器。\n# 服务器确实换了证书时，删掉这一段（或者写上新的 server_fp），然后重新启动 Airhand\n",
        "# Certificate fingerprint remembered on the first connection to the relay server (used when server_fp isn't set): if the server's certificate changes later, Airhand refuses to connect, so nobody can impersonate the server.\n# If the server really got a new certificate, delete this section (or set the new server_fp) and restart Airhand\n"
    )
}

// ───────────────────────────── 写配置 ─────────────────────────────

/// 新建配置文件时的内容（按系统语言写注释）
fn template() -> String {
    let fp = if DEFAULT_SERVER_FP.is_empty() {
        tr!(
            "（未设置，第一次连接时记住服务器的证书）",
            "(none, the server's certificate is remembered on the first connection)"
        )
    } else {
        DEFAULT_SERVER_FP.to_string()
    };
    let server = DEFAULT_SERVER;
    #[cfg(not(windows))]
    {
        template_user(server, &fp)
    }
    #[cfg(windows)]
    {
        template_windows(server, &fp)
    }
}

/// Windows 的模板：文件在安装目录里，由系统服务维护
#[cfg(windows)]
fn template_windows(server: &str, fp: &str) -> String {
    let access = access_comment();
    tr!(
        r#"# Airhand 配置文件。修改后要重新启动 Airhand 才生效（托盘菜单"退出"，然后再打开 Airhand）。
# 这个文件由 Airhand 维护（保存控制码、开机自动启动、控制台的"我的设备"），手动修改的内容和注释都会保留。
# 不小心删掉了也没关系：Airhand 会按默认配置重新生成，控制码和我的设备不会丢。

# 中转服务器地址 host:port（UDP）。不设置时使用编译进程序的地址：{server}
# server = "relay.example.com:4433"

# 中转服务器证书的 SHA-256 指纹（服务器日志里打印的 64 位十六进制，可以用 : 或空格分隔）。
# 设为 "" 表示不固定指纹：第一次连接时记住服务器的证书（下面的 [server_trust]），之后证书变了就拒绝连接。不设置时使用编译进程序的指纹：{fp}
# server_fp = ""

{access}

# 开机自动启动：开机后被控端服务自动运行（登录之前也能被控制），登录后显示托盘图标。托盘菜单里也可以切换
autostart = true

# 控制码：第一次连上服务器时由服务器分配，之后一直使用同一个（重启、掉线、升级都不变）。
# 托盘菜单"更换控制码"会换一个新的；删掉 code 和 secret 则下次启动时重新分配。
# secret 是取回这个控制码的凭据，用 Windows DPAPI 加密过，只有这台电脑上的 Airhand 能解开
[code]
"#,
        r#"# Airhand configuration. Restart Airhand after editing it (tray menu "Quit", then open Airhand again).
# Airhand maintains this file (it saves the control code, the autostart setting and the console's "My devices"); your edits and comments are kept.
# If it gets deleted by accident, Airhand recreates it with the default settings, keeping the control code and your devices.

# Relay server address host:port (UDP). Without it the address built into the program is used: {server}
# server = "relay.example.com:4433"

# SHA-256 fingerprint of the relay server certificate (the 64 hex digits printed in the server log, `:` and spaces are allowed).
# "" means no fixed fingerprint: the server's certificate is remembered on the first connection ([server_trust] below) and a different one is refused later. Without it the built-in fingerprint is used: {fp}
# server_fp = ""

{access}

# Start with Windows: the host service runs after boot (the computer can be controlled before anyone signs in),
# and the tray icon shows up after signing in. Can also be changed in the tray menu
autostart = true

# Control code: assigned by the server the first time, then always the same (across restarts, disconnects and upgrades).
# "Change control code" in the tray menu gets a new one; delete code and secret to get a new one at the next start.
# secret proves to the server that this computer owns the code. It's encrypted with Windows DPAPI, only Airhand on this computer can read it
[code]
"#
    )
}

/// macOS / Linux 的模板：文件在用户自己的目录里，没有开机自动启动
#[cfg(not(windows))]
fn template_user(server: &str, fp: &str) -> String {
    let access = access_comment();
    let quit = if cfg!(target_os = "macos") {
        tr!("菜单栏图标的菜单里选\"退出\"", "\"Quit\" in the menu bar icon's menu")
    } else {
        tr!("托盘菜单\"退出\"", "tray menu \"Quit\"")
    };
    tr!(
        r#"# Airhand 配置文件。修改后要重新启动 Airhand 才生效（{quit}，然后再打开 Airhand）。
# 这个文件由 Airhand 维护（保存控制码和控制台的"我的设备"），手动修改的内容和注释都会保留。
# 不小心删掉了也没关系：Airhand 会按默认配置重新生成，控制码和我的设备不会丢。
# 里面的控制码能用来控制电脑，所以只有你的账户能读写这个文件（权限 600），不要发给别人。

# 中转服务器地址 host:port（UDP）。不设置时使用编译进程序的地址：{server}
# server = "relay.example.com:4433"

# 中转服务器证书的 SHA-256 指纹（服务器日志里打印的 64 位十六进制，可以用 : 或空格分隔）。
# 设为 "" 表示不固定指纹：第一次连接时记住服务器的证书（下面的 [server_trust]），之后证书变了就拒绝连接。不设置时使用编译进程序的指纹：{fp}
# server_fp = ""

{access}

# 本机的控制码：第一次连上服务器时由服务器分配，之后一直使用同一个（重启、掉线、升级都不变）。
# 菜单里"更换控制码"会换一个新的；删掉 code 和 secret 则下次启动时重新分配。
# secret 是取回这个控制码的凭据
[code]
"#,
        r#"# Airhand configuration. Restart Airhand after editing it ({quit}, then open Airhand again).
# Airhand maintains this file (it saves the control code and the console's "My devices"); your edits and comments are kept.
# If it gets deleted by accident, Airhand recreates it with the default settings, keeping the control code and your devices.
# The control codes in it can be used to control computers, so only your account can read and write it (mode 600). Don't share it.

# Relay server address host:port (UDP). Without it the address built into the program is used: {server}
# server = "relay.example.com:4433"

# SHA-256 fingerprint of the relay server certificate (the 64 hex digits printed in the server log, `:` and spaces are allowed).
# "" means no fixed fingerprint: the server's certificate is remembered on the first connection ([server_trust] below) and a different one is refused later. Without it the built-in fingerprint is used: {fp}
# server_fp = ""

{access}

# This computer's control code: assigned by the server the first time, then always the same (across restarts, disconnects and upgrades).
# "Change control code" in the menu gets a new one; delete code and secret to get a new one at the next start.
# secret proves to the server that this computer owns the code
[code]
"#
    )
}

/// 模板里 `access` 的说明
fn access_comment() -> String {
    tr!(
        "# 没配对过的控制端能不能连本机。会话是端到端加密的，中转服务器看不到画面和键盘输入，但它知道所有控制码：\n#   \"pairing\"：第一次连接时要输入托盘菜单里的配对码（配对码不经过服务器），配对之后记住它（默认）；\n#   \"open\"：知道控制码就能连。服务器也知道控制码，所以这等于信任中转服务器\n# access = \"pairing\"",
        "# Whether controllers that haven't paired with this computer may connect. Sessions are end-to-end encrypted, so the relay server can't see the screen or keystrokes, but it does know every control code:\n#   \"pairing\": the first connection needs the pairing code shown in the tray menu (it never goes through the server); paired controllers are remembered (default);\n#   \"open\": the control code alone is enough. The server knows it too, so this means trusting the relay server\n# access = \"pairing\""
    )
}

/// 写在第一台设备前面的说明
fn devices_comment() -> String {
    #[cfg(windows)]
    let how = tr!(
        "code 用你的 Windows 账户加密过，别的账户、别的电脑解不开；手动添加时直接写 6 位数字。",
        "code is encrypted with your Windows account; other accounts and computers can't read it. To add one by hand, just write the 6 digits."
    );
    #[cfg(not(windows))]
    let how = tr!(
        "code 是对方的控制码，有了它就能控制那台电脑。",
        "code is the remote computer's control code: anyone who has it can control that computer."
    );
    tr!(
        "# 我的设备：控制台连接过的电脑，连上时自动保存（\"新建连接\"页里的\"我的设备\"，最多 50 台）。\n# {how}\n",
        "# My devices: computers the console has connected to, saved automatically (\"My devices\" on the New connection page, up to 50).\n# {how}\n"
    )
}

/// 本进程最近一次读到 / 写入的配置文件内容。文件被删掉时从这里接着用控制码、开机自动启动、我的设备（见 [`regenerate`]）。
/// 同时也是读改写配置文件的锁：被控端保存控制码、控制台保存设备可能同时发生
static LAST: Mutex<Option<String>> = Mutex::new(None);

fn last() -> MutexGuard<'static, Option<String>> {
    LAST.lock().unwrap_or_else(|e| e.into_inner())
}

/// 按模板（默认配置）生成配置文件的内容。`before` 是文件被删掉之前的内容：服务器等设置回到默认，
/// 状态接着用——开机自动启动（Windows）、本机的控制码、我的设备。只搬值，模板里的说明和顺序不动
fn regenerate(before: Option<&str>) -> Result<DocumentMut> {
    let mut doc: DocumentMut = template().parse()?;
    let Some(old) = before.and_then(|t| t.parse::<DocumentMut>().ok()) else {
        return Ok(doc);
    };
    // Windows 的模板里有；macOS 的模板里没有，之前有的话（已经按默认打开过登录时自动启动）照样加上
    if let Some(on) = old.get("autostart").and_then(Item::as_bool)
        && (doc.contains_key("autostart") || cfg!(target_os = "macos"))
    {
        put_autostart(&mut doc, on);
    }
    if let (Some(from), Some(to)) = (
        old.get("code").and_then(Item::as_table_like),
        doc.get_mut("code").and_then(Item::as_table_like_mut),
    ) {
        for (k, v) in from.iter() {
            to.insert(k, v.clone());
        }
    }
    // 记下的服务器指纹：不重新记（重新记的话，那一次连接就防不了中间人）
    if let Some((server, fp)) = old.get("server_trust").and_then(|t| {
        let server = t.get("server")?.as_str()?.to_string();
        let fp = parse_fingerprint(t.get("fp")?.as_str()?).ok()??;
        Some((server, fp))
    }) {
        put_trust(&mut doc, &server, &fp);
    }
    // 端到端加密的身份、配对码、配对过的控制端：丢了的话控制端会认为本机"身份变了"，配对过的也要重新配对
    for key in ["host", "controller"] {
        if let Some(item) = old.get(key).filter(|i| i.is_table()) {
            doc[key] = item.clone();
        }
    }
    if let Some(item) = old.get("paired").filter(|i| i.is_array_of_tables()) {
        doc["paired"] = item.clone();
    }
    // 设备写在最后，前面有自己的说明
    if let Some(item) = old.get("device").filter(|i| i.is_array_of_tables()) {
        doc["device"] = item.clone();
    }
    Ok(doc)
}

/// 修改配置文件（不存在时按默认配置重新生成）。没有配置文件时什么也不做
fn edit(f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    match path() {
        Some(path) => edit_at(&path, f),
        None => Ok(()),
    }
}

fn edit_at(path: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let mut last = last();
    if FROZEN.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    let mut doc = match std::fs::read_to_string(path) {
        // 用户写坏了的文件不覆盖：这次就不保存了
        Ok(text) => text.parse::<DocumentMut>().map_err(|e| {
            anyhow::anyhow!("{} isn't valid TOML, leaving it alone: {e}", path.display())
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => regenerate(last.as_deref())?,
        Err(e) => return Err(e.into()),
    };
    f(&mut doc)?;
    let text = doc.to_string();
    write(path, &text)?;
    *last = Some(text);
    Ok(())
}

/// 先写临时文件再改名：写到一半断电也不会把配置文件弄坏。目录不存在时创建（macOS / Linux 第一次运行、
/// 整个目录被删掉了）；macOS / Linux 上目录只有本账户能进（700），文件只有本账户能读写（600）
fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        // umask 只会去掉权限，这里再明确设一次
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 配置文件不存在时按默认配置新建，存在时记下它的内容（见 [`keep_file`]）。
/// 维护配置文件的进程启动时调用（Windows 安装版是服务里的被控端，其它情况是托盘进程），安装时也调用
pub fn ensure_file() -> Result<()> {
    let Some(path) = path() else {
        return Ok(());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if text.parse::<DocumentMut>().is_ok() {
                *last() = Some(text);
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            edit(|_| Ok(()))?;
            info!("created {}", path.display());
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// 运行中配置文件被删掉了（文件或者整个目录）：几秒之内按默认配置重新生成，控制码、开机自动启动、
/// 我的设备从删掉之前的内容接着用。只在维护配置文件的进程里调用（见 [`ensure_file`]）
pub fn keep_file() {
    let Some(path) = path() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("airhand-config".into())
        .spawn(move || {
            let mut seen = None;
            loop {
                std::thread::sleep(KEEP_CHECK);
                #[cfg(test)]
                if STOP_KEEPING.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if FROZEN.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                match std::fs::metadata(&path) {
                    Ok(m) => {
                        // 文件变了（包括手动修改）：记下新的内容，删掉之后接着用的是最新的
                        let modified = m.modified().ok();
                        if modified != seen {
                            seen = modified;
                            let mut last = last();
                            if let Ok(text) = std::fs::read_to_string(&path)
                                && text.parse::<DocumentMut>().is_ok()
                            {
                                *last = Some(text);
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        seen = None;
                        match edit_at(&path, |_| Ok(())) {
                            Ok(()) => info!("{} was deleted, recreated it", path.display()),
                            Err(e) => warn!("can't recreate {}: {e:#}", path.display()),
                        }
                    }
                    Err(_) => {}
                }
            }
        });
    if let Err(e) = spawned {
        warn!("can't watch the configuration file: {e}");
    }
}

/// 卸载（macOS 菜单里的"卸载 Airhand…"）：从现在起不再写配置文件，配置文件被删掉也不再重新生成。
/// 还在运行的东西（例如正在停下来的被控端保存控制码）也写不回去。返回时，正在写的那一次已经写完
#[cfg(target_os = "macos")]
pub fn freeze() {
    let _wait = last();
    FROZEN.store(true, std::sync::atomic::Ordering::Release);
}

/// 保存服务器分配的控制码；`None` 表示放弃了原来的控制码（下次启动时重新分配）
pub fn save_code(code: Option<Reclaim>) -> Result<()> {
    edit(|doc| {
        let table = doc
            .entry("code")
            .or_insert(toml_edit::table())
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("`code` in config.toml must be a table"))?;
        match code {
            Some(r) => {
                table["code"] = toml_edit::value(i64::from(r.code));
                table["secret"] = toml_edit::value(secret::seal(&r.secret)?);
            }
            None => {
                table.remove("code");
                table.remove("secret");
            }
        }
        Ok(())
    })
}

/// 保存开机自动启动的设置（macOS：登录时自动启动的记录）
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn save_autostart(on: bool) -> Result<()> {
    edit(|doc| {
        put_autostart(doc, on);
        Ok(())
    })
}

/// macOS：还没有按默认打开过登录时自动启动（配置文件里还没有 `autostart`）。
///
/// macOS 的模板里没有这一项：第一次从"应用程序"文件夹运行时，Airhand 和 Windows 一样默认打开登录时自动启动，
/// 然后记下 `autostart`，之后再也不会自己打开——用户关掉了就一直关着（见 [`crate::login_item`]）。
/// 配置文件不存在、写坏了时返回 `false`：判断不了，宁可不打开
#[cfg(target_os = "macos")]
pub fn autostart_default_pending() -> bool {
    read_text()
        .and_then(|t| t.parse::<DocumentMut>().ok())
        .is_some_and(|doc| !doc.contains_key("autostart"))
}

/// 写入 `autostart`。已经有了就只改值；没有（macOS 的模板里没有）时加上一段说明，放在文件开头的说明后面
/// （直接插入的话 toml_edit 会把它排在整个文件的第一行，开头的说明前面）
fn put_autostart(doc: &mut DocumentMut, on: bool) {
    if let Some(item) = doc.get_mut("autostart") {
        *item = toml_edit::value(on);
        return;
    }
    let note = autostart_note();
    // 文件开头的说明属于排在最前面的那一项：已经有别的值（例如用户写了 server）时是那个值，
    // 否则是第一个表（模板里的 [code]），这时在最后一个空行处分开，前半段（开头的说明）移到 autostart 前面
    let has_values = doc.iter().any(|(_, item)| item.is_value());
    let mut prefix = format!("\n{note}");
    if !has_values
        && let Some((_, item)) = doc.iter_mut().find(|(_, item)| item.is_table())
        && let Some(table) = item.as_table_mut()
    {
        let old = table
            .decor()
            .prefix()
            .and_then(|p| p.as_str())
            .unwrap_or_default()
            .to_string();
        let (head, tail) = old
            .rfind("\n\n")
            .map_or((old.as_str(), ""), |i| old.split_at(i + 1));
        prefix = format!("{head}\n{note}");
        table
            .decor_mut()
            .set_prefix(format!("\n{}", tail.trim_start_matches('\n')));
    }
    doc.insert("autostart", toml_edit::value(on));
    if let Some(mut key) = doc.as_table_mut().key_mut("autostart") {
        key.leaf_decor_mut().set_prefix(prefix);
    }
}

/// macOS 上 `autostart` 前面的说明
fn autostart_note() -> String {
    tr!(
        "# 登录时自动启动（\"系统设置 → 通用 → 登录项\"里的\"登录时打开\"）。第一次从\"应用程序\"文件夹运行时默认打开。\n# 以系统设置为准：在菜单栏图标的菜单里切换、或者在系统设置里改，这里都会跟着更新；只改这一项不起作用\n",
        "# Open at login (\"Open at Login\" in System Settings → General → Login Items). Turned on the first time Airhand runs from the Applications folder.\n# System Settings is what counts: switch it in the menu bar icon's menu or in System Settings and this line follows; editing only this line has no effect\n"
    )
}

/// 保存控制台的"我的设备"：整个列表换成 `list`（[`crate::devices::for_file`] 整理过的，`code` 已经加密）
pub fn save_devices(list: &[crate::devices::Device]) -> Result<()> {
    edit(|doc| {
        put_devices(doc, list);
        Ok(())
    })
}

fn put_devices(doc: &mut DocumentMut, list: &[crate::devices::Device]) {
    let mut arr = toml_edit::ArrayOfTables::new();
    for t in list
        .iter()
        .take(crate::devices::MAX_IN_FILE)
        .filter_map(crate::devices::to_table)
    {
        arr.push(t);
    }
    if let Some(first) = arr.get_mut(0) {
        first.decor_mut().set_prefix(format!("\n{}", devices_comment()));
    }
    doc.remove("device");
    if !arr.is_empty() {
        doc.insert("device", Item::ArrayOfTables(arr));
    }
}

// ───────────────────────────── 端到端加密的身份和配对 ─────────────────────────────

/// 配对过的一个控制端（`[[paired]]`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    /// 公钥（64 位十六进制）
    pub key: String,
    /// 它配对时报的名称
    pub name: String,
    /// 配对的时间（Unix 毫秒）
    pub since: i64,
}

/// 本机作为被控端的端到端状态（`[host]` 和 `[[paired]]`）
#[derive(Debug, Default)]
pub struct HostSecurity {
    /// 身份密钥（私钥）；没有、解不开时为 `None`
    pub key: Option<[u8; 32]>,
    /// 配对码
    pub pairing: Option<String>,
    /// 有人连续输错配对码，暂停了配对（本机用户换一个配对码才恢复）
    pub pairing_locked: bool,
    pub paired: Vec<Paired>,
}

/// 写在 `[host]` 前面的说明
fn host_comment() -> String {
    tr!(
        "# 本机作为被控端的端到端加密身份（key）和配对码（pairing），由 Airhand 维护，都是加密保存的。\n# 删掉 key 会生成新的身份：连过本机的控制端会提示本机的身份变了\n",
        "# This computer's end-to-end encryption identity as a host (key) and its pairing code (pairing). Maintained by Airhand, stored encrypted.\n# Deleting key creates a new identity: controllers that connected before will warn that this computer's identity changed\n"
    )
}

/// 写在第一个 `[[paired]]` 前面的说明
fn paired_comment() -> String {
    tr!(
        "# 配对过的控制端（它们的公钥）：不用配对码就能连本机。删掉一项就是忘掉那个控制端\n",
        "# Paired controllers (their public keys): they can connect without the pairing code. Delete an entry to forget that controller\n"
    )
}

/// 本机作为被控端的端到端状态（每次重新读文件）。密钥解不开（例如配置文件是从别的电脑拷来的）当作没有
pub fn host_security() -> HostSecurity {
    let Some(doc) = read_text().and_then(|t| t.parse::<DocumentMut>().ok()) else {
        return HostSecurity::default();
    };
    host_security_in(&doc)
}

fn host_security_in(doc: &DocumentMut) -> HostSecurity {
    let mut s = HostSecurity::default();
    if let Some(t) = doc.get("host").and_then(Item::as_table_like) {
        let open = |k: &str, kind| {
            let text = t.get(k)?.as_str()?;
            match secret::open_bytes(text, kind) {
                Ok(b) => Some(b),
                Err(e) => {
                    warn!("config.toml: can't read [host] {k} ({e:#})");
                    None
                }
            }
        };
        s.key = open("key", secret::Kind::HostKey).and_then(|b| <[u8; 32]>::try_from(b).ok());
        s.pairing = open("pairing", secret::Kind::Pairing)
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|p| airhand_proto::parse_pairing(&p));
        s.pairing_locked = t
            .get("pairing_locked")
            .and_then(Item::as_bool)
            .unwrap_or(false);
    }
    if let Some(arr) = doc.get("paired").and_then(Item::as_array_of_tables) {
        s.paired = arr
            .iter()
            .filter_map(|t| {
                let key = t.get("key")?.as_str()?.trim().to_ascii_lowercase();
                airhand_link::secure::unhex32(&key)?;
                Some(Paired {
                    key,
                    name: t
                        .get("name")
                        .and_then(Item::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(64)
                        .collect(),
                    since: match t.get("since") {
                        Some(Item::Value(toml_edit::Value::Datetime(d))) => {
                            crate::devices::to_millis(d.value()).unwrap_or(0)
                        }
                        _ => 0,
                    },
                })
            })
            .collect();
    }
    s
}

/// 保存本机作为被控端的身份密钥
pub fn save_host_key(key: &[u8; 32]) -> Result<()> {
    let sealed = secret::seal_bytes(key, secret::Kind::HostKey)?;
    edit(|doc| {
        host_table(doc)?["key"] = toml_edit::value(sealed);
        Ok(())
    })
}

/// 保存配对码和它有没有暂停
pub fn save_pairing(code: &str, locked: bool) -> Result<()> {
    let sealed = secret::seal_bytes(code.as_bytes(), secret::Kind::Pairing)?;
    edit(|doc| {
        let t = host_table(doc)?;
        t["pairing"] = toml_edit::value(sealed);
        t["pairing_locked"] = toml_edit::value(locked);
        Ok(())
    })
}

/// 保存配对过的控制端（整个换掉）
pub fn save_paired(list: &[Paired]) -> Result<()> {
    edit(|doc| {
        put_paired(doc, list);
        Ok(())
    })
}

fn put_paired(doc: &mut DocumentMut, list: &[Paired]) {
    let mut arr = toml_edit::ArrayOfTables::new();
    for p in list.iter().take(MAX_PAIRED) {
        let mut t = toml_edit::Table::new();
        t["key"] = toml_edit::value(&p.key);
        t["name"] = toml_edit::value(&p.name);
        if let Some(dt) = crate::devices::from_millis(p.since) {
            t["since"] = toml_edit::value(dt);
        }
        arr.push(t);
    }
    if let Some(first) = arr.get_mut(0) {
        first.decor_mut().set_prefix(format!("\n{}", paired_comment()));
    }
    doc.remove("paired");
    if !arr.is_empty() {
        doc.insert("paired", Item::ArrayOfTables(arr));
    }
}

/// 最多记住这么多个配对过的控制端（再多就忘掉最早的）
pub const MAX_PAIRED: usize = 64;

fn host_table(doc: &mut DocumentMut) -> Result<&mut toml_edit::Table> {
    let item = doc.entry("host").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.decor_mut().set_prefix(format!("\n{}", host_comment()));
        Item::Table(t)
    });
    item.as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`host` in config.toml must be a table"))
}

/// 控制台的身份（端到端握手里的静态密钥）：`[controller]` 里这个账户解得开的那一把。
///
/// Windows 上配置文件是所有账户共用的，每个账户一把，用各自的 DPAPI 加密（和我的设备里的控制码一样）。
/// 没有就生成一把，返回要保存的那一项（加密过的）：调用方自己写进去（[`save_controller_key`]），
/// Windows 安装版经命名管道交给服务里的被控端写
pub fn controller_identity() -> (Identity, Option<String>) {
    let doc = read_text().and_then(|t| t.parse::<DocumentMut>().ok());
    let found = doc.as_ref().and_then(|doc| {
        doc.get("controller")?
            .get("keys")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str())
            .find_map(|s| secret::open_bytes(s, secret::Kind::ControllerKey).ok())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
    });
    if let Some(k) = found {
        return (Identity::from_private(k), None);
    }
    let id = Identity::generate();
    let sealed = match secret::seal_bytes(id.private(), secret::Kind::ControllerKey) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("can't encrypt the console's identity ({e:#}), it's only kept until restart");
            None
        }
    };
    (id, sealed)
}

/// `[controller]` 里最多这么多把（Windows 上每个账户一把）
const MAX_CONTROLLER_KEYS: usize = 32;

/// 记下控制台的身份（[`controller_identity`] 返回的那一项）。别的账户的原样保留
pub fn save_controller_key(sealed: &str) -> Result<()> {
    let sealed = sealed.trim();
    anyhow::ensure!(
        !sealed.is_empty() && sealed.len() <= 1024 && !sealed.contains(['\n', '\r']),
        "invalid controller key"
    );
    edit(|doc| {
        let item = doc.entry("controller").or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.decor_mut().set_prefix(format!("\n{}", controller_comment()));
            Item::Table(t)
        });
        let t = item
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("`controller` in config.toml must be a table"))?;
        let mut keys = t
            .get("keys")
            .and_then(Item::as_array)
            .cloned()
            .unwrap_or_default();
        if !keys.iter().any(|v| v.as_str() == Some(sealed)) {
            keys.push(sealed);
        }
        while keys.len() > MAX_CONTROLLER_KEYS {
            keys.remove(0);
        }
        t["keys"] = toml_edit::value(keys);
        Ok(())
    })
}

/// 写在 `[controller]` 前面的说明
fn controller_comment() -> String {
    tr!(
        "# 控制台的端到端加密身份（加密保存，Windows 上每个账户一把）。配对过的电脑靠它认出这个控制台；删掉了就要重新配对\n",
        "# The console's end-to-end encryption identity (stored encrypted, one per account on Windows). Paired computers recognize this console by it; delete it and you'll have to pair again\n"
    )
}

/// 加密保存的东西：取回控制码用的密钥、"我的设备"里的控制码、端到端加密的身份密钥和配对码。
///
/// Windows：config.toml 在安装目录里，本机所有账户都能读，所以用 DPAPI 加密（只有同一个账户能解开：
/// 被控端的密钥是 SYSTEM，设备的控制码是保存它的用户），配置文件被别人读到、拷走也没用。
/// macOS / Linux：文件只有本账户能读（600），不加密。
pub(crate) mod secret {
    use anyhow::{Result, bail};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    #[cfg(windows)]
    const ENTROPY: &[u8] = b"airhand control code secret v1";
    #[cfg(windows)]
    const DEVICE_ENTROPY: &[u8] = b"airhand saved device code v1";

    /// 端到端加密用到的密文：各用各的 DPAPI 熵，不能互相冒充
    #[derive(Debug, Clone, Copy)]
    pub enum Kind {
        /// 本机作为被控端的身份密钥（Windows：SYSTEM 的 DPAPI）
        HostKey,
        /// 配对码（同上）
        Pairing,
        /// 控制台的身份密钥（Windows：用户的 DPAPI）
        ControllerKey,
    }

    #[cfg(windows)]
    fn entropy(kind: Kind) -> &'static [u8] {
        match kind {
            Kind::HostKey => b"airhand host identity v1",
            Kind::Pairing => b"airhand pairing code v1",
            Kind::ControllerKey => b"airhand console identity v1",
        }
    }

    /// 加密保存一段字节（Windows：DPAPI；macOS / Linux：文件只有本账户能读，只是 base64）
    pub fn seal_bytes(data: &[u8], kind: Kind) -> Result<String> {
        #[cfg(windows)]
        {
            Ok(STANDARD.encode(dpapi(data, entropy(kind), true)?))
        }
        #[cfg(not(windows))]
        {
            let _ = kind;
            Ok(STANDARD.encode(data))
        }
    }

    pub fn open_bytes(text: &str, kind: Kind) -> Result<Vec<u8>> {
        let raw = STANDARD.decode(text.trim())?;
        #[cfg(windows)]
        let raw = dpapi(&raw, entropy(kind), false)?;
        #[cfg(not(windows))]
        let _ = kind;
        Ok(raw)
    }

    pub fn seal(secret: &[u8; 16]) -> Result<String> {
        #[cfg(windows)]
        {
            Ok(STANDARD.encode(dpapi(secret, ENTROPY, true)?))
        }
        #[cfg(not(windows))]
        {
            Ok(STANDARD.encode(secret))
        }
    }

    pub fn open(text: &str) -> Result<[u8; 16]> {
        let raw = STANDARD.decode(text.trim())?;
        #[cfg(windows)]
        let raw = dpapi(&raw, ENTROPY, false)?;
        match <[u8; 16]>::try_from(raw.as_slice()) {
            Ok(s) => Ok(s),
            Err(_) => bail!("the secret has the wrong length"),
        }
    }

    /// "我的设备"里的控制码写进文件之前（macOS / Linux 原样）
    pub fn seal_device_code(code: &str) -> Result<String> {
        #[cfg(windows)]
        {
            Ok(STANDARD.encode(dpapi(code.as_bytes(), DEVICE_ENTROPY, true)?))
        }
        #[cfg(not(windows))]
        {
            Ok(code.to_string())
        }
    }

    /// 解开加密过的控制码（6 位数字的不用解，调用方直接用）。macOS / Linux 不加密，这里总是失败
    pub fn open_device_code(text: &str) -> Result<String> {
        #[cfg(windows)]
        {
            let raw = dpapi(&STANDARD.decode(text.trim())?, DEVICE_ENTROPY, false)?;
            Ok(String::from_utf8(raw)?)
        }
        #[cfg(not(windows))]
        {
            bail!("{text:?} isn't a control code")
        }
    }

    #[cfg(windows)]
    fn dpapi(data: &[u8], entropy: &[u8], protect: bool) -> Result<Vec<u8>> {
        use anyhow::Context;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        };

        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let entropy = CRYPT_INTEGER_BLOB {
            cbData: entropy.len() as u32,
            pbData: entropy.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    windows::core::w!("Airhand"),
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
                .context("CryptProtectData")?;
            } else {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                )
                .context("CryptUnprotectData (was config.toml copied from another computer?)")?;
            }
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(Some(HLOCAL(out.pbData.cast())));
            Ok(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第一次连上服务器时记下的指纹：跟着服务器地址走，再记只改值，文件被删掉重新生成时接着用
    #[test]
    fn server_trust_roundtrip_and_survives_regeneration() {
        let mut doc: DocumentMut = template().parse().unwrap();
        put_trust(&mut doc, "relay.example.com:4433", &[0xab; 32]);
        let text = doc.to_string();
        assert!(text.contains(&trust_comment()));
        assert_eq!(
            saved_trust(&text, "relay.example.com:4433"),
            Some([0xab; 32])
        );
        assert_eq!(saved_trust(&text, "other:4433"), None);
        put_trust(&mut doc, "other:4433", &[1; 32]);
        let text = doc.to_string();
        assert_eq!(text.lines().filter(|l| *l == "[server_trust]").count(), 1);
        assert_eq!(text.matches(trust_comment().as_str()).count(), 1);
        assert_eq!(saved_trust(&text, "other:4433"), Some([1; 32]));
        let again = regenerate(Some(&text)).unwrap().to_string();
        assert_eq!(saved_trust(&again, "other:4433"), Some([1; 32]));
        assert!(parse(&again).is_ok());
    }

    #[test]
    fn fingerprint_check_matches_the_parser() {
        let hex = "ab".repeat(32);
        for s in [
            String::new(),
            hex.clone(),
            ["AB"; 32].join(":"),
            format!(" {hex}\n"),
            "ab".repeat(31),
            format!("sha256: {hex}"),
            "é".into(),
        ] {
            assert_eq!(
                super::fingerprint_is_valid(&s),
                airhand_link::net::parse_fingerprint(&s).is_ok(),
                "{s:?}"
            );
        }
    }

    /// 模板就是默认配置；保存控制码、开关自动启动之后能读回来，用户的修改和注释都还在
    #[test]
    fn template_and_edits_roundtrip() {
        let mut text = template();
        assert_eq!(parse(&text).unwrap(), Settings::default());

        // 用户改了服务器地址，加了自己的注释
        text = text.replacen(
            "# server = \"relay.example.com:4433\"",
            "# 我的服务器\nserver = \"relay.example.com:4433\"",
            1,
        );
        let edit = |text: &str, f: &dyn Fn(&mut DocumentMut) -> Result<()>| {
            let mut doc: DocumentMut = text.parse().unwrap();
            f(&mut doc).unwrap();
            doc.to_string()
        };
        let code = Reclaim {
            code: 123_456,
            secret: [7; 16],
        };
        text = edit(&text, &|doc| {
            let t = doc["code"].as_table_mut().unwrap();
            t["code"] = toml_edit::value(i64::from(code.code));
            t["secret"] = toml_edit::value(secret::seal(&code.secret)?);
            doc["autostart"] = toml_edit::value(false);
            Ok(())
        });
        let s = parse(&text).unwrap();
        assert_eq!(s.server, "relay.example.com:4433");
        assert_eq!(s.code(), Some(code));
        assert!(!s.autostart);
        assert!(text.contains("# 我的服务器"));

        // 放弃控制码：表还在（注释也在），只是没有内容
        text = edit(&text, &|doc| {
            let t = doc["code"].as_table_mut().unwrap();
            t.remove("code");
            t.remove("secret");
            Ok(())
        });
        assert_eq!(parse(&text).unwrap().code(), None);
        assert!(text.contains("[code]"));

        // 别的账户 / 别的电脑加密的密钥打不开：当作没有控制码
        let foreign = "[code]\ncode = 123456\nsecret = \"AAAA\"\n";
        assert_eq!(parse(foreign).unwrap().code(), None);
    }

    fn device(code: &str) -> crate::devices::Device {
        crate::devices::Device {
            code: code.into(),
            alias: None,
            name: format!("PC-{code}"),
            os: "macOS 15.3".into(),
            resume: None,
            last_used: 1_762_597_230_000,
            quality: None,
            host_key: None,
        }
    }

    /// 被控端的身份、配对码、配对过的控制端、控制台的身份：保存之后读得回来，文件被删掉重新生成时接着用
    #[test]
    fn security_state_roundtrip_and_survives_regeneration() {
        let mut doc: DocumentMut = template().parse().unwrap();
        let key = [9u8; 32];
        host_table(&mut doc).unwrap()["key"] =
            toml_edit::value(secret::seal_bytes(&key, secret::Kind::HostKey).unwrap());
        let t = host_table(&mut doc).unwrap();
        t["pairing"] = toml_edit::value(secret::seal_bytes(b"482913", secret::Kind::Pairing).unwrap());
        t["pairing_locked"] = toml_edit::value(true);
        let paired = vec![Paired {
            key: "cd".repeat(32),
            name: "DESIGN-PC".into(),
            since: 1_762_597_230_000,
        }];
        put_paired(&mut doc, &paired);
        let text = doc.to_string();
        assert!(text.contains(&host_comment()));
        assert!(text.contains(&paired_comment()));
        let s = host_security_in(&text.parse().unwrap());
        assert_eq!(s.key, Some(key));
        assert_eq!(s.pairing.as_deref(), Some("482913"));
        assert!(s.pairing_locked);
        assert_eq!(s.paired, paired);
        // 设置不受影响
        assert_eq!(parse(&text).unwrap(), Settings::default());
        // 文件被删掉之后重新生成：身份和配对都还在
        let again = regenerate(Some(&text)).unwrap();
        let s = host_security_in(&again);
        assert_eq!(s.key, Some(key));
        assert_eq!(s.paired, paired);

        // access：只有写明了 open 才不要配对码
        assert_eq!(parse("access = \"open\"").unwrap().access, Access::Open);
        assert_eq!(parse("access = \"opne\"").unwrap().access, Access::Pairing);
        assert_eq!(parse("").unwrap().access, Access::Pairing);
    }

    /// 我的设备写在 [code] 后面，带一段说明；再次保存时整个换掉，说明只有一份；删光了就没有 [[device]]
    #[test]
    fn devices_in_the_file() {
        let mut doc: DocumentMut = template().parse().unwrap();
        put_devices(&mut doc, &[device("482913"), device("731205")]);
        let text = doc.to_string();
        assert_eq!(text.matches("[[device]]").count(), 2);
        assert!(text.find("[code]").unwrap() < text.find("[[device]]").unwrap());
        let comment = devices_comment();
        let first_line = comment.lines().next().unwrap();
        assert!(text.contains(first_line));
        let reread: DocumentMut = text.parse().unwrap();
        assert_eq!(crate::devices::entries(&reread), vec![device("482913"), device("731205")]);
        // 设置不受影响
        assert_eq!(parse(&text).unwrap(), Settings::default());

        let mut doc = reread;
        put_devices(&mut doc, &[device("731205")]);
        let text = doc.to_string();
        assert_eq!(text.matches("[[device]]").count(), 1);
        assert_eq!(text.matches(first_line).count(), 1);

        let mut doc: DocumentMut = text.parse().unwrap();
        put_devices(&mut doc, &[]);
        assert!(!doc.to_string().contains("[[device]]"));
        assert!(doc.to_string().contains("[code]"));
    }

    /// 文件被删掉后重新生成：服务器等设置回到默认，控制码、开机自动启动、我的设备接着用
    #[test]
    fn regenerate_keeps_state() {
        let mut doc: DocumentMut = template().parse().unwrap();
        doc["server"] = toml_edit::value("relay.example.com:4433");
        doc["autostart"] = toml_edit::value(false);
        let code = Reclaim {
            code: 654_321,
            secret: [9; 16],
        };
        let t = doc["code"].as_table_mut().unwrap();
        t["code"] = toml_edit::value(i64::from(code.code));
        t["secret"] = toml_edit::value(secret::seal(&code.secret).unwrap());
        put_devices(&mut doc, &[device("482913")]);
        let before = doc.to_string();

        let after = regenerate(Some(&before)).unwrap().to_string();
        let s = parse(&after).unwrap();
        assert_eq!(s.server, DEFAULT_SERVER);
        assert_eq!(s.code(), Some(code));
        // 开机自动启动（Windows）、登录时自动启动的记录（macOS）接着用；Linux 没有这一项，是默认值
        assert_eq!(s.autostart, cfg!(target_os = "linux"));
        assert_eq!(
            crate::devices::entries(&after.parse().unwrap()),
            vec![device("482913")]
        );
        // 模板的注释都还在，而且在原来的位置
        let tpl = template();
        let secret_note = tpl.lines().find(|l| l.starts_with("# secret")).unwrap();
        assert!(after.lines().next().unwrap().starts_with("# Airhand"));
        assert!(after.find(secret_note).unwrap() < after.find("[code]").unwrap());

        // 之前的文件是手写的、没有注释：生成的文件照样有说明
        let bare = format!(
            "[code]\ncode = 654321\nsecret = \"{}\"\n",
            secret::seal(&code.secret).unwrap()
        );
        let after = regenerate(Some(&bare)).unwrap().to_string();
        assert!(after.contains(secret_note));
        assert_eq!(parse(&after).unwrap().code(), Some(code));

        // 不知道之前的内容（或者之前的内容坏了）：就是默认配置
        assert_eq!(regenerate(None).unwrap().to_string(), template());
        assert_eq!(regenerate(Some("[code")).unwrap().to_string(), template());
    }

    /// macOS / Linux 的模板里没有 autostart（macOS 上有没有它表示是否已经按默认打开过登录时自动启动）。
    /// 加上时带一段说明，排在文件开头的说明后面、[code] 的说明前面；之后只改值
    #[test]
    #[cfg(not(windows))]
    fn autostart_goes_after_the_header() {
        let tpl = template();
        let mut doc: DocumentMut = tpl.parse().unwrap();
        assert!(!doc.contains_key("autostart"));
        put_autostart(&mut doc, true);
        let text = doc.to_string();
        println!("{text}");
        let note = autostart_note();
        let note = note.lines().next().unwrap();
        let first_line = tpl.lines().next().unwrap();
        let fp_line = tpl.lines().find(|l| l.starts_with("# server_fp")).unwrap();
        let code_note = tpl
            .lines()
            .rev()
            .skip_while(|l| !l.starts_with("[code]"))
            .skip(1)
            .take_while(|l| l.starts_with('#'))
            .last()
            .unwrap();
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("missing {s:?}"));
        assert!(text.starts_with(first_line), "开头的说明不在第一行了");
        assert!(at(fp_line) < at(note));
        assert!(at(note) < at("autostart = true"));
        assert!(at("autostart = true") < at(code_note));
        assert!(at(code_note) < at("[code]"));
        // 空行隔开，不和前后的说明挤在一起
        assert!(text.contains(&format!("\n\n{note}")));
        assert!(text.contains(&format!("autostart = true\n\n{code_note}")));
        // 模板里原有的说明一行都不少
        for line in tpl.lines().filter(|l| !l.is_empty()) {
            assert!(text.contains(line), "少了 {line:?}");
        }
        assert!(parse(&text).unwrap().autostart);

        // 再改只改值，说明只有一份
        put_autostart(&mut doc, false);
        let text = doc.to_string();
        assert_eq!(text.matches(note).count(), 1);
        assert!(!parse(&text).unwrap().autostart);

        // 用户自己写了 server：加在它后面，开头的说明不动
        let mine = tpl.replacen(
            "# server = \"relay.example.com:4433\"",
            "server = \"relay.example.com:4433\"",
            1,
        );
        let mut doc: DocumentMut = mine.parse().unwrap();
        put_autostart(&mut doc, true);
        let text = doc.to_string();
        assert!(text.starts_with(first_line));
        assert!(text.find("server = ").unwrap() < text.find("autostart = true").unwrap());
        let s = parse(&text).unwrap();
        assert!(s.autostart);
        assert_eq!(s.server, "relay.example.com:4433");
    }

    /// 真正的文件：
    /// * 没有时按默认配置生成（macOS / Linux：目录 700、文件 600），保存的控制码和设备读得回来；
    /// * 运行中文件、或者整个目录被删掉，很快就按默认配置重新生成，控制码和设备还在，删之前手动改过的也在
    #[test]
    fn file_lifecycle() {
        use std::time::Instant;
        let root = std::env::temp_dir().join(format!("airhand-config-test-{:016x}", rand::random::<u64>()));
        let dir = root.join("airhand");
        let file = dir.join(FILE_NAME);
        TEST_PATH.with(|p| *p.borrow_mut() = Some(file.clone()));
        let wait_for = |what: &str, ok: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ok() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        #[cfg(unix)]
        let mode = |p: &Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        };

        // 第一次运行：目录和文件都没有
        ensure_file().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), template());
        #[cfg(unix)]
        {
            assert_eq!(mode(&file), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }

        let code = Reclaim {
            code: 654_321,
            secret: [3; 16],
        };
        save_code(Some(code)).unwrap();
        let mut d = device("482913");
        save_devices(&crate::devices::for_file(vec![d.clone()]).unwrap()).unwrap();
        assert_eq!(crate::devices::load(), vec![d.clone()]);
        assert_eq!(parse(&read_text().unwrap()).unwrap().code(), Some(code));
        // Windows：文件里没有明文的控制码
        #[cfg(windows)]
        assert!(!read_text().unwrap().contains("code = \"482913\""));
        #[cfg(not(windows))]
        assert!(read_text().unwrap().contains("code = \"482913\""));

        keep_file();
        std::fs::remove_file(&file).unwrap();
        wait_for("the file to be recreated", &|| file.exists());
        let text = read_text().unwrap();
        assert!(text.starts_with("# Airhand"));
        assert_eq!(parse(&text).unwrap().code(), Some(code));
        assert_eq!(crate::devices::load(), vec![d.clone()]);

        // 手动给设备起了个名字，然后整个目录被删掉
        let edited = text.replace("name = \"PC-482913\"", "name = \"PC-482913\"\nalias = \"书房\"");
        assert_ne!(edited, text);
        std::fs::write(&file, edited).unwrap();
        d.alias = Some("书房".into());
        wait_for("the edit to be noticed", &|| {
            last().as_deref().is_some_and(|t| t.contains("书房"))
        });
        std::fs::remove_dir_all(&dir).unwrap();
        wait_for("the directory to be recreated", &|| file.exists());
        assert_eq!(crate::devices::load(), vec![d]);
        assert_eq!(parse(&read_text().unwrap()).unwrap().code(), Some(code));
        #[cfg(unix)]
        {
            assert_eq!(mode(&file), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }

        STOP_KEEPING.store(true, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(KEEP_CHECK * 4);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// macOS 的配置目录以 bundle ID 命名，要和 tauri.conf.json 一致
    #[test]
    fn bundle_id_matches_tauri_conf() {
        let conf = include_str!("../tauri.conf.json");
        assert!(conf.contains(&format!("\"identifier\": \"{BUNDLE_ID}\"")));
    }
}
