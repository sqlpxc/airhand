# Known limitations

[← Back to README](../README.md) · [简体中文](limitations.zh-CN.md)

## General

- Clipboard file transfer (see [How it works → Files on the clipboard](architecture.md#files-on-the-clipboard)): when only the relay is available, one paste can carry at most what the server allows (`AIRHAND_MAX_FILE_SIZE`, 50 MiB by default); there's no limit while the direct path is up. Files changed after they were copied can't be fetched (size or modification time no longer match): copy them again. Links inside a folder are only recreated when they are relative and stay inside the folder, and only Linux recreates them when receiving; other platforms skip them. There's no overall progress: the progress bar is the receiving file manager's own
- Audio is captured after the host's volume control, so a muted host sends silence
- Microphone forwarding requires Steam Streaming Microphone on Windows (already installed or available from the host’s copy of Steam), macOS 14.2+, or a running user PipeWire server (0.3.49+) on Linux. Android hosts and the native Android controller do not yet support it.
- Sessions are end-to-end encrypted, so the relay can't see their content, but it still knows who connected to whom, when, and how much traffic flowed, and with one stream per video frame also how large each frame is (see [Security](security.md#end-to-end-encryption))
- The web console is where end-to-end encryption ends: the machine running it can see the picture
- On the first connection to a computer its identity is whatever was seen then (like SSH): only the pairing code (or comparing the safety code) proves nobody is impersonating it
- The direct path relies on hole punching only: when both ends are behind symmetric NATs (common with some carriers' carrier-grade NAT and on corporate networks) and don't both have public IPv6, or a firewall blocks UDP, it doesn't work and the session stays on the relay; there's no UPnP / NAT-PMP port mapping. When an established direct path suddenly breaks (for example the host switches networks), the video stalls for about a second (until the host notices, moves back to the relay and encodes a keyframe) before the direct path is tried again
- While the direct path is up, the control lane is sent on both paths (the relay always has a complete copy) and the bulk lane (clipboard, cursor images) only over the relay: the relay carries much less traffic without video and audio, but not none
- Recovering a lost frame from a long-term reference frame (instead of a keyframe) currently only works with Windows hardware encoders: H.264 (NVIDIA, Intel) and NVIDIA AV1. HEVC, macOS (VideoToolbox), Linux and software encoding (OpenH264) and Android (MediaCodec) send a keyframe (see [How it works → Frame streams and reference invalidation](architecture.md#frame-streams-and-reference-invalidation))
- One stream per video frame also needs a v10 relay server: older servers still work, but video stays a single stream, so one lost packet holds up the frames behind it
- Showing several displays at once (see [Usage → Multiple displays](usage.md#multiple-displays)) is limited to 4; each display is captured and encoded on its own, so the host's encoding load and the bandwidth roughly scale with the number of displays (the quality settings apply to each one). When the GPU can't run that many hardware encoders at once, the extra streams fall back to software encoding. A/V sync delays all of them together. The Android host has a single screen, and the Android controller shows one remote display at a time
- Later frame streams can arrive first. Frames that depend on a missing reference still wait for retransmission or recovery, so long, lossy paths can stutter.

## Windows

- The host only runs in the console session (the physical screen): Remote Desktop (RDP) sessions can't be controlled
- `Ctrl+Alt+Del` can't be sent yet (it needs `SendSAS`)
- When pasting a folder copied on the other side, paths longer than 259 characters counted from that folder (a limit of Windows virtual files) and links inside it are skipped (logged, but not shown in the UI). When the host runs in the system service, copying and pasting files goes through the tray (which reads and writes as the signed-in user): if the tray has exited, only text is synced
- Debug builds run the host in the tray process, without SYSTEM rights, so they can't capture or control UAC prompts and the lock screen (the controller shows a capture error meanwhile)
- The [virtual display](usage.md#virtual-display) (control with the monitor off, privacy mode) is only available when installed for all users with the bundled virtual display driver (MttVDD). The driver is signed by SignPath Foundation, not Microsoft: Windows asks once during installation, and Windows 11 on ARM64 only accepts Microsoft-signed drivers, so it can't be installed there. If another program (Sunshine, for example) already installed a Virtual Display Driver on the computer, Airhand doesn't use it
- Switching to the virtual display and back means enabling / disabling the device and changing display settings, so the picture pauses for a few seconds; windows move to the virtual display, and when switching back Windows puts the monitors back as it has them saved, so windows may not end up exactly where they were. The virtual display doesn't support HDR
- Privacy mode can't block Ctrl+Alt+Del at the remote computer (it only switches to the secure desktop, which doesn't show on its monitors either), nor sleep from closing the lid or pressing the power button; the remote computer's sound still plays from its own speakers (that's what the host captures). While the controller is still connected, people at the remote computer can only end it by disconnecting it from the network (and pressing a key once the session has dropped) or restarting it
- Keeping the screen on and the computer awake during sessions relies on power requests: sleep the user asks for (closing the lid, the power button) isn't held off, and on Modern Standby computers running on battery, Windows ends these requests 5 minutes after the sleep timeout
- The controller's microphone is written into Steam Streaming Microphone in exclusive mode when its two devices ("Speakers" and "Microphone" in the sound settings) have different default formats (the usual case): while a controller's microphone is on, Steam Remote Play can't use it and vice versa, and it can't be opened if "Allow applications to take exclusive control of this device" is turned off (giving both devices the same default format avoids exclusive mode). It isn't made the default recording device automatically: choose it in the program, or set it in the sound settings yourself

## macOS

- When recording from **Airhand Microphone** in Safari on the Mac, turn off **echo cancellation in the website’s microphone settings**. On Safari 27.2, the voice-processing audio unit fails to open this virtual input. Web apps can request the selected `deviceId` with `echoCancellation: { exact: false }` in `getUserMedia`.
- The host needs the Screen & System Audio Recording and Accessibility permissions, which only the local user can grant in System Settings; since macOS 15 the system also asks again from time to time whether to keep allowing screen recording
- The login window (after a restart, before anyone logs in) can't be captured or controlled. The lock screen can: if the display is off, it's woken up when a controller connects
- A Mac that is asleep is offline and can't be reached. **Prevent automatic sleeping** (see "Lock screen and sleep" in [Usage → Host](usage.md#host-default)) only applies while Airhand runs and the Mac is on a power adapter (then a closed lid doesn't put it to sleep either); it can't stop Sleep from the Apple menu, a closed lid on battery or a low battery, and Airhand can't wake a Mac that has gone to sleep remotely. Closing the lid works only on a power adapter, a closed lid needs the virtual display to have been there before it closed (during remote control, or all the time with **Prevent automatic sleeping**), and the virtual display that fills in for it uses a private macOS interface (`CGVirtualDisplay`, the same one DeskPad and BetterDisplay use): if a future macOS changes it, there is no virtual display and the controller sees "no displays" until the lid is opened
- Privacy mode on a Mac turns the displays fully black through their gamma tables, so the backlight stays on; macOS puts the gamma tables back when displays are plugged in or removed and on wake, and Airhand blanks them again within 0.1 seconds, so they may flash briefly. macOS 26 on the M5 Pro / M5 Max and the MacBook Neo accepts a gamma table and then doesn't show it (an Apple bug), and AirPlay, Sidecar (iPad) and DisplayLink displays have no gamma table: those displays are only covered by the black window on top of everything, which doesn't cover the mouse pointer and may not cover the lock screen. It can't stop typing into a focused password field at the Mac (macOS's "secure input" keeps those keystrokes away from Airhand), the power button or closing the lid; it blocks input from the keyboard, mouse and trackpad, not keyboard and mouse events that other programs on the Mac synthesize. The Mac's sound still plays from its own speakers
- The host doesn't offer AV1 (current Apple chips can only decode it in hardware), so AV1 shows as unavailable in the console and HEVC is used instead
- The controller (WKWebView) can't lock the keyboard: in fullscreen, Command+Tab, Command+Space and the like are still handled locally, so use "Send shortcut" in the toolbar
- WKWebView on older macOS versions may lack Opus decoding, in which case the console says remote audio can't be played
- On a Mac, use **Airhand → Receive files to…** in the menu bar to receive files and folders copied on the other device. Paste in Finder uses the Mac’s own clipboard.

## Linux

- The host only supports **X11** desktops. Under Wayland only X11 (XWayland) applications can be seen and controlled; native Wayland windows are black and the controller suggests logging in with an X11 (Xorg) session. Full Wayland support needs xdg-desktop-portal screen sharing (the local user has to confirm each time) and isn't implemented
- The host only has OpenH264 software encoding (no VA-API / NVENC yet), so the controller uses H.264
- The lock screen can't be controlled (the screensaver grabs the keyboard and mouse)
- Receiving files copied on the other side needs FUSE (`fusermount3` and `/dev/fuse`): without it (in a container, for example) only text is synced. The clipboard used is X11's CLIPBOARD, so native Wayland apps only see it through XWayland
- Decoding on the controller relies on WebKitGTK + GStreamer: H.264 needs `gstreamer1.0-plugins-bad` (parsing) and `gstreamer1.0-libav` or openh264 (decoding); AV1 / HEVC depend on the installed GStreamer decoders. If a format turns out not to produce a picture, the controller switches to the next available format by priority

## Web console

- Reloading or closing the tab disconnects its sessions; My devices isn't saved and is cleared when the page reloads
- In Firefox, text copied locally isn't synced to the remote side automatically
- Local files only go to the other side when you press the paste key on the remote screen or drop them on it (web pages can't read files from the clipboard). Files copied on the other side don't go into the local clipboard: click "Download". Chrome / Edge save into a folder you pick; other browsers assemble the download in memory (several files or a folder become a ZIP) and are limited to 1 GiB
- Outside fullscreen some shortcuts (Ctrl+W…) can't reach the remote computer

## Android

Host (sharing the phone):

- A paired device connects without anyone at the phone confirming it (a notification tells you each time). Paired devices can only be forgotten all at once (Share screen or Settings), not one by one
- Each time sharing starts, screen recording must be confirmed on the phone (a system rule), and sharing can't resume by itself after the system kills the app
- Remote control uses accessibility gestures, so apps that handle touches themselves (games…) may react less precisely
- Typing only works in regular text fields (arrow keys and Tab need Android 13+)
- A secured lock screen and screens marked secure are black
- Since Android 10 apps can't read the clipboard in the background, so the clipboard only syncs from the controller to the phone
- Files on the phone reach the other side through the system share menu ("Send to the other device"), and only files: the share menu can't share folders. Folders copied on the other side can't go on the phone's clipboard, only be saved to Downloads from the notification; Android 9 and earlier save them in the app's own folder (`Android/data/<app>/files/Download/Airhand`). On Android 7 other apps can only read the remote files from start to end, not jump around in them
- Sharing sound needs Android 10+, and some apps don't allow their sound to be captured
- HEVC and AV1 are only offered when the phone has a **hardware** encoder for them (few phones have an AV1 hardware encoder so far), otherwise H.264 is used (with the system's software encoder if there's no hardware H.264 encoder)

Controller:

- One device at a time; no A/V sync; TVs only watch
- There's no foreground service while files are transferred: if the app stays in the background for long, the system may stop it and cut off a transfer
- HEVC and AV1 are only used when the device has a **hardware** decoder for them, otherwise H.264

AV1 (encoding and decoding) hasn't been verified on a real device yet: the emulator has no AV1 hardware codecs.
