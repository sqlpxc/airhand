# Usage guide

[← Back to README](../README.md) · [简体中文](usage.zh-CN.md)

## Host (default)

Double-click to run (on Windows install it first, see [Windows installation](#windows-installation)). A tray icon appears in the bottom-right corner (on macOS in the menu bar at the top of the screen, on Linux in the top bar / panel status area). Grey means not connected to the server, coloured means connected. The right-click menu (a plain click on macOS) contains:

- Current mode and server status
- The **control code** and "Copy control code"
- The **pairing code**, "Copy pairing code" and **Change pairing code**: a controller that hasn't paired with this computer needs it, besides the control code, the first time it connects (see "Pairing" below)
- Who is controlling this computer right now (with the **safety code** after each connection), and **Disconnect all controllers**
- **Change control code**: the old code stops working immediately and connected controllers are disconnected; the pairing code changes too and all paired controllers are forgotten
- **Forget paired controllers**: they'll need the pairing code again
- Open console and **Start with Windows** (Windows) / Switch to controller (macOS, Linux) and **Open at login** (macOS) / Language / **Uninstall Airhand…** (macOS, see [Uninstalling](#uninstalling)) / Quit

After a network drop the host reconnects automatically with exponential backoff and **gets its old control code back**, even after a server restart. The code is **fixed**: it's saved in `config.toml`, and the server keeps it for this computer however long it stays offline (codes unused for 180 days are released), so it stays the same across restarts and upgrades until you change it.

**Pairing**: the relay server assigns control codes, so it knows all of them; the control code alone therefore doesn't get anyone in (otherwise the server could connect by itself). The first time a controller connects to this computer it has to enter the 6-digit **pairing code** from the tray menu; the pairing code never goes through the server. After that this computer remembers the controller and the control code is enough. After 5 wrong pairing codes in a row pairing is paused (the tray says so) until you pick a new pairing code. There's no way to turn pairing off. See [Security → End-to-end encryption](security.md#end-to-end-encryption).

> On Linux the tray icon uses AppIndicator. It works out of the box on Ubuntu, KDE, Xfce and similar; stock GNOME needs the "AppIndicator and KStatusNotifierItem Support" extension. Without an AppIndicator library the program still runs as a host and prints the control code to the log (stderr).

> On macOS the host needs two system permissions: **Screen & System Audio Recording** (to capture the screen and sound) and **Accessibility** (to control the keyboard and mouse). macOS asks on the first start; while one is missing, the menu says so, and clicking that item opens the matching page in System Settings → Privacy & Security. A connected controller is told which permission is missing too (in the controller's own language), and the message goes away once it is granted. After granting Screen & System Audio Recording, reopen Airhand. Airhand normally lives in the menu bar only and shows up in the Dock while the console is open.

> **Open at login** on macOS (switched in the menu bar icon's menu): after you log in, Airhand starts in the menu bar by itself, so the Mac can still be controlled after a restart (on macOS programs can only run after login, and the login window can't be captured anyway; with automatic login it amounts to starting at boot). It is the same entry as "Open at Login" in System Settings → General → Login Items ("Login Items & Extensions" on some versions of macOS), so turning it off there works too and the menu follows. **It's turned on the first time Airhand runs from the Applications folder** (macOS shows a "Login Item Added" notification), and only that once: if you turn it off, it stays off and Airhand never turns it back on by itself. Only an Airhand in the Applications folder (`/Applications` or `~/Applications`) can turn it on: when it's opened straight from Downloads or a disk image, macOS runs it from a temporary location that won't be there next time, so the menu asks you to move it to the Applications folder first.

## Windows installation

On Windows Airhand is installed with its installer (into `C:\Program Files\Airhand` for all users; needs administrator rights). The host runs as the system service **Airhand**, as SYSTEM in the session of whoever is at the screen, so it keeps working on the lock screen, the sign-in screen and UAC prompts, and follows when users are switched. The tray icon is an ordinary program that shows the service's status and controls it:

- Opening Airhand starts the service; **Quit** in the tray stops it (as before, the computer can't be controlled after quitting); when the service stops (or is uninstalled) the tray icons close too
- **Start with Windows** (tray menu, on by default): the service starts at boot, so the computer can be controlled before anyone signs in, and the tray icon appears after signing in
- The configuration file `config.toml` is in the install directory (created on installation, see [Configuration file](#configuration-file)) and maintained by the service; edit it as administrator
- **Uninstalling** (Settings → Apps) stops and removes the service, the startup entry, `config.toml` and the program; nothing is left behind (see [Uninstalling](#uninstalling)). Installing a new version over an old one keeps `config.toml`, and with it the control code and My devices, even if you choose to uninstall the old version first
- A bare `airhand.exe` doesn't run without being installed: release builds ask you to run the installer. Debug builds (`npm run tauri dev`) run the host inside the tray process, like on macOS and Linux, with `config.toml` next to the program (`client/src-tauri/target/debug/`)

## Configuration file

Everything Airhand saves is in one `config.toml` (with comments). The menu in the console's top-left corner also shows where it is:

| System | Location |
|---|---|
| Windows | The install directory: `C:\Program Files\Airhand\config.toml` (next to the program for debug builds that aren't installed) |
| macOS | `~/Library/Application Support/app.airhand.client/config.toml` |
| Linux | `$XDG_CONFIG_HOME/airhand/config.toml`, by default `~/.config/airhand/config.toml` |

| Key | Description |
|---|---|
| `server` | Relay server `host:port`; without it, the address built into the program |
| `server_fp` | Relay certificate fingerprint; `""` means no fixed fingerprint: the server's certificate is remembered on the first connection (`[server_trust]`) and a different one is refused later; without it, the built-in fingerprint |
| `[server_trust]` | Written by Airhand: the relay's certificate fingerprint remembered on the first connection (when no fingerprint is set). Delete it if the relay really gets a new certificate |
| `autostart` | Windows: start with Windows (also in the tray menu). macOS: a record of Open at login; System Settings is what counts, so editing only this line has no effect. Without it, Open at login hasn't been turned on by default yet (see above) |
| `language` | UI language, `"zh"` or `"en"`: written when you pick one in the tray menu or the console (see [Language](#language)). Without it, the system language. On Windows it applies to every account on the computer (the file is shared) |
| `[code]` | This computer's control code, and the secret that gets it back. Delete `code` and `secret` to get a new code |
| `[host]` | Maintained by Airhand: this computer's end-to-end identity as a host (`key`), the pairing code and whether pairing is paused, all stored encrypted. Deleting `key` creates a new identity: controllers that connected before will warn that this computer's identity changed |
| `[[paired]]` | Maintained by Airhand: paired controllers (public key, name, when they paired); they connect without the pairing code. Delete an entry to forget that controller |
| `[controller]` | Maintained by Airhand: the console's end-to-end identity (stored encrypted, one per account on Windows). Paired computers recognize the console by it; delete it and you'll have to pair again |
| `[[device]]` | The console's **My devices**: computers you've connected to (control code, their name, the name you gave them, last quality settings, last connected, their public key `host_key`). Saved automatically on connect, up to 50. To add one by hand, write the 6 digits as `code` |

- Restart Airhand after changing settings (Quit, then open it again). Airhand keeps your edits and comments when it saves the code or devices
- **Deleted by accident** (the file or the whole directory): it's recreated with the default settings at startup, and within a few seconds while running. This computer's control code, the autostart setting (on macOS, the record of Open at login), the UI language, the end-to-end identities, the paired controllers and My devices carry over from before the deletion, so nothing is lost; other settings go back to their defaults
- A control code is enough to control its computer, so treat it like a key. On macOS / Linux only your account can read and write the file (mode 600, directory 700). On Windows the file is in the install directory, readable by every account on the computer: this computer's secret is encrypted with DPAPI for the SYSTEM account, and the codes in My devices with the Windows account that saved them, so other accounts and other computers can't read them (each account on the same computer only sees its own devices)
- Windows removes it when uninstalling; on macOS / Linux it stays after the program is removed (as is customary there), so remove it too, see [Uninstalling](#uninstalling)

## Uninstalling

After these steps nothing of Airhand is left on the computer (except two things on macOS that the system protects and Airhand can't remove, see the end of [macOS](#macos)). The configuration file holds this computer's control code and the codes of the computers in My devices (they work like keys), so do delete it if you stop using Airhand.

### Windows

Settings → Apps → Airhand → Uninstall. The uninstaller stops and removes the Airhand service and the startup entry, and deletes the install directory (including `config.toml`), the registry keys the installer wrote, the Windows Firewall rules added for `airhand.exe`, and the console's temporary directories. Nothing else to do by hand.

(Choosing "uninstall the old version first" during an upgrade isn't an uninstall: `config.toml` is kept, see [Windows installation](#windows-installation).)

### macOS

macOS apps have no uninstaller, and moving the app to the Trash only removes the program itself. Besides the program, Airhand leaves these behind on macOS (in the order they're removed):

| Item | Where | Notes |
|---|---|---|
| Incomplete file transfer | `.airhand-receive-<random ID>` in the selected folder, referenced by `pending-receive-<random ID>` in the configuration directory | Cancelling, quitting or uninstalling cleans up partial transfers and keeps completed items. After a forced quit, cleanup runs at the next launch. Reconnect any drive removed during a transfer first. |
| Airhand Microphone | Core Audio input device and its tap | Removed on quit. Startup and the uninstall menu also remove this user’s objects after a forced quit. |
| Open at login | System Settings → General → Login Items ("Login Items & Extensions" on some versions of macOS), under both "Open at Login" and "App Background Activity" | Has to be unregistered by Airhand itself: if you delete the app first, a dead Airhand entry stays in both places |
| Permissions | System Settings → Privacy & Security: Screen & System Audio Recording, Accessibility and so on | Can only be cleared while the app is still there; once it's deleted macOS can't find it any more |
| Configuration | `~/Library/Application Support/app.airhand.client/` | Holds the control codes, so be sure to delete it |
| Caches macOS created for it | `*app.airhand.client` in the system's cache and temporary directories (`getconf DARWIN_USER_CACHE_DIR`, `DARWIN_USER_TEMP_DIR`) | Created by Metal and WebKit under Airhand's name; mostly empty, at most a small GPU shader cache. WebKit's ones in the temporary directory can't be removed, see the end |
| Crash reports | `~/Library/Logs/DiagnosticReports/airhand-*.ips`, `~/Library/Application Support/CrashReporter/airhand_*.plist` | Only there if Airhand has crashed |
| Menu bar icon settings | `~/Library/Preferences/app.airhand.client.plist` | Airhand doesn't use this file and it normally doesn't exist: macOS writes it when you ⌘-drag the menu bar icon (its position) or turn it off in System Settings → Menu Bar (hidden). If it stays, the icon is still hidden after installing Airhand again |

**Uninstalling from the menu**: click the Airhand icon in the menu bar and choose **Uninstall Airhand…**. After you confirm, it removes everything in the table in that order (skipping what the system protects, see the end), moves Airhand to the Trash and quits; empty the Trash to delete it for good. If an item didn't work out, the final message lists it; finish it with the matching command under "Uninstalling by hand" below.

**Uninstalling by hand**: if Airhand won't open, or you'd rather do it yourself. The first two steps have to come before the app is deleted, **in this order**.

1. Click the Airhand icon in the menu bar, **turn off "Open at login"**, then choose **Quit** (older versions don't have this item and never registered a login item, so just quit).
2. Open Terminal and clear the permissions (only Airhand's):

   ```bash
   tccutil reset All app.airhand.client
   ```

   It should say `Successfully reset All approval status for app.airhand.client`. If it says `No such bundle identifier`, the app is already gone, see "Already deleted the app" below.
3. If the app was force-quit during a transfer, reopen it once to clean up and then quit. For manual cleanup, `find "$HOME/Library/Application Support/app.airhand.client" -maxdepth 1 -type l -name 'pending-receive-*' -exec readlink {} \;` shows the hidden folder for that transfer. You can remove that folder in Finder. Keep the completed files alongside it.
4. Delete the rest and the program:

   ```bash
   rm -rf ~/Library/Application\ Support/app.airhand.client
   find "$(getconf DARWIN_USER_CACHE_DIR)" "$(getconf DARWIN_USER_TEMP_DIR)" -maxdepth 1 -name '*app.airhand.client' -exec rm -rf {} + 2>/dev/null
   find ~/Library/Logs/DiagnosticReports ~/Library/Application\ Support/CrashReporter -maxdepth 1 -name 'airhand[-_]*' -delete 2>/dev/null
   defaults delete app.airhand.client 2>/dev/null; rm -f ~/Library/Preferences/app.airhand.client.plist
   rm -rf /Applications/Airhand.app ~/Applications/Airhand.app
   ```

   One line per item: the configuration, the caches macOS created for it, the crash reports, the menu bar icon settings and the program. Instead of the last line you can also drag Airhand from the Applications folder to the Trash (and empty it). `find` runs into other apps' protected directories; those errors are silenced and don't affect the result. The menu bar icon settings are first deleted through the system's preferences service (`defaults delete`), which leaves an empty file behind for `rm` to delete.

**Check**: if these commands print nothing except WebKit's empty directories in the temporary directory (they can't be removed, see the end), and neither System Settings → General → Login Items nor Privacy & Security → Screen & System Audio Recording / Accessibility lists Airhand, it's all gone.

```bash
ls -d ~/Library/Application\ Support/app.airhand.client ~/Library/Preferences/app.airhand.client.plist /Applications/Airhand.app ~/Applications/Airhand.app 2>/dev/null
find "$(getconf DARWIN_USER_CACHE_DIR)" "$(getconf DARWIN_USER_TEMP_DIR)" -maxdepth 1 -name '*app.airhand.client' 2>/dev/null
find ~/Library/Logs/DiagnosticReports ~/Library/Application\ Support/CrashReporter -maxdepth 1 -name 'airhand[-_]*' 2>/dev/null
```

**Already deleted the app** (without steps 1 and 2 first): put Airhand.app back where it was (for example drag it from the downloaded `.dmg` into the Applications folder again), open it, and start again from step 1. macOS still remembers the login item, and turning it off in the menu removes both entries, under "Open at Login" and "App Background Activity". If you'd rather not install it again, remove them by hand: select Airhand under "Open at Login" in Login Items and click "−", do the same in Privacy & Security → Screen & System Audio Recording / Accessibility, then follow steps 3 and 4.

**What Airhand can't remove**: the system protects these two.

- WebKit's directories `com.apple.WebKit.*+app.airhand.client` in the system's temporary directory (only there if you've opened the console): they're under System Integrity Protection (SIP), so nobody can remove them; deleting them says `Operation not permitted`, and uninstalling from the menu skips them. They're empty; every app that uses WebKit (Safari included) has directories like these there.
- On macOS 26 and later, the list of apps allowed in the menu bar (System Settings → Menu Bar → Allow in the Menu Bar) may still show Airhand. The system keeps that list in a protected file and offers no way to remove a single app from it; you can only reset the whole list as described below. It's only a record and does nothing if you leave it; if you install Airhand again and had turned it off there before, its icon won't appear in the menu bar until you turn it back on there.

**Resetting the menu bar app list** (optional, macOS 26 and later): the list is the `trackedApplications` entry in `~/Library/Group Containers/group.com.apple.controlcenter/Library/Preferences/group.com.apple.controlcenter.plist`. Once that entry is deleted and you've logged out and back in, the list is empty; apps with a menu bar icon are added again as they run, and deleted apps don't come back. This isn't something Apple provides, it edits the system's own file, and **it clears the choices for every app in the list**: menu bar icons you turned off will show up again, and you'll have to turn them off again in System Settings → Menu Bar.

1. Give Terminal Full Disk Access: turn on Terminal in System Settings → Privacy & Security → Full Disk Access (if it isn't listed, click "+" and add Applications → Utilities → Terminal), then quit Terminal and open it again. The file is in a protected directory and can't be read or changed without this.
2. Quit System Settings, then in Terminal back the file up and delete the entry:

   ```bash
   cd ~/Library/Group\ Containers/group.com.apple.controlcenter/Library/Preferences
   cp group.com.apple.controlcenter.plist ~/group.com.apple.controlcenter.plist.bak
   defaults delete "$PWD/group.com.apple.controlcenter.plist" trackedApplications
   ```

   Delete it with `defaults` rather than editing the file: the system's preferences service still has it cached, and an edited file may get the old contents written back.
3. Log out and back in (or restart), then have a look at System Settings → Menu Bar.
4. Turn Terminal off again under Full Disk Access.

To get the old list back: in a Terminal with Full Disk Access, run `defaults import ~/Library/Group\ Containers/group.com.apple.controlcenter/Library/Preferences/group.com.apple.controlcenter.plist ~/group.com.apple.controlcenter.plist.bak`, then log out and back in.

### Linux

```bash
sudo apt remove airhand     # stops the packaged app and releases its PipeWire microphone
rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/airhand"  # remove this user's configuration and pairings
```

There's no startup entry and no system permission to clean up on Linux. The tray icon's and the console's temporary directories (`$XDG_RUNTIME_DIR/airhand-run-*`, `/tmp/airhand-webview-*`) are deleted when Airhand quits; they're only left behind if the process was killed (they're cleaned up at the next start), and the system usually clears them when you log out or reboot. To remove them by hand: `rm -rf "$XDG_RUNTIME_DIR"/airhand-run-* /tmp/airhand-webview-*`.

### Android

Uninstall the app in the system: the settings, saved devices and this phone's control code are all in the app's private directory, which the system deletes (see [Android → Where the data is stored](android.md#where-the-data-is-stored)); the accessibility service used for remote control goes with the app.

## Controller

Tray → **Switch to controller** opens the console. Closing the console window switches back to host mode. On Windows, double-click the Airhand tray icon or choose **Open console** from its menu. If the console is already open, double-clicking brings it to the front. The installed host service keeps running while the console is open.

- It works like a browser: each computer gets a **tab** that you can drag to reorder and close (= disconnect) with × or a middle click. Click **+** for a new connection, enter the 6-digit control code (pasting `123 456` works), and once connected that tab becomes the computer. With two or more connections a **Multi-screen overview** tab appears on the left
- **First connection to a computer or phone**: it asks for a pairing code (every host does), and the new-connection page shows a "Pairing code" field: enter the 6-digit pairing code shown there (a computer's tray menu, a phone's Share screen tab in Airhand) and connect again (or paste all 12 digits at once: control code + pairing code). Once paired, it remembers this console
- **End-to-end encryption**: sessions are encrypted end to end, so the relay server can't see the screen or keystrokes. The console remembers a computer's identity the first time it connects; if the identity changes later (reinstalled, or someone impersonating it), the console warns and doesn't reconnect by itself. Once you've confirmed it was reinstalled, enter its current pairing code (from its tray, or a phone's Share screen tab) to pair again. Hover the control code in the toolbar to see the **safety code**: if the remote device shows the same one (a computer in its tray, a phone in its list of who's watching), nobody is in between
- **My devices**: computers you connect to are saved (in `config.toml`); next time, one click on the new-connection page connects with the quality settings you used last. Rename or remove them (with undo), and search when there are many. If one is offline, there's no error: its tab opens and waits, and connects as soon as it's back (using the saved reconnect token, which doesn't count as code guessing)
- The window has no system title bar: drag an empty part of the tab bar to move it, double-click to maximize. The icon on the far left opens the menu: this computer's name, server status, language, switch to host. Closing the console while connections are open (including from the taskbar or Alt+F4) asks first
- Shortcuts (when not controlling a computer): Ctrl+T new connection, Ctrl+W close tab, Ctrl+Tab / Ctrl+Shift+Tab switch tabs, Ctrl+1–9, Ctrl+Shift+T reopen the last closed connection. While the remote screen has focus, every key goes to the remote computer
- Toolbar (like an address bar): remote name, control code, frame rate / bitrate / latency, encoder; monitor switch (when the remote computer has several, see [Multiple displays](#multiple-displays) below), **Quality**, shortcuts, clipboard (two-way sync / type as keystrokes), sound (mute / A/V sync), microphone (only when the remote computer can take it, see [Microphone](#microphone) below), mouse (see [Mouse](#mouse) below), actual size (1:1), fullscreen, disconnect
- When controlling a Mac, enable **Send shortcut → Swap Ctrl and ⌘ Command** to copy with Ctrl+C on a Windows keyboard and use the Win key as Control. The shortcuts in the menu are unchanged. The switch is off by default and remembered per device in the desktop console; the web console forgets it on refresh.
- In fullscreen the tab bar is hidden and the toolbar slides in when the mouse touches the top edge; click the remote name to switch to another connection without leaving fullscreen. The console also tries to lock the keyboard, so system shortcuts such as the Win key and Alt+Tab go to the remote computer. On macOS fullscreen doesn't open a separate desktop Space, and the menu bar and Dock stay hidden (so they don't cover the toolbar or the remote taskbar; they come back while you're in another app); leave fullscreen with the toolbar button. The window's green button enters the system's own fullscreen, with the menu bar and Dock as usual
- **Playing games**: the default Auto mode locks the mouse by itself when the remote computer hides its cursor (turning the camera in a game); on a computer you mostly play games on, switch to Game mode, see [Mouse](#mouse) below
- Network hiccups reconnect automatically. If the user at the host disconnects you, the console says so and doesn't reconnect
- If the remote computer goes offline (shut down, asleep, network gone), the console shows "Waiting for the remote computer to come back" and reconnects as soon as it's back with its old code. It doesn't keep retrying in the meantime, and the server doesn't mistake the reconnects for code guessing
- A phone needs no one to tap anything once you're paired; it shows a notification with your device's name and the safety code when you connect. With a phone, the shortcut menu holds system actions such as Back, Home and notifications

**Quality panel**: presets from Data saver (720p / 24 fps / 1.2 Mbps) through Balanced (1080p / 30 fps / 4 Mbps), Smooth (1080p / 60 fps / 8 Mbps) and HD (1440p / 60 fps / 16 Mbps) to Original (native resolution / 60 fps / 30 Mbps), or set the resolution cap, frame rate (15–120), bitrate cap (0.5–100 Mbps), codec and adaptive bitrate yourself. The console only offers codecs this computer can hardware-decode; if the host has no matching hardware encoder it falls back automatically, and the toolbar shows the encoder actually in use.

When the remote screen is larger than the requested resolution, the host scales it on the GPU when possible to keep latency low. CPU scaling is an automatic fallback.

QUIC retransmits lost video packets. If a frame takes too long, the host resumes from a reference frame already at the controller or sends a keyframe.

Besides the resolution cap, the picture is never larger than what the window actually shows: when the window is smaller than the remote screen, the host scales straight to the window size and the console shows it 1:1 instead of scaling it a second time (sharper text, less bandwidth). While you drag the window border, the resolution changes once you stop. In actual size (1:1) mode the window doesn't limit the resolution.

**Enhance (this device)**: Off / Low / Medium / High, off by default. When the picture is smaller than the window and has to be enlarged (a 1080p preset in full screen on a 2K / 4K screen, say), it's upscaled on this computer's GPU with a sharper filter (Catmull-Rom) and text edges are sharpened a little, on brightness only; at 1:1 it's only sharpened. No extra bandwidth and practically no extra latency. Strong settings also make compression artifacts (blocks, ragged text edges) more visible, so pick Low at low bitrates. It only applies to the connection you're working in (thumbnails in the overview aren't processed), applies to all connections, and is remembered on this computer. Without WebGL2 the browser's own high-quality scaling is used instead (no sharpening). To actually get more detail, raise the resolution cap and bitrate: enhancement can only display what's received better.

### Mouse

The mouse button in the toolbar (its icon follows the mode) picks how the mouse works. The choice is remembered per device, so the next connection to that computer uses it again:

- **Auto** (default): follows this computer's pointer, so the remote cursor is wherever you point, with no delay. When the remote computer hides its cursor (holding the right button to turn the camera in World of Warcraft, first-person games), the local pointer is locked to the picture and the mouse sends "how far it moved" instead, so it never stops at the edge of the screen; it's freed again when the remote cursor comes back (if a mouse button is held, once you let go, so turning the camera is never cut off halfway)
- **Desktop**: always follows this computer's pointer and never locks it. Use it when a program on the remote computer hides the cursor but shouldn't lock the mouse: videos, drawing, some design software
- **Game**: the local pointer stays locked and the mouse always sends "how far it moved"; the remote computer draws its own cursor into the picture, so what you see is exactly where its cursor is. Turning the camera and clicking the game's interface need no switching, and the pointer is locked only once (the browser's "press Esc to show your cursor" notice appears only then). The cursor arrives with the picture, a round trip behind your hand, and a low frame rate makes it less smooth, so a 60 fps quality setting works better. Also the mode to use when the remote computer is a Mac (it can't report a hidden cursor) or the game doesn't hide the cursor

While the pointer is locked, **hold Esc** (about a second) to free the mouse (in fullscreen the keyboard is locked, so a short Esc still goes to the remote computer, where games need it). In Auto mode it then won't lock by itself until your next click; in Game mode the mouse isn't sent to the remote computer until you click the picture again (a hint at the bottom says so), and that click only locks the pointer, it isn't sent, so nothing gets clicked in the game by accident. **Ctrl+Alt+Shift+M** switches Game mode on and off (off goes back to the mode you had before).

**Show my cursor when the picture has one** (in the mouse menu, on by default): a Windows host draws its cursor straight into the picture while a window is being dragged or resized. On: this computer's cursor stays visible and follows your hand with no delay, next to the (slightly slower) one in the picture. Off: only the one in the picture is shown. Either way the mouse keeps working by position and isn't locked. Not used in Game mode. Applies to all connections and is remembered on this computer.

### Multiple displays

When the remote computer has more than one display, the toolbar has a display button. By default you see one display: pick another in its menu to switch. Turn on **Show all displays at once** at the bottom of the menu to see all of them together, arranged the way they are on the remote desktop (side by side, stacked or offset, just like over there) and scaled to fit the window. Whichever one the mouse is over is the display you're controlling, and moving from one onto the next works like on the remote computer; keyboard, clipboard and sound work as with a single display. Click a display in the menu to go back to just that one.

- Each display is its own video stream: encoded separately, with its own bitrate control and its own loss recovery, so one doesn't hold up another. The quality settings apply to each stream, so two displays take about twice the bandwidth (adaptive bitrate turns each one down when the network can't keep up); each stream's resolution follows the size it's shown at in the window
- Up to 4 displays at once (a phone only has one)
- Plugging displays in and out on the remote computer is followed: a display that goes away disappears, and a new one is added while showing all displays
- Fullscreen, actual size (1:1, scrollable) and the overview thumbnails all work as usual; Enhance processes each display separately

**Separate windows**: when you have several screens too, you can put the remote displays in windows of their own, one window per local screen, each in full screen. Click the button next to a display in the display menu and it moves to a new window (the desktop app puts it on a screen the console isn't on); the main window shows the rest. The new window has buttons for full screen and for moving the display back to the main window at the top; in full screen the toolbar appears when the mouse touches the top edge, as in the main window. Closing the window also moves the display back.

- No extra connection: it's the same connection, that display's stream is just drawn in another window, so the bandwidth is the same as showing several displays at once
- With a single display shown you can still put another one in its own window (that adds a stream); at least one display stays in the main window
- The mouse and keyboard control the display in whichever window they're in; quality, sound, shortcuts and so on are in the main window's toolbar
- In a browser, allow pop-ups for the site

### Copying and pasting files

While clipboard sync is on (the clipboard button in the toolbar, on by default), files and folders can be copied and pasted too, in both directions: copy them on this computer and paste in the remote file manager, or copy them over there and paste here.

- Copying only sends a list; the transfer starts when you paste, and the progress bar and Cancel button are the pasting file manager's own. Nothing is transferred if you copy and never paste
- While the two ends have a direct path (see [How it works → Direct path](architecture.md#direct-path)) any size goes. When everything has to go through the relay server, one paste is limited to what the server allows (50 MiB by default, see `AIRHAND_MAX_FILE_SIZE` in the deployment guide), and you're told when it's over
- Files changed after you copied them can't be transferred: copy them again
- Shortcuts and symbolic links inside a folder don't bring along what they point to
- **Web console**: press Ctrl+V (⌘V on a Mac) on the remote screen to paste files copied locally, or drop files and folders on it. When the other side copies files a banner appears above the picture: click **Download** to fetch them (Chrome / Edge let you pick a folder to save into)
- **Android**: files copied on the other side go on the phone's clipboard, so you can paste them in other apps, and the notification can **Save to Downloads** (the only way for folders). To send files from the phone, use **Share → Send to the other device** in a file manager or the gallery, then paste on the other side
- **Receiving on a Mac**: copy files or folders on Windows or another connected device, then open **Airhand → Receive files to…** in the Mac menu bar. Pick a folder and click **Receive**. You can cancel from the same menu. Each top-level item appears once complete. Name conflicts get a numbered suffix. Files copied on the Mac can still be pasted in the other device’s file manager.

See [Known limitations](limitations.md) for the details.

### Microphone

Send this computer's microphone to the remote computer: programs there (meetings, voice typing, voice messages) hear you when they record from its virtual microphone. Off by default and turned on per connection (the microphone button in the toolbar → **Send this computer's microphone**); it's only sent while you're viewing that connection. The browser asks for microphone access the first time (the desktop console doesn't).

- **Windows hosts** use Steam’s **Steam Streaming Microphone** (Valve’s virtual microphone driver, signed by Microsoft). When you turn on forwarding, Airhand looks for an existing device and installs the driver from Steam on the remote computer if needed. Forwarding starts as soon as the device is ready. The microphone menu prompts you to install Steam if it’s missing from the remote computer
- In programs on the remote computer, choose "Microphone (Steam Streaming Microphone)" as the recording device, or make it the default under Sound settings → Input there, so programs that use the default microphone (such as Windows voice typing, Win+H) hear you directly
- Only one controller at a time: another controller sees "Another controller is using the remote microphone" and takes over when that one stops
- **macOS 14.2+ and Linux with PipeWire** publish **Airhand Microphone** when forwarding starts. Choose it as the input in the remote app. macOS may ask for permission for that app to record system audio. Turning forwarding off or disconnecting removes the device. PipeWire also removes it when Airhand exits. After a forced quit on Mac, reopening Airhand or choosing **Uninstall Airhand…** cleans up any leftover device and tap.
- Android hosts and the native Android controller do not yet support microphone forwarding.

## Language

The console and tray menu are available in **English** and **Simplified Chinese**. By default the client follows the system display language: Chinese if it is Chinese, English otherwise. Switch any time from the tray menu (Language / 语言) or the 中文 / EN switch in the console menu (the icon at the top-left). The choice is saved in `config.toml` (`language`) and used from then on; delete that line to follow the system language again.

Notices and errors from the other computer or the server are sent as message types, not text, so they always appear in **your** language. Logs (client and server) are always in English.
