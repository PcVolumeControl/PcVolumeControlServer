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
| Close that app completely | `SessionRemoved { id }` (can take a few seconds) |
| Switch the default device in the tray or Sound panel | `DefaultDeviceChanged`, new device's sessions |
| Plug in or unplug a USB or Bluetooth device | `DevicesChanged` |
| Disable and re-enable a device in the Sound panel | `DevicesChanged`; if it is the default, master and session changes still arrive afterwards |
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
- [ ] An unknown device id or session id changes nothing and re-sends the
      current state.
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
- **Sessions of exited apps** stay listed while Windows keeps them inactive,
  until they expire. The C# server hid sessions whose process had exited.
- **Session names**: elevated or protected processes may refuse
  `OpenProcess`, which makes a session without a display name show as
  `Unknown`.
- **Shutdown**: dropping the backend waits up to 5 s for the Core Audio thread
  and then detaches it with a warning; a warning there means a COM call hung.
- **Tray timing** (once the GUI is wired up): the tray icon must be created
  after the GUI's event loop is running. The audio backend runs on its own
  MTA thread and does not depend on the GUI thread's COM apartment.
