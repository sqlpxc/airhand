# Known limitations

[← Back to README](../README.md) · [简体中文](limitations.zh-CN.md)

## General

- The clipboard syncs text only, not files
- Audio flows from host to controller only; the controller's microphone isn't forwarded. Audio is captured after the volume control, so a muted host sends silence
- Sessions are end-to-end encrypted, so the relay can't see their content, but it still knows who connected to whom, when, and how much traffic flowed (see [Security](security.md#end-to-end-encryption))
- The web console is where end-to-end encryption ends: the machine running it can see the picture
- On the first connection to a computer its identity is whatever was seen then (like SSH): only the pairing code (or comparing the safety code) proves nobody is impersonating it

## Windows

- The host only runs in the console session (the physical screen): Remote Desktop (RDP) sessions can't be controlled
- `Ctrl+Alt+Del` can't be sent yet (it needs `SendSAS`)
- Debug builds run the host in the tray process, without SYSTEM rights, so they can't capture or control UAC prompts and the lock screen (the controller shows a capture error meanwhile)

## macOS

- The host needs the Screen & System Audio Recording and Accessibility permissions, which only the local user can grant in System Settings; since macOS 15 the system also asks again from time to time whether to keep allowing screen recording
- The login window and the lock screen can't be captured or controlled
- The host doesn't offer AV1 (current Apple chips can only decode it in hardware), so AV1 shows as unavailable in the console and HEVC is used instead
- The controller (WKWebView) can't lock the keyboard: in fullscreen, Command+Tab, Command+Space and the like are still handled locally, so use "Send shortcut" in the toolbar
- WKWebView on older macOS versions may lack Opus decoding, in which case the console says remote audio can't be played

## Linux

- The host only supports **X11** desktops. Under Wayland only X11 (XWayland) applications can be seen and controlled; native Wayland windows are black and the controller suggests logging in with an X11 (Xorg) session. Full Wayland support needs xdg-desktop-portal screen sharing (the local user has to confirm each time) and isn't implemented
- The host only has OpenH264 software encoding (no VA-API / NVENC yet), so the controller uses H.264
- The lock screen can't be controlled (the screensaver grabs the keyboard and mouse)
- Decoding on the controller relies on WebKitGTK + GStreamer: H.264 needs `gstreamer1.0-plugins-bad` (parsing) and `gstreamer1.0-libav` or openh264 (decoding); AV1 / HEVC depend on the installed GStreamer decoders. If a format turns out not to produce a picture, the controller switches to the next available format by priority

## Web console

- Reloading or closing the tab disconnects its sessions; My devices isn't saved and is cleared when the page reloads
- In Firefox, text copied locally isn't synced to the remote side automatically
- Outside fullscreen some shortcuts (Ctrl+W…) can't reach the remote computer

## Android

Host (sharing the phone):

- No pairing code: someone new is either asked about (with the safety code) or let in on the control code alone, if "Ask before someone connects" is off. Allowed controllers are only remembered until sharing stops
- Each time sharing starts, screen recording must be confirmed on the phone (a system rule), and sharing can't resume by itself after the system kills the app
- Remote control uses accessibility gestures, so apps that handle touches themselves (games…) may react less precisely
- Typing only works in regular text fields (arrow keys and Tab need Android 13+)
- A secured lock screen and screens marked secure are black
- Since Android 10 apps can't read the clipboard in the background, so the clipboard only syncs from the controller to the phone
- Sharing sound needs Android 10+, and some apps don't allow their sound to be captured
- HEVC and AV1 are only offered when the phone has a **hardware** encoder for them (few phones have an AV1 hardware encoder so far), otherwise H.264 is used (with the system's software encoder if there's no hardware H.264 encoder)

Controller:

- One device at a time; no A/V sync; TVs only watch
- HEVC and AV1 are only used when the device has a **hardware** decoder for them, otherwise H.264

AV1 (encoding and decoding) hasn't been verified on a real device yet: the emulator has no AV1 hardware codecs.
