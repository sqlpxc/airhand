# iOS / iPadOS (iPhone, iPad)

[← Back to README](../README.md) · [简体中文](ios.zh-CN.md)

One app (`ios/`) for iPhone and iPad, iOS / iPadOS 17 or later:

| Tab | What it does |
|---|---|
| **Remote** | **Control others**: view and operate computers or phones (same features as the Android phone app) |
| **Share** | **Be viewed**: share this iPhone / iPad's screen and the sound it plays through the system screen broadcast. iOS doesn't let apps inject touches, so it's **view only** |
| **Settings** | Server, device name, control code, default quality and input mode… |

The UI follows the system language (Chinese / English), dark theme.

## Remote

- Enter the other device's **6-digit control code** on the home page — it connects as soon as all 6 digits are in (or tap “Paste” to pick the code out of text like “control code: 123 456”, without the system paste prompt). Connecting stays on the home page; you enter the remote screen once connected. Wrong code, offline and similar problems show right below the boxes
- **Pairing code**: the first connection to a device asks for the 6-digit pairing code it shows; afterwards it remembers this iPhone / iPad. If its identity changed (reinstalled, or impersonated) you pair again
- **My devices**: devices you connected to are saved — one tap to connect. Offline devices show “Offline · connects when it's back” and open automatically when they come online. Long-press to rename, copy the code, or delete (with undo)
- **Links**: `airhand://connect/123456?pairing=654321` connects with one tap (`pairing` is optional)
- The remote screen is full-screen with a single **draggable round button** (snaps to the left / right edge). It opens the menu: the remote's name, codec, frame rate, latency, bitrate, direct or relayed path and safety code at the top; then keyboard, sound, microphone, input mode, lock mouse, fit screen, rotate (iPhone), send files, gestures, home, disconnect; and pages for shortcuts, quality (presets, codec, A/V sync), clipboard, displays (multi-monitor), privacy mode (when the remote is Windows or a Mac) and game controls
- On iPhone the screen follows the remote's orientation (landscape for computers), and the menu can rotate it; iPad rotates freely

### Gestures

Two **input modes** (switch any time in the menu; the default is set in Settings — “Automatic” uses the trackpad for computers and touch for phones). The gesture help appears the first time you enter a remote screen:

| Gesture | Trackpad (computer) | Touch (computer) | Phone |
|---|---|---|---|
| One-finger slide | Move the pointer (slow = precise, fast = accelerated) | Drag with the left button | Every finger is passed through (if the remote takes multi-touch) |
| Tap | Left click, tap twice = double-click | Click there | 〃 |
| Long press | Right click | Right-click there | 〃 |
| Tap, then hold and drag | Drag with the left button | — | 〃 |
| Two-finger tap | Right click | Right click | 〃 |
| Two-finger drag | Scroll (natural, with inertia) | Scroll | 〃 |
| Pinch | Zoom, the picture follows the pointer | Zoom / move the picture | 〃 |
| Three-finger tap | Show / hide the keyboard | Same | 〃 (keyboard is in the menu) |

After zooming in, Airhand asks the remote for a sharper picture once your fingers stop (720p / 1080p / 1440p / 4K steps, up to the remote's native resolution), and goes back to the quality setting when you zoom out.

- **Keyboard**: the picture shrinks above it; an extra row has Esc, Tab, Ctrl, Alt, Shift, Win (⌘ for Macs), arrows, Home / End, PgUp / PgDn, Del, F1–F12. Modifiers are sticky: tap once for the next key, twice to lock. IME composition (pinyin…) shows above the row and is sent when committed
- **Hardware keyboard**: physical keys are sent as key down / up (hold W in a game). Shortcuts the system keeps (⌘Tab, ⌘Space, Globe) can't be passed — use the shortcuts page
- **Mouse / trackpad (iPad)**: move, left / right / middle / side buttons and the wheel map to the remote; the remote's cursor shape is drawn locally. When the remote hides its cursor (games) or “Lock mouse” is on, the pointer is **locked** and raw movement is sent; hold Esc to release (iPadOS only allows pointer lock in full screen)
- Phones get **Back / Home / Recents** below the picture (can be turned off) and their system actions on the shortcuts page; computers get shortcuts for their OS
- **Clipboard**: text copied on the remote lands in this device's clipboard; the menu can send this device's text to the remote clipboard or type it (iOS asks once to allow pasting)
- **Files**: files copied on the remote show a prompt; “Save to Files” transfers them into Files › On My iPhone › Airhand. “Send files” puts files from this device on the remote clipboard to paste there (only a list is sent until pasted)
- **Sound** with adaptive jitter buffer; **A/V sync** for videos; **microphone** forwarding to Windows, macOS 14.2+ and Linux hosts, with system echo cancellation
- **Privacy mode** (Windows and Mac hosts): blanks all of the remote monitors and disables its keyboard and mouse, so only you can see and control it. The page shows the current state (turning on, on, why it isn't available), and a failure also pops up a message. It turns off by itself after you disconnect and stays on while reconnecting. See [Usage → Privacy mode](usage.md#privacy-mode)
- A quick trip to another app keeps the session; after a longer time in the background iOS suspends the app and the session reconnects when you return (with its resume token — no pairing code needed). On the home page the picture drops to 5 fps; the card at the top returns to it

### Game controls

For computers, Menu → **Game controls** overlays buttons and sticks (same as Android): hold buttons (keys, mouse buttons, wheel, combos of up to 3 keys; tap-to-toggle; drag-to-look), WASD / arrow sticks with 8 directions, hysteresis and an edge key (Shift to sprint), floating start; dragging on empty space turns the view when the game hides the cursor. Templates: shooter, skills (MOBA / ARPG), arrow keys; per-computer on/off and layout. The editor dims the picture with a dot grid; drag to place (snaps to other buttons and the center lines with guides and a tick), pinch to resize, tap to edit, undo, add, layout settings (name, opacity, look sensitivity, haptics).

## Share

Tap **Start sharing the screen** on the Share page and choose “Start Broadcast” in the system sheet. Then:

- The **control code** (fixed until you change it) and **pairing code** are shown large, with copy and share
- A **notification** appears when a device connects (who, from which IP, safety code); “Viewing this device” lists them, with **Disconnect all**
- Pairing code, forgetting paired devices and changing the control code work like on computers and Android
- Viewers can only **watch** (iOS doesn't allow injecting touches) and hear what this device plays (never the microphone); controllers show “view only”
- Landscape screens are rotated upright; a still screen isn't encoded, with a few refinement frames for sharp text
- Sharing runs in the system **broadcast upload extension**, so it keeps going in other apps (red recording indicator in the status bar). Stop it from the Share page or the red indicator

## Data

Settings, saved devices, the end-to-end identity, control code, pairing state and game layouts live in the App Group (`group.app.airhand.ios`) container, readable only by Airhand and its broadcast extension, not backed up, deleted with the app. Received files are in the app's Documents/Airhand (visible in Files). Logs go to the system log (subsystem `app.airhand`).

## Building

On a Mac with Xcode 27+ (which needs Apple silicon and macOS 26.6 or later; the app still runs on iOS 17+), Rust with `rustup target add aarch64-apple-ios aarch64-apple-ios-sim`, and [XcodeGen](https://github.com/yonaskolb/XcodeGen):

```bash
cd ios
# IPA without a real signature (ad-hoc signed, entitlements included): build/Airhand.ipa
AIRHAND_SERVER=relay.example.com:4433 AIRHAND_SERVER_FP=<fingerprint> scripts/build-ipa.sh
# Signed with your Apple developer account (signed in to Xcode): build/export/Airhand.ipa
TEAM_ID=<team id> EXPORT_METHOD=development AIRHAND_SERVER=... AIRHAND_SERVER_FP=... scripts/build-ipa.sh
```

`EXPORT_METHOD` can also be `ad-hoc`, `app-store-connect` (TestFlight / App Store) or `enterprise`. For development, run `xcodegen generate` and open `ios/Airhand.xcodeproj`; a build phase compiles the Rust core (`ios/native`) for the current SDK with `scripts/build-rust.sh`.

iOS only runs signed apps: use your developer account, Xcode with a free Apple ID (re-install every 7 days; if the App Group capability isn't available, remote control still works but sharing doesn't), or re-sign the unsigned IPA with tools like Sideloadly / AltStore (the app and its extension must share one App Group).

## How it works

- **One network stack**: `crates/airhand-mobile` is the phone core shared by Android and iOS (controller service wrapper, the host's server connection and sessions, flow and bitrate control, clipboard files). Android wraps it with JNI in `android/native`; iOS with a C ABI in `ios/native` (`include/airhand.h`, `import AirhandCore` in Swift). Commands and events are JSON as on Android; media packets are handed over without copying; Rust never calls back into Swift — Swift threads `poll`
- **Controller** logic mirrors Android (`RemoteSession.swift` ↔ `RemoteSession.kt`): reconnects, waiting for offline hosts, revoked tokens, background frame rate, sharper picture when zoomed
- **Video**: Annex-B → length-prefixed, parameter sets → format description (av1C built from the AV1 sequence header), `VTDecompressionSession` in real-time mode, decoded frames marked display-immediately on an `AVSampleBufferDisplayLayer`. Acknowledged when submitted to the decoder (in the bounded wait queue with A/V sync). Codecs follow what this device decodes in hardware (AV1 > HEVC > H.264)
- **Audio**: Opus via AudioToolbox, the same jitter buffer / drift / A/V sync algorithm as Android and the web console; one `AVAudioEngine` for playback and microphone (needed for echo cancellation)
- **Input**: gestures (`Gestures.swift` = `Gestures.kt`), soft keyboard (hidden text field, waits for IME composition), hardware keys (HID usage → `KeyboardEvent.code`), mouse (`GCMouse` raw deltas, hover position, pointer lock), game controls — all become protocol input events, coalesced every 8 ms
- **Host** runs in the broadcast extension: `Shared/HostRunner.swift` rotates ReplayKit frames upright, scales to the requested short side, encodes with VideoToolbox (H.264 / HEVC, low latency, no B-frames, keyframes only on demand) and encodes app audio as 20 ms Opus packets; flow control, bitrate and keyframes are decided by Rust's `VideoHub` (same as the Android host). The extension memory limit (~50 MB) keeps buffers minimal
- **App ↔ extension**: shared settings / identity / code / pairing files in the App Group; live state in `host-live.json`, commands in `host-command.json`, each side nudged with Darwin notifications
- The host reports the new protocol platform `ios`; desktop, web and Android controllers hide mouse modes and show “view only”

## Testing

```bash
cargo test                       # repository root: protocol, link, mobile core, server
cd ios && xcodegen generate && xcodebuild -project Airhand.xcodeproj -scheme Airhand -sdk iphonesimulator \
  -destination 'platform=iOS Simulator,name=iPhone 17' test   # Swift unit tests incl. the shared protocol fixture
```

End to end without devices: run a server and the simulated host (`ios/native`: `cargo run --release --example fake_host`, a color test pattern encoded with OpenH264 through the same network code as an iPhone host), then `ios/scripts/sim-run.sh -AirhandConnect <code> -AirhandPairing <pairing>`. Debug builds also accept `-AirhandSelfTest 1` (scripted gestures and typing, check the inputs `fake_host` prints), `-AirhandNoTips 1`, `-AirhandOpenMenu 1`, `-AirhandKeyboard 1`, `-AirhandGame 1`, `-AirhandEditGame 1`, `-AirhandTab share|settings`.

The simulator has no screen broadcast: `-AirhandHostSelfTest 1` runs the same `HostRunner` with a synthetic picture and a 440 Hz tone; connect with the protocol-level controller in `android/native` (`cargo run --release --example probe -- <code> 8 --audio --pairing <pairing> --server 127.0.0.1:4433`) or with a second simulator.

## Known limitations

- **iPhone / iPad can only be viewed**, not controlled: iOS has no API for injecting touches or keys
- Sharing must be started by the user (“Start Broadcast”, a system rule)
- After a while in the background iOS suspends the app; remote sessions reconnect on return (sharing is unaffected)
- The broadcast extension's ~50 MB memory limit; high-resolution iPads are scaled to the requested short side before encoding
- AV1 hardware decoding needs A17 Pro / M3 or later; the simulator only has H.264
- Pointer lock only on iPad in full screen
- ⌘Tab, ⌘Space, Globe and similar shortcuts are kept by the system
- Reading this device's clipboard (Menu → Clipboard) shows iOS's paste prompt once
- Direct LAN connections need the “Local Network” permission; without it everything goes through the relay (or public IPv6)
- On IPv6-only networks (NAT64), use a host name for the server: iOS doesn't synthesize NAT64 addresses for IPv4 literals
