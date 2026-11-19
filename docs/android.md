# Android (phones / tablets / TVs)

[← Back to README](../README.md) · [简体中文](android.zh-CN.md)

One APK (`android/`, see [building](building.md#android-apk)) for phones, tablets and TVs. The same app shows a different interface depending on how it's opened:

| Opened from | Interface | What it does |
|---|---|---|
| A phone / tablet home screen | Phone interface (three tabs) | **Control others**: watch and operate computers or other phones<br>**Be controlled**: share this phone's screen so others can watch and operate it |
| A TV home screen (Android TV / Google TV / TV boxes) | TV interface, everything works with the remote | **Watch only** (computers or phones); no control, can't be controlled |

On a TV, the phone entry point also switches to the TV interface.

## Phone: Remote

- The home page is a **6-digit control code field**; it connects as soon as 6 digits are in ("Paste code" also finds a code inside text like "code: 123 456"). Connecting happens right there (a spinner on the button) and the remote screen opens once connected; a wrong code, an offline device, too many attempts and so on are explained right under the field
- **Pairing code**: the first time you connect to a computer or a phone it asks for the 6-digit pairing code shown there (a computer's tray menu, a phone's Share screen tab; a dialog pops up, on TVs the keypad switches to the pairing code). Once paired, it remembers this phone. If a device's identity changed since the last connection (reinstalled, or someone impersonating it), you need to enter its current pairing code to pair again. Sessions are end-to-end encrypted: the relay server can't see the screen or what you type
- **My devices**: every device you connect to is saved (name, OS, control code, last connection, resume token, its end-to-end public key), so next time it's **one tap**. If it's offline the row says "Offline · connects automatically when it's back" and the remote screen opens by itself when it comes online; you can cancel any time. Devices can be renamed, their code copied, or removed (with undo)
- A card at the top leads back to a session that's still running

The remote screen is full screen with just one **draggable round button** (it snaps to the left / right edge) that opens the menu: keyboard, shortcuts, sound, A/V sync, quality (same presets as the desktop, plus the video format; see the quality panel in the [usage guide](usage.md)), clipboard (send to the remote clipboard / type it in), displays (with several monitors), mouse lock and game keys (when controlling a computer), fit screen, gestures, disconnect. The top of the menu shows the device's name, video format, frame rate, latency, bitrate and path. Rotating the phone to landscape fills the screen.

**Files**: files copied on the other side go on the phone's clipboard, so you can paste them in other apps, and the notification can "Save to Downloads" (the only way for folders). To send files from the phone, use **Share → Send to the other device** in a file manager or the gallery, then paste on the other side (this also works while the phone shares its screen: they go to the device controlling it). Copying only sends a list; the contents travel when you paste (see [Usage → Copying and pasting files](usage.md#copying-and-pasting-files)).

Two **touch input modes** (two big buttons in the menu switch any time, Settings sets the default; the default "Automatic" uses the trackpad for computers and touch for phones). The gestures are explained the first time each mode is used:

| Gesture | Trackpad (computers) | Touch (computers) | Touch (phones) |
|---|---|---|---|
| Slide one finger | Move the pointer (precise when slow, accelerated when fast) | Drag with the left button | Every finger is passed through: taps, long presses, swipes, pinch-to-zoom and multi-finger gestures all happen on the remote phone |
| Tap | Click; tap twice = double-click | Click under the finger | 〃 |
| Long press | Right-click | Right-click under the finger | 〃 |
| Tap, then hold and drag | Drag with the left button (select text, move windows) | — | 〃 |
| Two-finger tap | Right-click | Right-click | 〃 |
| Two-finger drag | Scroll (natural, with momentum) | Scroll | 〃 |
| Pinch | Zoom in, the view follows the pointer | Zoom / move the view | 〃 |
| Three-finger tap | Show / hide the keyboard | Same | 〃 (the keyboard is in the menu) |

When the remote phone runs Android 7 (no continuous multi-finger gestures), only one finger is passed through and pinching zooms the local view.

After you pinch to zoom the local view, a moment after your fingers stop the app asks the remote side for a sharper picture (based on the zoomed size, rounded up to 720p / 1080p / 1440p / 4K, at most the remote screen's native resolution, a bit less if this device's decoder can't take it), and goes back to the quality setting's resolution when you zoom out. The bitrate cap stays the same: moving content may look a little softer than before, still content sharpens up within a few frames.

- In trackpad mode the pointer is drawn locally (with the remote shape), so it moves without lag; in touch mode a fading circle shows where you tapped
- **Keyboard**: the picture shrinks to stay fully visible above it, and a row of extra keys sits on top: Esc, Tab, Ctrl, Alt, Shift, Win (⌘ for Macs), arrows, Home / End, PgUp / PgDn, Del, F1–F12. Modifiers are sticky: tap once for the next key only, again to lock, again to release (tap Ctrl, then type c = Ctrl+C). While an IME composes (pinyin etc.) the text shows above the extra keys and is only sent once chosen
- When controlling a phone, **Back / Home / Recents** keys sit below the picture (can be turned off in Settings), and the shortcut menu holds system actions: notifications, quick settings, screenshot, lock screen, power menu, volume (whichever the phone supports). For computers the shortcut menu lists shortcuts for the remote OS (Windows / macOS / Linux)
- A connected mouse and keyboard map directly to the remote mouse and keyboard
- **Games**: when the remote computer hides its cursor (turning the camera in a game), trackpad slides and touch drags send how far you moved instead of a position, so the camera turns smoothly and doesn't stop at the edge of the picture; a connected mouse is captured (Android 8+). **Mouse lock** in the menu does the same all the time, e.g. for Macs, which can't report a hidden cursor. For games played with the keyboard, see game keys below
- Back first asks "Disconnect?", so it can't happen by accident. Network drops reconnect automatically; if the device goes offline it says so and reconnects when it's back; if the other side disconnected you or changed its code, it says why and doesn't retry
- In the background the session stays, at 2 fps and without sound, and resumes immediately when you return
- **A/V sync** is off by default. Enable it in the session menu or Settings; the choice is saved. While sound is playing, video follows the actual audio output clock, and audio waits if video stays behind. This is useful for watching videos and adds control latency. Turning sound off, backgrounding and reconnecting reset the clock measurements
- Sound uses adaptive jitter buffering (the same approach as the desktop console): each packet's lateness is measured against its capture time and the buffer grows right away to cover it, and the largest lateness of roughly the last minute is also the floor it shrinks back to, so an occasional hiccup doesn't cause another dropout as soon as the buffer shrinks. Small speed corrections cancel clock drift. Excessive backlog skips old audio; short notification sounds still play

### Game keys

When controlling a computer, **Game keys** in the menu lays buttons and a joystick over the picture, so games that need a keyboard (WASD to walk, Space to jump, hold Shift to run…) can be played from the phone. It lives only in the controller: it sends the key, mouse button, wheel and relative-motion events the protocol already has, so hosts don't need an update; Windows injects them as scan codes, which games accept.

- **Buttons**: hold a button to hold its key, let go to release it. A button can press a keyboard key, a mouse button, the wheel (keeps turning while held) or a combination of up to 3 keys (pressed in order, released in reverse, e.g. Ctrl+1). A button can be set to **tap to toggle** (tap to press, tap again to release: auto-run, staying crouched), and shows an accent ring while it is down. Sliding your finger off a button doesn't release it (thumbs drift while firing)
- **Joystick**: WASD or the arrow keys, 8 directions (two keys on diagonals). Both the direction boundaries and the dead zone in the middle have hysteresis, so a thumb resting on a boundary doesn't make keys flicker. It can also press a key when pushed past its edge (Shift to sprint, etc.). By default it centers where your thumb lands, so you don't have to hit the middle
- **Turning the view**: fingers that don't land on a button work as usual (trackpad / touch). When the game hides the cursor, dragging on empty space turns the view: it sends motion only, without holding the left button or accelerating, times the layout's sensitivity. A quick tap is still a left click, but a resting thumb is not a long press (no accidental scope), and a slow release is not a click (no accidental shot). A button can be set to **drag to turn the view**: in the shooter template, holding fire and dragging aims while firing
- A key held in several places (the joystick's W and a W button) is released only when all of them let go. Opening the menu, pulling down the notification shade, going to the background, losing the connection, opening the keyboard and switching layouts release every held key (toggles too), so your character doesn't keep walking
- The joystick and buttons are excluded from the system back gesture (with gesture navigation, swiping in from the side means Back), and buttons stay clear of the display cutout
- **Layouts**: three templates to start with — shooter (WASD joystick that sprints past the edge; fire, scope, jump, crouch, reload, E, Q, weapons 1–3, Tab, Esc), skills (MOBA / ARPG: Q W E R, D F, items 1–4, B, Space; tap the picture to click) and arrow keys (platformer / emulator: arrow-key joystick, Z X C, Space, Enter, Esc). You can create new ones (from a template or blank), duplicate, rename and delete them. Each computer remembers whether game keys are on and which layout it uses
- **Edit layout** (Game keys → Edit layout): the picture dims under a dot grid. Drag buttons to move them (they snap to other buttons and the middle of the screen, with an alignment line and a light tick), pinch with two fingers to resize the selected one, and tap a button to open its settings on the other side of the screen (key, text on the button, hold / tap to toggle, drag to turn the view, size; for the joystick: directions, key pressed past the edge, center where you touch, size), or duplicate / delete it. The toolbar has undo (every step can be undone), add (a key, or a joystick), layout settings (name, opacity, view turning speed, vibrate on press), cancel and done; drag it by the handle on its left when it covers a button. You edit a copy: nothing is saved until Done, and cancelling with changes asks first
- A short explanation is shown the first time

## Phone: Share screen (be controlled)

On the "Share screen" tab, tap **Start sharing screen**: Airhand asks for notification and audio-recording permission (recording is only used to share **what the phone is playing**, never the microphone; the screen is shared even if you decline), then the system asks to confirm screen recording. Once sharing:

- The **control code** is shown in large digits, with copy and share. Like the Windows installation it is **fixed** (saved in the app's own data directory, and the server keeps it for this phone) until you tap "Change control code"
- The **pairing code**, the same as on computers: the control code alone never gets anyone in (the relay server knows every control code), so a device connecting for the first time also has to enter this 6-digit code. It never goes through the server. Once paired, the phone remembers that device (by its public key from the end-to-end handshake, which the relay server can't fake) and the control code is enough from then on, even after sharing stops and starts again. Share sends both codes. After 5 wrong pairing codes pairing is paused until you pick a new one (**New pairing code**). **Forget paired devices** (here or in Settings) makes them enter the pairing code again; **Change control code** also changes the pairing code and forgets them all. "Disconnect all" only disconnects: paired devices can come back
- When a device connects, a **notification** says who it is, whether it can watch or control, from which IP, and the **safety code** (the other device shows the same one; if not, someone is in between), with "Disconnect all". Reconnects of the same device update that notification instead of alerting again, and it stays after they leave, so you can see who was here
- **Who's watching** (name, IP, safety code) is listed, with "Disconnect all"; the sharing notification always shows "Sharing · control code" and "Stop sharing"
- The screen is kept on while someone is watching (nothing can be captured when it's off); with nobody watching nothing is captured or encoded
- Below is a **permission checklist**, each item with its purpose, state and a "Turn on" button:
  - **Remote control** (accessibility service): needed to tap, swipe and type; without it the other side can only watch (and is told so). From Android 13, apps not installed from a store must first be allowed in "App info → ⋮ at the top right → Allow restricted settings"; the guide explains it and links there
  - **Share sound** (Android 10+, audio-recording permission)
  - **Notifications** (Android 13+)
  - **Run in background** (ignore battery optimization): keeps the system from stopping sharing when locked or in the background

Remote input maps to the phone like this: touch points and the left mouse button are fingers (tap, long press, drag, swipe; several at once is multi-touch), the wheel swipes; Back, Home, Recents, notifications, screenshot and so on are **system actions** in the protocol, sent by the controller's navigation keys and shortcut menu (the desktop console's shortcut menu lists them too when the remote side is a phone). For people at a computer, right-click / Esc = Back, middle-click / Win alone = Home, Alt+Tab = Recents, Ctrl+C / V / X / A = copy / paste / cut / select all; typing goes straight into the focused text field (physical keys from desktop consoles are translated with a US layout).

## Phone: Settings

Server address and certificate fingerprint (defaults are compiled in and can be changed here; a malformed fingerprint can't be saved; left empty, the server's certificate is remembered on the first connection and a different one is refused later, and saving the fingerprint again forgets it), this device's name (shown to others, defaults to the system device name), change control code (also changes the pairing code and forgets paired devices), paired devices (how many, and forget them), default quality, default touch input, play sound by default, A/V sync, navigation keys for phones, local cursor while dragging windows (on by default: the trackpad cursor is still drawn while a Windows host draws its own into the picture), clear saved devices, version and protocol version.

## TV

- Enter the control code on the left with the remote's number keys or the on-screen keypad; recent devices are on the right (arrow keys to pick, OK to connect); to remove a device, use the delete button next to it, or hold OK / press Menu on it (asks first); Settings (server, fingerprint, device name) is at the top right
- The picture is full screen with sound on. **OK / Menu** opens the side menu: sound, A/V sync, displays, quality (each press picks the next preset), video format (auto / each format both sides support, each press picks the next one), picture size (fit / fill, cropping the edges / stretch), connection info, disconnect; video format and picture size are remembered for later connections; **left / right** switch the remote display; **Back** closes the menu, and asks before disconnecting
- TVs only watch, so there's no warning when the other side hasn't allowed remote control (the phone's accessibility service, the Mac's Accessibility permission)

## Where the data is stored

Settings (`settings.json`), saved devices (`devices.json`), this phone's control code with the secret to reclaim it (`host.json`), its pairing code with the paired devices (`pairing.json`) and the game key layouts (`game.json`) are only kept in the app's private directory (`/data/data/app.airhand.android/files/`), which other apps can't read. Backup and device transfer are disabled in the manifest, and **uninstalling the app deletes all of it**. Logs only go to logcat.
