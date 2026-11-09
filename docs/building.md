# Building

[← Back to README](../README.md) · [简体中文](building.zh-CN.md)

The clients have the **relay server's address and certificate fingerprint** compiled in as defaults, so [deploy the relay server](deploy.md#relay-server) first and note its certificate fingerprint.

## Desktop client

### Requirements

| System | Needs |
|---|---|
| All | A recent stable Rust (edition 2024), Node.js 20+, CMake (to build libopus) |
| Windows | Windows 10/11. WebView2 usually ships with the OS; if it's missing the installer downloads and installs it |
| macOS | macOS 13 or later and the Xcode Command Line Tools (`xcode-select --install`); CMake can come from `brew install cmake` |
| Linux | Tauri's dependencies, the PulseAudio development files and the GStreamer plugins the console uses to decode video (see below) |

Debian / Ubuntu:

```bash
sudo apt install build-essential cmake pkg-config libssl-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev libxdo-dev libpulse-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav
```

### Server address and certificate fingerprint

Set them at build time with the `AIRHAND_SERVER` and `AIRHAND_SERVER_FP` environment variables, or change the defaults in `client/src-tauri/src/config.rs`:

- Without `AIRHAND_SERVER` the client connects to `127.0.0.1:4433`
- A malformed fingerprint (it must be 64 hex digits; `:` and spaces are allowed) fails the build
- These are only defaults: once installed, `server` and `server_fp` in `config.toml` override them (see [Configuration file](usage.md#configuration-file)). An invalid `server_fp` there refuses every connection rather than turning verification off

> [!NOTE]
> When `AIRHAND_SERVER_FP` is empty the client remembers the server's certificate on its first connection (`[server_trust]` in `config.toml`) and refuses a different one later (trust on first use; the console shows "Trust on first use" at the top left). Only that first connection is unprotected against a man in the middle, so pin the fingerprint for releases. If the server's certificate changes on purpose, delete `[server_trust]` from `config.toml`.

### Build

Windows (PowerShell):

```powershell
cd client
npm install
$env:AIRHAND_SERVER    = "relay.example.com:4433"
$env:AIRHAND_SERVER_FP = "<server certificate fingerprint>"
npm run tauri build
```

macOS / Linux:

```bash
cd client
npm install
export AIRHAND_SERVER=relay.example.com:4433
export AIRHAND_SERVER_FP=<server certificate fingerprint>
npm run tauri build
```

The output is under `client/src-tauri/target/release/`:

| System | Files |
|---|---|
| Windows | `bundle/nsis/Airhand_<version>_x64-setup.exe`: the installer |
| macOS | `bundle/macos/Airhand.app` (~8 MB), `bundle/dmg/Airhand_<version>_<arch>.dmg` |
| Linux | `airhand` (single file, ~10 MB), `bundle/deb/Airhand_<version>_amd64.deb` (pulls in the tray, audio and GStreamer dependencies) |

### Windows notes

- The installer installs for all users and registers the system service (see [Windows installation](usage.md#windows-installation)); its extra steps are in `client/src-tauri/windows/hooks.nsh`
- `npm run tauri build -- --no-bundle` only builds `airhand.exe`, but a release build refuses to run until installed
- The C runtime is statically linked, so the VC++ redistributable isn't needed
- Media Foundation is delay-loaded, so Windows N editions (without the Media Feature Pack) still run and use software encoding

### macOS notes

- For just the .app use `npm run tauri build -- --bundles app`; it is built for the machine's own architecture (arm64 on Apple silicon)
- The app is only ad-hoc signed, not notarized: the first time you open a copy on another Mac, right-click it in Finder → Open (or run `xattr -dr com.apple.quarantine Airhand.app`)
- macOS remembers granted permissions by code signature, and an ad-hoc signature changes with every build: after rebuilding, remove Airhand in System Settings → Privacy & Security and grant the permissions again. For long-term use, put your own Developer ID in `signingIdentity` in `client/src-tauri/tauri.macos.conf.json`

## Android (APK)

Requirements: JDK 17 or later (the one bundled with Android Studio works), the Android SDK (platform 35) and NDK 27, Rust's Android targets and [cargo-ndk](https://github.com/bbqsrc/cargo-ndk):

```bash
# sdkmanager from the Android SDK (or tick them in Android Studio's SDK Manager)
sdkmanager "platforms;android-35" "build-tools;35.0.0" "ndk;27.2.12479018"
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
```

Then build in `android/` (Gradle first builds the Rust core in `android/native` with cargo-ndk). As on the desktop, the server address and fingerprint are compiled-in defaults that can be changed later in the app's Settings.

Windows (PowerShell):

```powershell
cd android
# tell Gradle where the Android SDK is (or set ANDROID_HOME)
"sdk.dir=C:/Users/<you>/AppData/Local/Android/Sdk" | Out-File -Encoding ascii local.properties
$env:AIRHAND_SERVER    = "relay.example.com:4433"
$env:AIRHAND_SERVER_FP = "<server certificate fingerprint>"
./gradlew assembleRelease
```

macOS / Linux:

```bash
cd android
export ANDROID_HOME=~/Library/Android/sdk   # usually ~/Android/Sdk on Linux
export AIRHAND_SERVER=relay.example.com:4433
export AIRHAND_SERVER_FP=<server certificate fingerprint>
./gradlew assembleRelease
```

This produces `android/app/build/outputs/apk/release/Airhand-<version>-release.apk` (~12 MB, arm64-v8a / armeabi-v7a / x86_64).

- One APK for phones and TVs (Android 7.0 or later); `-Pairhand.abis=arm64-v8a` builds just one CPU type, faster and smaller
- **Signing**: with `AIRHAND_KEYSTORE` (plus `AIRHAND_KEYSTORE_PASSWORD`, `AIRHAND_KEY_ALIAS`, `AIRHAND_KEY_PASSWORD`) set, the APK is signed with your key; otherwise with this machine's debug key, which still installs fine. For APKs you hand out and update over time, always use the same key of your own
- `./gradlew assembleDebug` builds a debug version (package `app.airhand.android.debug`, installable next to the release)
- A malformed fingerprint fails the build (same rule as the desktop)
