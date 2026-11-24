# 开发

[← 返回 README](../README.zh-CN.md) · [English](development.md)

## 测试

```bash
# 仓库根目录：协议 + 链路库 + 服务器（调试构建里服务器的每次加锁都检查先后，见 server/src/broker/lock.rs）
cargo test
# 服务器只用 airhand-link 的 wire（关掉了默认的 client）：上面的 cargo test 总是带着 client 编译它，这里单独检查一下
cargo test -p airhand-link --no-default-features

# crates/airhand-proto/fuzz/：信令握手、信令消息和会话头；
# 需要 nightly 和 cargo-fuzz
cargo +nightly fuzz run signal    # 还有 hello、session_header

# web/：网页控制台（WebSocket 协议、访问密码、HTTPS 证书）
cargo test

# client/：界面（抖动缓冲和音画同步仿真、H.264 格式转换、界面文案）
npm test

# client/src-tauri/：客户端 Rust 部分（编码器含硬件探测、Opus 打包、格式转换等）
# 装了 ffmpeg 的话，ltr_recovery_decodes_without_the_lost_frames 还会用它检查长期参考帧恢复解出来的画面和不丢帧时逐位相同
cargo test --lib
# 每个硬件编码器的长期参考帧报告了什么（调查新显卡用）
cargo test --lib ltr_probe -- --ignored --nocapture
# 声音环回采集（需要声音输出设备，会以 -60 dBFS 播放 1.5 秒测试音，几乎听不见）
cargo test --lib loopback_captures_tone -- --ignored --nocapture
# Windows：控制端的麦克风写进 Steam Streaming Microphone（这台电脑上要有它），从它的录音端录回 1 kHz 的声音
cargo test --lib mic_reaches_steam_streaming_microphone -- --ignored --nocapture
# macOS 14.2+ / Linux（PipeWire、pactl）：检查强退后的清理、断开和转发期间卸载
cargo test --lib native_mic_cleanup -- --ignored --nocapture
# Windows：列出现在的显示器、哪些是虚拟显示器、本机主显示器的分辨率（只读，不改显示设置）
cargo test --lib list_displays -- --ignored --nocapture
# macOS 防窥：主显示器调黑 1 秒，读回来是黑的、恢复之后不是；调黑时采集到的画面不变（需要屏幕录制权限）。本机屏幕会黑一下
cargo test --lib blanks_and_restores -- --ignored --nocapture
cargo test --lib gamma_is_not_captured -- --ignored --nocapture

# 手机核心（Android 和 iOS 共用，crates/airhand-mobile：在途控制、关键帧、声音分发、命令格式）在仓库根目录的 cargo test 里
# ios/：Swift 单元测试（协议样例、Annex-B、AV1、Opus、抖动缓冲、手势），以及模拟器里的端到端测试，见 docs/ios.zh-CN.md 的“测试”
xcodebuild -project Airhand.xcodeproj -scheme Airhand -sdk iphonesimulator -destination 'platform=iOS Simulator,name=iPhone 17' test
# android/：手势（触控板 / 触屏 / 多点触摸 / 直通）、键盘、控制码、文案（Rust 的每种问题 / 提示 / 系统操作都有中英文文案）
./gradlew testDebugUnitTest -Pairhand.abis=x86_64
```

测试 Android 被控端不需要第二台手机：`android/native` 里有一个协议级的控制端，在电脑上连上手机的控制码，统计画面 / 声音，还可以点一下、打字。每次运行都是新的控制端身份，所以要带上手机“共享本机”页里的配对码：

```bash
# android/native/（模拟器里的应用把服务器设为 10.0.2.2:4433 就能连到本机的服务器）
cargo run --example probe -- <手机的控制码> 8 --tap 0.5,0.5 --text hello --audio --pairing <配对码>
# 多点触摸和系统操作：双指张开、打开快捷设置
cargo run --example probe -- <手机的控制码> 6 --pinch --action QuickSettings --pairing <配对码>
```

macOS 上，屏幕 / 声音采集（`sck_capture`、`loopback_captures_tone`、端到端测试里的画面和声音）和键鼠注入（`mac_move_to_center`）的测试需要先在“系统设置 → 隐私与安全性”里给运行测试的终端授予“屏幕与系统录音”和“辅助功能”权限。没有权限时 `sck_capture`、`loopback_captures_tone`、`mac_move_to_center` 会直接跳过；端到端测试会检查被控端发来的是“缺少权限”的提示而不是“采集失败”，然后跳过画面和声音的检查（剪贴板、Ping、断开等其余部分照常检查）。

端到端测试（真实服务器、带屏幕采集和编码的真实被控端、协议级控制端）：

```bash
# 仓库根目录：启动一个不固定证书的服务器
cargo build -p airhand-server
AIRHAND_CERT=target/e2e/cert.pem AIRHAND_KEY=target/e2e/key.pem ./target/debug/airhand-server &

# client/src-tauri/
cargo test --lib e2e_roundtrip -- --ignored --nocapture   # 视频帧流/ 编码切换 / 剪贴板 / 声音 / Ping / 断开
cargo test --lib e2e_reconnect -- --ignored --nocapture   # 服务器重启后取回控制码、更换控制码
cargo test --lib e2e_multi_display -- --ignored --nocapture  # 同时看两个显示器、再改回只看一个（只有一个显示器时跳过）
cargo test --lib e2e_mic -- --ignored --nocapture         # 从录音设备录回 1 kHz。Mac 用系统 Swift 工具，Linux 用 pipewire-pulse 和 parec，Windows 需要 Steam 驱动
```

剪贴板里的文件用的是本机真的剪贴板，测试会换掉剪贴板里的东西，所以也默认跳过（一个一个跑）：

```bash
# client/src-tauri/
cargo test --lib clipfiles -- --ignored --test-threads=1
# Windows：虚拟文件放进剪贴板，再让资源管理器真的粘贴到一个临时目录（explorer_pastes_the_virtual_files）
# Linux：要 FUSE 和 X11（WSLg 也行），挂载之后读文件、看剪贴板里的 text/uri-list
```

## 调试工具

```bash
# client/：直接进入控制端的调试构建；AIRHAND_LOG 控制日志级别
AIRHAND_DEV_CONTROLLER=1 AIRHAND_LOG=debug npm run tauri dev
# client/：音画同步实测（被控端播放闪白 + 短音，用法见脚本开头）
node scripts/measure-avsync.mjs <控制码> [配对码]

# client/src-tauri/：无界面被控端，用来在同一台机器上调试控制端
#（请在控制端静音，否则播放出的声音会被再次采集，形成回声）
cargo run --release --example headless_host
# client/src-tauri/：编码性能测试（抓真实屏幕，对比各编码器）
cargo run --release --example encode_bench
```

测画面流畅度或音画同步时请用 release 构建：被控端回退到 CPU 缩放时，调试构建从 4K 缩到 1080p 每帧可能要约 0.5 秒。

## 调试 Windows 服务

调试构建在托盘进程里运行被控端，`npm run tauri dev` 不需要安装。要调试服务本身，可以装上安装程序，或者在管理员终端里直接注册某个构建：

```powershell
# client/src-tauri/
target\release\airhand.exe --install       # 为这个 airhand.exe 注册并启动服务（在它旁边生成 config.toml）
target\release\airhand.exe --stop          # 重新构建之前先停止服务和托盘：exe 正在使用
target\release\airhand.exe --uninstall     # 删除服务和开机启动项
```

服务和被控端的日志用 `OutputDebugString` 输出：以管理员身份运行 Sysinternals 的 DebugView，勾选 Capture → Capture Global Win32。要改日志级别，在 `HKLM\SYSTEM\CurrentControlSet\Services\Airhand` 下新建多字符串值 `Environment`，写入 `AIRHAND_LOG=debug`，然后重启服务。服务装着的时候，从别处启动的调试构建仍然在自己的进程里运行被控端（有自己的 `config.toml` 和控制码）。

## 网页控制台

```bash
# 先构建界面，再在 web/ 里启动服务（默认连接 127.0.0.1:4433 的服务器，网页取自 ../client/dist），浏览器打开 http://localhost:8080
(cd client && npm run build)
cd web && cargo run
# 修改界面时：在 client/ 里 npm run dev，浏览器打开 http://localhost:1420（/ws 会转给 8080 端口上的 airhand-web）
```

`crates/airhand-link/src/console.rs` 开头写了控制台和控制端服务之间的 WebSocket 协议（桌面版和浏览器一样）。

## Linux / WSL

`bash client/scripts/linux-test.sh` 一次跑完上面所有测试（单元、X11 采集与输入注入、声音环回、端到端、前端）。X11 相关测试会开全屏窗口、注入键鼠，所以脚本自己开一个 Xvfb 虚拟屏幕，不影响当前桌面；没有 PulseAudio / PipeWire 时跳过声音测试。需要 `sudo apt install xvfb x11-apps`。

在 Windows 上可以用 WSL2（Ubuntu 24.04 测过，WSLg 自带 PulseAudio）：

```bash
# 先把仓库复制到 WSL 自己的磁盘上（在 /mnt/c、/mnt/d 下编译慢很多）
rsync -a --exclude target --exclude node_modules /mnt/<盘符>/<仓库路径>/airhand/ ~/airhand/
cd ~/airhand && bash client/scripts/linux-test.sh
```

- 控制台可以在 WSLg 里运行：`AIRHAND_DEV_CONTROLLER=1 npm run tauri dev`，窗口出现在 Windows 桌面上
- 运行被控端：`Xvfb :99 &`，然后 `DISPLAY=:99 cargo run --example headless_host`（WSLg 自己的桌面是 Wayland）
- WSL2 默认是 NAT 网络：Windows 上的程序要通过 WSL 的 IP（`ip addr show eth0`）连 WSL 里的服务器，例如 `AIRHAND_SERVER=172.30.x.x:4433`

## 约定

### 协议版本

有两个版本号（`crates/airhand-proto/src/lib.rs`），分别精确校验，不做版本区间或协议能力位协商：

- `SIGNAL_VERSION`（10）：客户端和中转交换 `ClientHello` / `ServerHello`，各报一个版本。不同就不处理后续请求。
- `SESSION_VERSION`（1）：被控端和控制端在端到端握手里校验，中转不看。不匹配就不建立会话。

v10 开发阶段可以调整线上格式，不保留旧客户端分支。测试或部署时，请从同一版源码更新中转、桌面、Android 和网页服务，不要混用早期 v10 构建。编解码约定见[协议的演进](architecture.zh-CN.md#协议的演进)。

中转延迟基准（本机回环，真实 QUIC，不包含编解码）：

```bash
cargo test --release -p airhand-server relay_video_bench -- --ignored --nocapture
```

### 一份协议，四份实现

`crates/airhand-proto/tests/v10.rs` 固定当前 v10 的握手、流头、控制消息和媒体包头的线上字节。

Rust、控制台（TypeScript）、Android（Kotlin）和 iOS（Swift）用同一套格式：媒体包（`crates/airhand-proto/src/media.rs`）、输入事件、界面命令、连接选项（`airhand_link::controller::Command`、`ConnectOptions`）。界面和 Rust 之间的 JSON 全都是 camelCase：字段名、枚举值（`"h264"`、`"android"`、`"quickSettings"`），带字段的联合类型用一个字段标明是哪一种（命令 `cmd`、事件和输入 `type`、问题和提示 `kind`）。`crates/airhand-link/tests/protocol.json` 是 Rust 测试生成的样例；`npm test`（`tests/protocol.test.ts`）、`./gradlew testDebugUnitTest`（`ProtocolTest`）和 iOS 的 `xcodebuild test`（`ios/Tests/ProtocolTests.swift`）用同一个文件检查各自的解析和拼装。改了格式之后用 `AIRHAND_UPDATE_FIXTURES=1 cargo test -p airhand-link --test protocol` 重新生成，再让另外三边的测试通过。

`client/tests/protocol-contract.ts` 由同一 Rust 测试生成字面量，并用 `satisfies` 和正式的 TypeScript 类型检查每个字段；`npm test` 会先运行 `tsc`。它不能替换成 `JSON.parse()` 后的类型标注，因为 `any` 会绕过检查。

### 消息是类型，不是文字

凡是经过网络、或者从 Rust 端传给界面、可能显示给用户的内容，都是枚举（`ConnectError`、`Notice`、`ByeReason`、`Problem` 等），由界面按用户的语言变成文字：

- 控制台的文案在 `client/src/locales/zh.ts`（全部词条以它为准）和 `en.ts`
- 托盘的文案用 `tray.rs` 里的 `tr!`（`client/src-tauri/src/tray.rs`）
- 某个 Rust 的 `Problem` / `NoticeView` 在任一语言里没有文案时，`npm test` 会失败
- Android 的文案在 `android/app/src/main/res/values/strings.xml`（英文，默认）和 `values-zh/strings.xml`，类型和文案的对应关系在 `Messages.kt`。Rust 的 `Problem` / `NoticeView` 在 Android 上没有文案、带 `detail` 的文案没有显示它、或者两种语言的词条和参数不一致时，`./gradlew testDebugUnitTest` 会失败（`MessagesTest`）
- 日志和内部错误一律用英文

### 图标

源文件是 `assets/icon.svg`。修改后在 `client/` 下运行 `npx tauri icon ../assets/icon.svg -o src-tauri/icons`，然后把 `src-tauri/icons/32x32.png` 复制为 `src-tauri/icons/tray.png`。
