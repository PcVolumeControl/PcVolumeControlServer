# Windows backend: manual test checklist

The Core Audio backend (`pcvc_audio::WindowsBackend`) was written and
type-checked on Linux and has not yet run on Windows. Work through this list
on a Windows 10 or 11 machine before relying on it, and note anything that
differs from the expected result.

## Setup

- Rust (MSVC toolchain, 1.88 or newer) and Git.
- At least two playback devices (speakers plus headphones, a USB headset or a
  Bluetooth device) and a couple of apps that play audio (a browser, a media
  player).
- Allow TCP port 3000 and mDNS (UDP 5353) through Windows Firewall when
  prompted, for the phone tests.
- `ncat` (ships with Nmap) or any line-based TCP client, for the protocol tests.

```powershell
cargo build -p pcvc-audio --example windows_dump
cargo build -p pcvc-server --example windows_server
$env:RUST_LOG = "info,pcvc_audio=debug"   # optional, shows skipped sessions and retries
```

Both examples log to stderr; keep any warnings they print with your notes.

## 1. Snapshot (`windows_dump`)

```powershell
cargo run -p pcvc-audio --example windows_dump
```

Compare the output with the Sound control panel (`mmsys.cpl`) and the Volume
Mixer.

- [ ] Every enabled playback device is listed, and no disabled, unplugged or
      recording device.
- [ ] Device ids are bare lowercase GUIDs such as
      `0f4090a9-dee2-4563-ba29-0ad6b93d9e22`; names match the Sound panel,
      such as `Speakers (Realtek High Definition Audio)`.
- [ ] The default device matches the one marked "Default Device".
- [ ] Master volume and mute match the tray volume flyout.
- [ ] Sessions match the Volume Mixer apps, without "System Sounds".
- [ ] Session ids look like
      `{0.0.0.00000000}.{<device id>}|\Device\HarddiskVolume..\app.exe%b{...}`
      and contain the default device's id.
- [ ] Session names are the app's display name or its exe name without
      `.exe` (`chrome`, `Spotify`); nothing shows as `@%SystemRoot%...`.

## 2. Events (`windows_dump`, keep it running)

Each action should print the event(s) listed and then a state that reflects
the change. Nothing should print when nothing changed.

| Action | Expected event |
| --- | --- |
| Drag the tray volume slider | `MasterChanged`, volume follows |
| Mute and unmute in the tray | `MasterChanged`, `muted` toggles |
| Move an app's slider in the Volume Mixer | `SessionChanged { id }` |
| Mute an app in the Volume Mixer | `SessionChanged { id }` |
| Start playback in an app that had no session | `SessionAdded { id }` |
| Close that app completely | `SessionRemoved { id }` within about 2 s |
| Switch the default device in the tray or Sound panel | `DefaultDeviceChanged`, new device's sessions |
| Plug in or unplug a USB or Bluetooth device | `DevicesChanged` |
| Disable and re-enable a non-default device in the Sound panel | `DevicesChanged` after each |
| Restart the default device's driver (Device Manager: disable and enable the audio adapter) while it stays the default | `DevicesChanged`; master and session changes still arrive afterwards |
| Restart the Windows Audio service (`Restart-Service audiosrv`, admin) | events resume afterwards, possibly after a logged retry |
| Rename a device (Sound panel, Properties) | `DevicesChanged`, new name |
| Change a recording device or the default microphone | nothing |

Then stop and drop the backend:

```powershell
cargo run -p pcvc-audio --example windows_dump -- --seconds 20
```

- [ ] `backend dropped` prints within a second of the deadline, without a
      warning about detaching the Core Audio thread, and the process exits.

## 3. Protocol (`windows_server` and `ncat`)

```powershell
cargo run -p pcvc-server --example windows_server
ncat 127.0.0.1 3000
```

On connect the server sends one full-state line. Paste each message below as
one line, replacing the ids with real ones from that state. The server never
replies directly; a broadcast follows every real change.

- [ ] Master volume: `{"protocolVersion":7,"defaultDevice":{"deviceId":"<default id>","masterVolume":42.0}}`
      sets the Windows volume to 42 and broadcasts `"masterVolume":42.0`.
- [ ] Master mute: `{"protocolVersion":7,"defaultDevice":{"deviceId":"<default id>","masterMuted":true}}`
      mutes Windows; send `false` to unmute.
- [ ] Session: `{"protocolVersion":7,"defaultDevice":{"deviceId":"<default id>","sessions":[{"id":"<session id>","volume":30.0,"muted":true}]}}`
      sets that app to 30 % and muted in the Volume Mixer. Paste the session id
      exactly as received, including its escaped backslashes.
- [ ] Default device: `{"protocolVersion":7,"defaultDevice":{"deviceId":"<other id>"}}`
      makes that device both "Default Device" and "Default Communication
      Device" in the Sound panel, and the broadcast lists its sessions.
- [ ] An unknown device id changes nothing and re-sends the current state.
- [ ] An unknown session id changes nothing and sends nothing (unknown
      sessions are ignored, see `PROTOCOL.md`).
- [ ] A line that is not JSON closes the connection.
- [ ] Volumes above 100 or below 0 are clamped.
- [ ] Ctrl-C stops the server within a few seconds.

If the old C# server is available, run it with the same devices and compare
its state line with this one: device ids and session ids must be identical,
since the phone apps depend on their format.

## 4. Phone apps

- [ ] The Android and iOS apps discover the server over mDNS, or connect by IP.
- [ ] Sliders and mute buttons on the phone change Windows, and changes made
      on the PC show up on the phone.
- [ ] Switching the default device from the phone works and the session list
      follows.

## 5. Application icons in the Mixer

```powershell
$env:RUST_LOG = "info,pcvolumecontrol::app_icons=debug"
cargo run -p pcvolumecontrol -- --show
```

The log has one `application icon lookup` line per application, with the
executable path and where the icon came from (`Executable`, `ShellLocation`
or `Shell`), or `None` when there is none.

- [ ] Each application row starts with that program's icon (browser, media
      player, Discord, a game), drawn crisply and with transparent corners,
      not on a black or white square.
- [ ] At 150 % or 200 % display scaling the icons are sharp, and moving the
      window between monitors with different scaling keeps them shown.
- [ ] A session whose process could not be opened (an elevated app, or the
      system sounds session if listed) shows a coloured letter tile instead.
- [ ] Store (UWP/MSIX) apps such as Media Player or Spotify from the Store
      show a generic program icon or a letter tile, not an error. Note which.
- [ ] Rows keep the same height with and without an icon, and the window
      stays responsive while the icons appear.

## 6. Mini mixer

The mini mixer was only type-checked for Windows; its placement and focus
handling have not run there.

```powershell
$env:RUST_LOG = "info,pcvolumecontrol=debug"
cargo run -p pcvolumecontrol -- --mock-audio
```

The log shows each tray click with the icon's rectangle, the anchoring
(icon, taskbar edge, work area, monitor scale), `shown at` with the logical
position, `focused` and `unfocused` with the time since opening, and each
close with its reason and whether the window had focus.

Opening and closing:

- [ ] A left click on the tray icon opens it; another left click closes it,
      and does not close and immediately reopen it.
- [ ] A middle click toggles it the same way.
- [ ] Mixer in the tray's right-click menu opens it (the menu taking focus
      closes an open one first).
- [ ] A double click on the tray icon opens the full window and leaves the
      mini mixer closed, without it flashing open again afterwards.
- [ ] It gets keyboard focus when it opens (the log has `focused` within a
      few hundred ms); Escape closes it.
- [ ] Clicking the desktop, another window, the taskbar and the Start
      button each close it.
- [ ] Its open button opens the full window and closes the mini mixer; its
      close button closes it. Both windows can be open together, and a
      change in one shows in the other.
- [ ] It has no taskbar button (note whether it shows in Alt+Tab), stays
      above other windows, and has rounded corners on Windows 11 (square on
      Windows 10), with no black or white fringe at the corners.

Placement (try each, the scale factors where you can):

- [ ] Taskbar at the bottom: it opens just above the taskbar, centred on the
      icon, never past the screen's right edge (icon near the corner).
- [ ] Taskbar at the top, left and right (Windows 10, or Windows 11 with a
      tool such as ExplorerPatcher): it opens beside the taskbar, next to the
      icon, inside the screen.
- [ ] The icon in the overflow flyout (the `^` area): it opens near the
      flyout, inside the screen.
- [ ] 100 %, 150 % and 200 % scaling: same spot relative to the icon, and
      the content is not blurry or cut off.
- [ ] Two monitors with different scaling and the taskbar (with tray) on the
      secondary one: it opens on that monitor, beside the icon, at that
      monitor's size.
- [ ] Opened from the menu: bottom right of the primary monitor's work
      area.
- [ ] An application starting or stopping playback while it is open: it
      grows or shrinks and stays against the taskbar.

## Known risk areas

These could not be checked without Windows and are the most likely to need
fixes:

- **`IPolicyConfig`** (default device switching) is undocumented and declared
  by hand in `crates/pcvc-audio/src/windows/policy_config.rs`. A wrong vtable
  slot would fail with an error or crash the process when switching devices.
- **Missed notifications.** If an event in section 2 never arrives, the state
  goes stale until something else refreshes it. Session creation is the most
  fragile: Core Audio needs `GetSessionEnumerator` before it delivers
  session-created notifications.
- **Sessions of exited apps**: Core Audio keeps them as inactive, so the
  backend checks every 2 s whether a session's process has exited and drops
  it, as the C# server effectively did; it comes back only if it plays again.
  Sessions spanning several processes, or whose process could not be opened,
  stay until Windows expires them. A leftover session whose process id has
  been reused by another process at start-up is shown under that process's
  name.
- **Session names**: elevated or protected processes may refuse
  `OpenProcess`, which makes a session without a display name show as
  `Unknown`.
- **Shutdown**: dropping the backend waits up to 5 s for the Core Audio thread
  and then detaches it with a warning; a warning there means a COM call hung.
- **Icon pixels**: the icon is read with `GetIconInfo` and `GetDIBits` and its
  alpha is taken as straight (not premultiplied); icons without alpha use
  their mask. Wrong colours, dark fringes or opaque backgrounds point at
  `crates/pcvc-app/src/app_icons/windows.rs`.
- **Tray timing** (once the GUI is wired up): the tray icon must be created
  after the GUI's event loop is running. The audio backend runs on its own
  MTA thread and does not depend on the GUI thread's COM apartment.
- **Mini mixer placement**: the window is created hidden, moved, then
  shown; the move is given in logical pixels at the window's own scale,
  which winit converts back. On mixed-scale monitors a wrong spot, or a
  window that jumps after showing, points at `placement::Anchoring::position`
  and `App::mini_rescaled` in `crates/pcvc-app/src/app/mini.rs`.
- **Mini mixer focus**: Windows' foreground lock may keep the new window
  from taking focus; then clicking outside does not close it (the log says
  `had focus: false` when it closes).
