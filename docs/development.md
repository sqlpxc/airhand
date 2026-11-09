# Development

[← Back to README](../README.md) · [简体中文](development.zh-CN.md)

## Tests

```bash
# repository root: protocol + link library + server
cargo test

# web/: web console (WebSocket protocol, password, HTTPS certificate)
cargo test

# client/: UI (jitter buffer and A/V sync simulations, H.264 conversion, UI strings)
npm test

# client/src-tauri/: client Rust (encoders incl. hardware probing, Opus packing, format conversion…)
cargo test --lib
# audio loopback capture (needs an audio output device; plays a 1.5 s test tone at -60 dBFS, barely audible)
cargo test --lib loopback_captures_tone -- --ignored --nocapture

# android/native/: Android Rust core (runs on this computer: in-flight control, keyframes, audio fan-out, command format)
cargo test
# android/: gestures (trackpad / touch / multi-touch / pass-through), keyboard, control codes, strings (every Rust problem / notice / system action has English and Chinese text)
./gradlew testDebugUnitTest -Pairhand.abis=x86_64
```

Testing the Android host doesn't need a second phone: `android/native` has a protocol-level controller that connects to the phone's code from a computer, counts frames / audio, and can tap and type:

```bash
# android/native/ (an app in the emulator reaches a server on this computer at 10.0.2.2:4433)
cargo run --example probe -- <phone's control code> 8 --tap 0.5,0.5 --text hello --audio
# multi-touch and system actions: pinch out, open quick settings (with "Ask before someone connects" on, allow it on the phone first; timing starts then)
cargo run --example probe -- <phone's control code> 6 --pinch --action QuickSettings
```

On macOS the screen / audio capture tests (`sck_capture`, `loopback_captures_tone`, video and audio in the end-to-end test) and the input injection test (`mac_move_to_center`) need the Screen & System Audio Recording and Accessibility permissions for the terminal that runs them (System Settings → Privacy & Security). Without them `sck_capture`, `loopback_captures_tone` and `mac_move_to_center` skip themselves; the end-to-end test checks that the host reports the missing permission (rather than a capture failure) and then skips the video and audio checks (clipboard, ping, disconnect and the rest are still checked).

End to end (a real server, a real host with screen capture and encoding, and a protocol-level controller):

```bash
# repository root: start a server without certificate pinning
cargo build -p airhand-server
AIRHAND_CERT=target/e2e/cert.pem AIRHAND_KEY=target/e2e/key.pem ./target/debug/airhand-server &

# client/src-tauri/
cargo test --lib e2e_roundtrip -- --ignored --nocapture   # video / codec switching / clipboard / audio / ping / disconnect
cargo test --lib e2e_reconnect -- --ignored --nocapture   # getting the code back after a server restart, changing the code
```

## Debugging tools

```bash
# client/: debug build that starts straight in controller mode; AIRHAND_LOG sets the log level
AIRHAND_DEV_CONTROLLER=1 AIRHAND_LOG=debug npm run tauri dev
# client/: A/V sync measurement (the host plays a white flash + short beep; usage at the top of the script)
node scripts/measure-avsync.mjs <control-code>

# client/src-tauri/: headless host, for testing the controller on the same machine
# (mute the controller, otherwise the played-back audio is captured again and echoes)
cargo run --release --example headless_host
# client/src-tauri/: encoder benchmark (captures the real screen and compares the encoders)
cargo run --release --example encode_bench
```

Use release builds when judging smoothness or A/V sync: debug builds scale very slowly (4K → 1080p takes about 0.5 s per frame).

## Debugging the Windows service

Debug builds run the host inside the tray process, so `npm run tauri dev` needs no installation. To work on the service itself, install the installer, or register a build directly from an administrator terminal:

```powershell
# client/src-tauri/
target\release\airhand.exe --install       # register and start the service for this airhand.exe (creates config.toml next to it)
target\release\airhand.exe --stop-service  # stop it before rebuilding: the exe is in use
target\release\airhand.exe --uninstall     # remove the service and the startup entry
```

The service and the host log with `OutputDebugString`: run Sysinternals DebugView as administrator and enable Capture → Capture Global Win32. For a different log level, add `AIRHAND_LOG=debug` to the multi-string value `Environment` under `HKLM\SYSTEM\CurrentControlSet\Services\Airhand` and restart the service. While the service is installed, a debug build started from elsewhere still runs its own in-process host (with its own `config.toml` and control code).

## Web console

```bash
# build the UI, then start the service in web/ (connects to a server on 127.0.0.1:4433, serves ../client/dist); open http://localhost:8080
(cd client && npm run build)
cd web && cargo run
# while working on the UI: npm run dev in client/ and open http://localhost:1420 (/ws is proxied to airhand-web on port 8080)
```

The WebSocket protocol between the browser and the service is described at the top of `web/src/console.rs`.

## Linux / WSL

`bash client/scripts/linux-test.sh` runs all of the tests above (unit, X11 capture and input injection, audio loopback, end to end, frontend). The X11 tests open fullscreen windows and inject input, so the script starts its own Xvfb screen and leaves your desktop alone; audio tests are skipped without PulseAudio / PipeWire. It needs `sudo apt install xvfb x11-apps`.

On Windows you can use WSL2 (tested on Ubuntu 24.04; WSLg ships with PulseAudio):

```bash
# Copy the repository to WSL's own disk first (building under /mnt/c or /mnt/d is much slower)
rsync -a --exclude target --exclude node_modules /mnt/<drive>/<path-to>/airhand/ ~/airhand/
cd ~/airhand && bash client/scripts/linux-test.sh
```

- The console works in WSLg: `AIRHAND_DEV_CONTROLLER=1 npm run tauri dev` opens it on the Windows desktop
- To run a host: `Xvfb :99 &`, then `DISPLAY=:99 cargo run --example headless_host` (WSLg's own desktop is Wayland)
- WSL2 uses NAT networking by default: Windows programs reach a server inside WSL through WSL's IP (`ip addr show eth0`), e.g. `AIRHAND_SERVER=172.30.x.x:4433`

## Conventions

### Protocol versions

There are two version numbers (`crates/airhand-proto/src/lib.rs`); the rules are in [How it works → Protocol evolution](architecture.md#protocol-evolution):

- `SIGNAL_VERSION` / `SIGNAL_MIN`: client ⇄ server (signaling, the session header, the datagram's session ID). The server accepts clients within its range; without a common version it replies `Incompatible` (with its own range) and the client tells the user whether to update Airhand or the server, then retries once a minute
- `SESSION_VERSION` / `SESSION_MIN`: host ⇄ controller, negotiated in the end-to-end handshake; the server never looks at it. Without a common version the controller shows `peerIncompatible`

Before changing the protocol, look for a compatible way; if there is one, don't bump a version:

- New message: add an enum variant with a **new number**; older versions skip it. If the peer may not know it, add a capability bit in `features` and only send it when the peer advertises the bit
- New field: only `Option` fields (or `#[cbor(default)]`), with a new number; required fields must stay forever
- New enum variants: use `crate::cbor::open_vec` in lists, `crate::cbor::or_default` for single values and `crate::cbor::opt` for optional ones (don't write `Option<index-only enum>` directly: minicbor's derive mishandles it, see the notes in `cbor.rs`)
- Numbers can't be changed or reused once released; bump the version only when there's no other way (e.g. removing a required field)

The start of the version handshake (the postcard part of `ClientHello` / `ServerHello`) and the ALPN must **never** change, otherwise different versions can't even tell each other that they differ. The `version_handshake_prefix_is_frozen` test guards this. The `airhand-proto` tests include examples of a newer peer sending new messages, fields and enum values; write one like them when adding a compatibility rule.

### One protocol, three implementations

The Rust side, the console (TypeScript) and Android (Kotlin) all speak the same formats: media packets (`crates/airhand-proto/src/media.rs`), input events and UI commands (`airhand_link::controller::Command`). `crates/airhand-link/tests/protocol.json` holds samples of each, generated by a Rust test; `npm test` (`tests/protocol.test.ts`) and `./gradlew testDebugUnitTest` (`ProtocolTest`) check their parsers and builders against the same file. After changing a format, regenerate it with `AIRHAND_UPDATE_FIXTURES=1 cargo test -p airhand-link --test protocol`, then make the other two pass.

### Messages are types, not text

Anything the user may see that crosses the network or comes from the Rust side is an enum (`ConnectError`, `Notice`, `ByeReason`, `Problem`…). The UI turns it into text in the user's language:

- Console strings live in `client/src/locales/zh.ts` (the full list of keys) and `en.ts`
- Tray strings use `tr!` in `client/src-tauri/src/tray.rs`
- `npm test` fails if a Rust `Problem` / `NoticeView` variant has no string in either language
- Android strings live in `android/app/src/main/res/values/strings.xml` (English, the default) and `values-zh/strings.xml`; `Messages.kt` maps the types to them. `./gradlew testDebugUnitTest` (`MessagesTest`) fails if a Rust `Problem` / `NoticeView` has no Android string, a string for a variant with `detail` doesn't show it, or the two languages differ in keys or placeholders
- Logs and internal errors are English

### Icon

The source is `assets/icon.svg`. After editing it, run `npx tauri icon ../assets/icon.svg -o src-tauri/icons` in `client/`, then copy `src-tauri/icons/32x32.png` to `src-tauri/icons/tray.png`.
