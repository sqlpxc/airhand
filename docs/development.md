# Development

[← Back to README](../README.md) · [简体中文](development.zh-CN.md)

## Tests

```bash
# repository root: protocol + link library + server (debug builds check the order of every server lock, see server/src/broker/lock.rs)
cargo test
# the server only uses airhand-link's wire module (default client feature off); the cargo test above always builds it with client, so check this separately
cargo test -p airhand-link --no-default-features

# crates/airhand-proto/fuzz/ covers the signaling handshake, signaling messages and session headers (nightly and cargo-fuzz)
cargo +nightly fuzz run signal    # also hello, session_header

# web/: web console (WebSocket protocol, password, HTTPS certificate)
cargo test

# client/: UI (jitter buffer and A/V sync simulations, H.264 conversion, UI strings)
npm test

# client/src-tauri/: client Rust (encoders incl. hardware probing, Opus packing, format conversion…)
# with ffmpeg installed, ltr_recovery_decodes_without_the_lost_frames also uses it to check that frames
# recovered from a long-term reference decode bit-identically to the stream without loss
cargo test --lib
# what each hardware encoder reports about long-term reference frames (for investigating new GPUs)
cargo test --lib ltr_probe -- --ignored --nocapture
# audio loopback capture (needs an audio output device; plays a 1.5 s test tone at -60 dBFS, barely audible)
cargo test --lib loopback_captures_tone -- --ignored --nocapture
# Windows: the controller's microphone into Steam Streaming Microphone (needs it on this computer), recorded back as a 1 kHz tone
cargo test --lib mic_reaches_steam_streaming_microphone -- --ignored --nocapture
# macOS 14.2+ / Linux with PipeWire and pactl: crash recovery, disconnect and uninstall during forwarding
cargo test --lib native_mic_cleanup -- --ignored --nocapture

# android/native/: Android Rust core (runs on this computer: in-flight control, keyframes, audio fan-out, command format)
cargo test
# android/: gestures (trackpad / touch / multi-touch / pass-through), keyboard, control codes, strings (every Rust problem / notice / system action has English and Chinese text)
./gradlew testDebugUnitTest -Pairhand.abis=x86_64
```

Testing the Android host doesn't need a second phone: `android/native` has a protocol-level controller that connects to the phone's code from a computer, counts frames / audio, and can tap and type. Each run is a new controller identity, so pass the pairing code from the phone's Share screen tab:

```bash
# android/native/ (an app in the emulator reaches a server on this computer at 10.0.2.2:4433)
cargo run --example probe -- <phone's control code> 8 --tap 0.5,0.5 --text hello --audio --pairing <pairing code>
# multi-touch and system actions: pinch out, open quick settings
cargo run --example probe -- <phone's control code> 6 --pinch --action QuickSettings --pairing <pairing code>
```

On macOS the screen / audio capture tests (`sck_capture`, `loopback_captures_tone`, video and audio in the end-to-end test) and the input injection test (`mac_move_to_center`) need the Screen & System Audio Recording and Accessibility permissions for the terminal that runs them (System Settings → Privacy & Security). Without them `sck_capture`, `loopback_captures_tone` and `mac_move_to_center` skip themselves; the end-to-end test checks that the host reports the missing permission (rather than a capture failure) and then skips the video and audio checks (clipboard, ping, disconnect and the rest are still checked).

End to end (a real server, a real host with screen capture and encoding, and a protocol-level controller):

```bash
# repository root: start a server without certificate pinning
cargo build -p airhand-server
AIRHAND_CERT=target/e2e/cert.pem AIRHAND_KEY=target/e2e/key.pem ./target/debug/airhand-server &

# client/src-tauri/
cargo test --lib e2e_roundtrip -- --ignored --nocapture   # video frame streams / codec switching / clipboard / audio / ping / disconnect
cargo test --lib e2e_reconnect -- --ignored --nocapture   # getting the code back after a server restart, changing the code
cargo test --lib e2e_multi_display -- --ignored --nocapture  # showing two displays at once, then one (skipped with a single display)
cargo test --lib e2e_mic -- --ignored --nocapture         # record a 1 kHz tone from the OS device (Swift on Mac, pipewire-pulse and parec on Linux, Steam driver on Windows)
```

The clipboard file tests use this computer's real clipboard and replace what's on it, so they're skipped by default too (run them one at a time):

```bash
# client/src-tauri/
cargo test --lib clipfiles -- --ignored --test-threads=1
# Windows: puts virtual files on the clipboard, then has Explorer really paste them into a temporary folder (explorer_pastes_the_virtual_files)
# Linux: needs FUSE and X11 (WSLg works); mounts, reads the files and checks text/uri-list on the clipboard
```

## Debugging tools

```bash
# client/: debug build that starts straight in controller mode; AIRHAND_LOG sets the log level
AIRHAND_DEV_CONTROLLER=1 AIRHAND_LOG=debug npm run tauri dev
# client/: A/V sync measurement (the host plays a white flash + short beep; usage at the top of the script)
node scripts/measure-avsync.mjs <control-code> [pairing-code]

# client/src-tauri/: headless host, for testing the controller on the same machine
# (mute the controller, otherwise the played-back audio is captured again and echoes)
cargo run --release --example headless_host
# client/src-tauri/: encoder benchmark (captures the real screen and compares the encoders)
cargo run --release --example encode_bench
```

Use release builds when judging smoothness or A/V sync: if the host falls back to CPU scaling, a debug build can take about 0.5 s per frame to shrink 4K to 1080p.

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

The WebSocket protocol between the console and the controller service (the same for the desktop console and the browser) is described at the top of `crates/airhand-link/src/console.rs`.

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

There are two version numbers (`crates/airhand-proto/src/lib.rs`). Each must match exactly; there are no version ranges or protocol capability bits:

- `SIGNAL_VERSION` (10): the client and relay each report one version in `ClientHello` / `ServerHello`. The relay does not process further requests if it differs.
- `SESSION_VERSION` (1): the host and controller check it in the authenticated end-to-end handshake. The relay does not see it; a mismatch prevents admission.

During v10 development the wire format can change without keeping paths for older clients. Build the relay, desktop, Android and web service from the same revision rather than mixing earlier v10 builds. See [Protocol evolution](architecture.md#protocol-evolution) for codec conventions.

Relay latency benchmark (local loopback over real QUIC, excluding media encode and decode):

```bash
cargo test --release -p airhand-server relay_video_bench -- --ignored --nocapture
```

### One protocol, three implementations

`crates/airhand-proto/tests/v10.rs` pins the current v10 wire bytes for handshakes, stream headers, control messages and media headers.

The Rust side, the console (TypeScript) and Android (Kotlin) all speak the same formats: media packets (`crates/airhand-proto/src/media.rs`), input events, UI commands and connect options (`airhand_link::controller::Command`, `ConnectOptions`). All the JSON between the UIs and Rust is camelCase: field names, enum values (`"h264"`, `"android"`, `"quickSettings"`), and a field naming the variant of tagged unions (`cmd` for commands, `type` for events and input, `kind` for problems and notices). `crates/airhand-link/tests/protocol.json` holds samples of each, generated by a Rust test; `npm test` (`tests/protocol.test.ts`) and `./gradlew testDebugUnitTest` (`ProtocolTest`) check their parsers and builders against the same file. After changing a format, regenerate it with `AIRHAND_UPDATE_FIXTURES=1 cargo test -p airhand-link --test protocol`, then make the other two pass.

`client/tests/protocol-contract.ts` contains literals generated by the same Rust test. `satisfies` checks every field against the actual TypeScript types, and `npm test` runs `tsc` first. A type annotation on `JSON.parse()` cannot replace this check because its `any` result bypasses validation.

### Messages are types, not text

Anything the user may see that crosses the network or comes from the Rust side is an enum (`ConnectError`, `Notice`, `ByeReason`, `Problem`…). The UI turns it into text in the user's language:

- Console strings live in `client/src/locales/zh.ts` (the full list of keys) and `en.ts`
- Tray strings use `tr!` in `client/src-tauri/src/tray.rs`
- `npm test` fails if a Rust `Problem` / `NoticeView` variant has no string in either language
- Android strings live in `android/app/src/main/res/values/strings.xml` (English, the default) and `values-zh/strings.xml`; `Messages.kt` maps the types to them. `./gradlew testDebugUnitTest` (`MessagesTest`) fails if a Rust `Problem` / `NoticeView` has no Android string, a string for a variant with `detail` doesn't show it, or the two languages differ in keys or placeholders
- Logs and internal errors are English

### Icon

The source is `assets/icon.svg`. After editing it, run `npx tauri icon ../assets/icon.svg -o src-tauri/icons` in `client/`, then copy `src-tauri/icons/32x32.png` to `src-tauri/icons/tray.png`.
