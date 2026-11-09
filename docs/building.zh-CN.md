# 构建

[← 返回 README](../README.zh-CN.md) · [English](building.md)

客户端会把**中转服务器的地址和证书指纹**编译进去作为默认值，所以请先[部署中转服务器](deploy.zh-CN.md#中转服务器)，记下证书指纹。

## 桌面客户端

### 依赖

| 系统 | 需要 |
|---|---|
| 所有系统 | 较新的 stable Rust（edition 2024）、Node.js 20+、CMake（编译 libopus） |
| Windows | Windows 10/11。WebView2 一般是系统自带的，没有时安装程序会自动下载安装 |
| macOS | macOS 13 及以上，Xcode 命令行工具（`xcode-select --install`）；CMake 可以用 `brew install cmake` 安装 |
| Linux | Tauri 的依赖、PulseAudio 开发库，以及控制台解码视频用的 GStreamer 插件（见下） |

Debian / Ubuntu：

```bash
sudo apt install build-essential cmake pkg-config libssl-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev libxdo-dev libpulse-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
```

### 服务器地址和证书指纹

构建时用环境变量 `AIRHAND_SERVER` 和 `AIRHAND_SERVER_FP` 设置，也可以直接修改 `client/src-tauri/src/config.rs` 里的默认值：

- 不设置 `AIRHAND_SERVER` 时连接 `127.0.0.1:4433`
- 指纹的格式不对（不是 64 位十六进制，允许用 `:` 或空格分隔）时构建会直接报错
- 它们只是默认值，装好之后 `config.toml` 里的 `server`、`server_fp` 优先（见[配置文件](usage.zh-CN.md#配置文件)）。那里的 `server_fp` 写错了会拒绝所有连接，而不是不校验

> [!NOTE]
> `AIRHAND_SERVER_FP` 留空时，客户端第一次连上服务器时记住它的证书（`config.toml` 里的 `[server_trust]`），之后证书变了就拒绝连接（首次连接时信任，控制台左上角显示“首次连接时信任”）。只有第一次连接防不了中间人，正式发布时请固定指纹。服务器有意换了证书时，删掉 `config.toml` 里的 `[server_trust]`。

### 构建

Windows（PowerShell）：

```powershell
cd client
npm install
$env:AIRHAND_SERVER    = "relay.example.com:4433"
$env:AIRHAND_SERVER_FP = "<服务器证书指纹>"
npm run tauri build
```

macOS / Linux：

```bash
cd client
npm install
export AIRHAND_SERVER=relay.example.com:4433
export AIRHAND_SERVER_FP=<服务器证书指纹>
npm run tauri build
```

生成的文件都在 `client/src-tauri/target/release/` 下：

| 系统 | 文件 |
|---|---|
| Windows | `bundle/nsis/Airhand_<版本>_x64-setup.exe`：安装程序 |
| macOS | `bundle/macos/Airhand.app`（约 8 MB）、`bundle/dmg/Airhand_<版本>_<架构>.dmg` |
| Linux | `airhand`（单个文件，约 10 MB）、`bundle/deb/Airhand_<版本>_amd64.deb`（会自动装上托盘、声音、GStreamer 依赖） |

### Windows 说明

- 安装程序为所有用户安装并注册系统服务（见 [Windows 安装版](usage.zh-CN.md#windows-安装版)），附加步骤在 `client/src-tauri/windows/hooks.nsh`
- `npm run tauri build -- --no-bundle` 只生成 `airhand.exe`，但发布版没有安装时不能运行
- C 运行时是静态链接的，不需要装 VC++ 运行库
- Media Foundation 是延迟加载的，Windows N 版（没装媒体功能包）也能运行，会改用软件编码

### macOS 说明

- 只要 .app 时用 `npm run tauri build -- --bundles app`；构建的是本机的架构（Apple 芯片是 arm64）
- 程序只做了临时签名（ad-hoc），没有经过 Apple 公证：拷到别的 Mac 上第一次打开时，在访达里右键 → 打开（或者 `xattr -dr com.apple.quarantine Airhand.app`）
- 系统按签名记住授予的权限，而临时签名每次构建都会变：重新构建后要在“系统设置 → 隐私与安全性”里把 Airhand 删掉再重新授权。长期使用的话，在 `client/src-tauri/tauri.macos.conf.json` 里把 `signingIdentity` 换成自己的开发者证书

## Android（APK）

需要：JDK 17 及以上（Android Studio 自带的就行）、Android SDK（Platform 35）和 NDK 27、Rust 的 Android 目标、[cargo-ndk](https://github.com/bbqsrc/cargo-ndk)：

```bash
# Android SDK 里的 sdkmanager（或者在 Android Studio 的 SDK Manager 里勾选）
sdkmanager "platforms;android-35" "build-tools;35.0.0" "ndk;27.2.12479018"
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
```

然后在 `android/` 里构建（Gradle 会先用 cargo-ndk 编译 `android/native` 里的 Rust 核心）。服务器地址和指纹和桌面版一样是编译进去的默认值，装好后可以在应用的“设置”里改。

Windows（PowerShell）：

```powershell
cd android
# 告诉 Gradle Android SDK 在哪里（或者设置环境变量 ANDROID_HOME）
"sdk.dir=C:/Users/<你>/AppData/Local/Android/Sdk" | Out-File -Encoding ascii local.properties
$env:AIRHAND_SERVER    = "relay.example.com:4433"
$env:AIRHAND_SERVER_FP = "<服务器证书指纹>"
./gradlew assembleRelease
```

macOS / Linux：

```bash
cd android
export ANDROID_HOME=~/Library/Android/sdk   # Linux 上一般是 ~/Android/Sdk
export AIRHAND_SERVER=relay.example.com:4433
export AIRHAND_SERVER_FP=<服务器证书指纹>
./gradlew assembleRelease
```

生成 `android/app/build/outputs/apk/release/Airhand-<版本>-release.apk`（约 12 MB，含 arm64-v8a / armeabi-v7a / x86_64）。

- 一个 APK 同时支持手机和电视（Android 7.0 及以上）；只要某一种 CPU 时用 `-Pairhand.abis=arm64-v8a` 可以编得更快、更小
- **签名**：设置了 `AIRHAND_KEYSTORE`（以及 `AIRHAND_KEYSTORE_PASSWORD`、`AIRHAND_KEY_ALIAS`、`AIRHAND_KEY_PASSWORD`）就用你的密钥签名；没设置时用本机的调试密钥签名，生成的 APK 也能直接安装。要发给别人长期使用（覆盖安装升级），请始终用同一个自己的密钥
- `./gradlew assembleDebug` 生成调试版（包名 `app.airhand.android.debug`，可以和正式版同时安装）
- 指纹写错了构建会直接报错（规则和桌面版相同）
