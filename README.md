# PcVolumeControlServer

The PC side of [PcVolumeControl](https://github.com/PcVolumeControl), rewritten
in Rust. It owns the audio stack, serves the iOS and Android apps over TCP
(protocol v7, compatible with the existing apps), and advertises itself over
mDNS. It replaces the C# `PcVolumeControlWindows` server. The server library
also implements the optional "v8" extensions (app icons and session
details), which keep the wire version at 7 so the shipped apps are
unaffected; the desktop app serves real application icons and stream titles
with them, and the mock server below serves demo icons and titles for client
development.

Status: the protocol, server, desktop app, the Linux (PulseAudio) backend and
a Windows (Core Audio) backend are in place. The Windows backend and the
Windows build of the app have not been run on Windows yet.

## Install

Packages for each release are on the
[releases page](https://github.com/PcVolumeControl/PcVolumeControlServer/releases).

**Windows 10 and 11 (x64).** Run `PcVolumeControl-<version>-setup.exe`. It
installs for the current user (no administrator rights needed) unless you
choose all users, adds a Start menu entry, and has an uninstaller in
Settings > Apps. The portable `pcvolumecontrol-<version>-windows-x86_64.zip`
runs from any folder. The builds are not code signed, so SmartScreen may ask
for confirmation (More info > Run anyway). Uninstall the old C#
PcVolumeControl first: both listen on the same port.

The server listens on TCP port 3000. On first run Windows Firewall asks
whether to allow PcVolumeControl; allow it on private networks or phones
cannot connect. To change the answer later, open Windows Security > Firewall
& network protection > Allow an app through firewall.

**Debian, Ubuntu and derivatives (x86-64).** Install the `.deb`, which pulls
in its dependencies:

```sh
sudo apt install ./pcvolumecontrol_<version>-1_amd64.deb
```

**Other Linux (x86-64, glibc 2.35 or newer).** Unpack
`pcvolumecontrol-<version>-linux-x86_64.tar.gz` and copy the files into place,
for example for the current user:

```sh
tar xzf pcvolumecontrol-<version>-linux-x86_64.tar.gz
cd pcvolumecontrol-<version>-linux-x86_64
install -Dm755 pcvolumecontrol ~/.local/bin/pcvolumecontrol
install -Dm644 pcvolumecontrol.desktop ~/.local/share/applications/pcvolumecontrol.desktop
install -Dm644 pcvolumecontrol.svg ~/.local/share/icons/hicolor/scalable/apps/pcvolumecontrol.svg
```

The desktop entry runs `pcvolumecontrol` from `PATH`; if `~/.local/bin` is not
on it, log out and back in after creating it, or install the binary to
`/usr/local/bin` instead. It needs the PulseAudio client library (`libpulse.so.0`, also provided on
PipeWire systems) and libxkbcommon. If a firewall is active, allow TCP port
3000 and mDNS (UDP 5353).

Releases are made as described in [docs/RELEASING.md](docs/RELEASING.md).

## Build and test

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The app's view tests render headlessly in software (`ICED_TEST_BACKEND` is set
in `.cargo/config.toml`). To look at every page in both themes:

```sh
PCVC_SNAPSHOT_DIR=/some/dir cargo test -p pcvolumecontrol render_pages -- --ignored
```

On Linux, `render_real_mixer` renders the Mixer page (`real-mixer-*.png`)
with the streams playing right now and the icons this machine resolves for
them, and prints each lookup. It only reads the sound server:

```sh
PCVC_SNAPSHOT_DIR=/some/dir cargo test -p pcvolumecontrol render_real_mixer -- --ignored --nocapture
```

The mDNS test needs working multicast and is ignored by default:

```sh
cargo test -p pcvc-server --test mdns -- --ignored
```

## Desktop app

```sh
cargo run -p pcvolumecontrol -- --show --mock-audio
```

`pcvolumecontrol` lives in the system tray and starts the server on launch.
The window has three pages: Mixer (default output device, master volume and
one slider per application; the default page), Status (server state,
addresses to connect to, mDNS, connected clients, recent log) and Settings.
Closing the window hides it; Quit in the tray menu stops the server and
exits.

| Flag | Effect |
| --- | --- |
| `--show` | Open the window on launch. Launching again while running always shows the running instance's window and exits. |
| `--mock-audio` | Use the scripted mock backend instead of the system audio backend (PulseAudio on Linux, Core Audio on Windows). |
| `--version`, `--help` | Print and exit. |

Logging follows `RUST_LOG` (default `info`). Files:

| | Linux | Windows |
| --- | --- | --- |
| Settings | `~/.config/pcvolumecontrol/config.toml` | `%APPDATA%\PcVolumeControl\PcVolumeControl\config\config.toml` |
| Logs (daily, 7 kept) | `~/.local/share/pcvolumecontrol/logs/` | `%LOCALAPPDATA%\PcVolumeControl\PcVolumeControl\data\logs\` |
| Start on login | `~/.config/autostart/PcVolumeControl.desktop` | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `PcVolumeControl` |

An unreadable config file is renamed to `config.toml.corrupt-<time>` and the
defaults are used.

On Linux the tray icon is a StatusNotifierItem (via `ksni`, no GTK). KDE,
Ubuntu's GNOME and most other desktops show it; stock GNOME needs the
[AppIndicator and KStatusNotifierItem Support](https://extensions.gnome.org/extension/615/appindicator-support/)
extension. Without a tray the window opens at launch and closing it quits.
`packaging/linux/` has the desktop entry and icon for packages.

### Mini mixer

A small window with the output device, the master volume and one row per
application, the same controls as the Mixer page, for quick changes from
the tray. It sits on top of other windows, grows with the number of
applications (then scrolls), and has buttons to open the full window and to
close it.

| To | Do |
| --- | --- |
| Open or close it | Left click the tray icon, middle click it, or choose Mixer in the tray menu |
| Close it | Click anywhere outside it, press Escape, or use its close button |
| Open the full window | Its open button, Open in the tray menu, a double click on the tray icon (Windows), or launching the app again |

Both windows can be open at once and stay live. Clicking outside reaches
the app only as the mini mixer losing focus, so a window that never had
focus (focus stealing prevention) stays open until you click into it, press
Escape, use the tray or its close button. Opening the tray menu takes focus
too, so it closes the mini mixer; choosing Mixer there opens it again.

Where it opens depends on the platform:

- **Windows**: beside the tray icon, on the side away from the taskbar,
  inside the monitor's work area; from the tray menu, at the bottom right of
  the primary monitor's work area.
- **Linux on X11**: beside the click, or beside the pointer when the tray
  host gives no position (as from the menu), inside the monitor.
- **Linux on Wayland**: wherever the compositor puts it; Wayland does not
  let applications position their windows.

On GNOME the AppIndicator extension opens the tray menu on a left click, so
use the menu's Mixer item or a middle click on the icon there.

### Application icons

Each row in the Mixer shows the application's icon. The Mixer's lookups run
in the background and only while the window is open; a row shows an empty
tile until its lookup finishes, and a tile with the first letter of the
application's name when nothing was found. Phone clients that speak the
icon extension get the icons that were found (see below). Nothing is
fetched from the network.

On Linux the app tries, in order:

1. the icon the stream declares (`application.icon_name`);
2. the desktop entry whose id is the stream's application id
   (`application.id`, or the Flatpak app id);
3. for Steam games (Proton included), `steam_icon_<app id>` with the
   `SteamAppId` from the process environment, the icon Steam's shortcuts use;
4. the desktop entry for the process binary (by id, `StartupWMClass` or the
   program its `Exec` line runs), then for the application name.

Desktop entries are read from `$XDG_DATA_HOME/applications`, each
`$XDG_DATA_DIRS/applications`, and the Flatpak and Snap export directories.
Icon names are resolved in the current icon theme (from `gsettings`, the GTK
`settings.ini` or `kdeglobals`, depending on the desktop), the themes it
inherits, `hicolor`, then `/usr/share/pixmaps`, preferring SVG and the
smallest size not below what the window needs at its scale factor. PNG, SVG
and SVGZ are supported; XPM icons are skipped. Desktop entries and themes
are re-read after a minute, and a missing icon is looked up again after two
minutes, so newly installed applications get theirs.

Limits: a stream whose properties name neither an icon, an application id,
a binary nor a name that matches a desktop entry gets a letter tile (some
games and command line players). Sandboxed clients report their binary as
seen inside the sandbox. The Steam lookup needs the game's environment to be
readable, which it is for the user's own processes.

On Windows the icon comes from the session's executable: its own icon at the
needed size, else the icon Explorer shows for it. Packaged (Store) apps keep
their logos outside the executable, so they get a generic program icon or a
letter tile. Sessions whose process cannot be opened get a letter tile.

### Icons for phone clients

The server offers the same icons to clients that speak the v8 icon
extension ([docs/PROTOCOL.md](docs/PROTOCOL.md)); the shipped apps ignore
it. Sessions of an application the server has not looked up yet appear
without an `iconId` and gain one in a later state once the lookup finishes,
whether or not the window is open; sessions with no icon found have no
`iconId`, and the phone shows its own placeholder. The id is the first 32
hex digits of the SHA-256 of the icon drawn at 64 px (its width and height
as big-endian 32-bit integers, then its RGBA pixels), so it stays the same
across restarts while the icon file and the renderer do not change, and
applications with the same icon share it. Icons are sent at the size asked
for: SVG icons are drawn at that size, raster icons are scaled up at most to
twice their own size (or 16 px, the protocol's minimum), and the reply's
`size` gives the edge actually sent. On Windows the shell scales an
executable's icons itself, so that limit only applies to its fallback icon.
An application keeps its id while the app runs, so an icon changed in the
meantime (a new icon theme) reaches phones after a restart.

### Session details for phone clients

Sessions of the same application, such as Firefox's one stream per tab, all
have the same name. The server also sends each session's `detail`, a short
label that tells them apart ([docs/PROTOCOL.md](docs/PROTOCOL.md#session-details)),
to clients that show it; the shipped apps ignore it. On Linux it is the
stream's title (`media.name`, else `media.title`), which for Firefox is the
tab's title; titles equal to the name and generic ones such as `Playback`
are left out. Windows sends none: Core Audio has nothing per session beyond
the name. Any client on the network can read the titles, so turn them off
if that matters.

To turn off icons, details or both (plain v7), add these lines near the top
of `config.toml`, before any `[section]` such as `[window]` (a line below one
belongs to that section and is ignored):

```toml
serve_icons = false
serve_details = false
```

Then quit from the tray menu and start the app again; the settings are read
at startup. They have no control in the Settings page, and saving settings
there keeps them.

## Try it with a phone

Run the desktop app above, or the server alone without a GUI:

```sh
cargo run -p pcvc-server --example mock_server
```

This listens on `0.0.0.0:3000` with a scripted set of devices and sessions and
advertises `_pcvolumecontrol._tcp`. Point the iOS or Android app at this
machine (or let it discover the server). Options: `--port N`, `--no-mdns`,
`--no-icons`, `--no-detail`; logging follows `RUST_LOG`.

### Building a client against session details

The mock server serves the v8 `detail` feature by default (`--no-detail`
turns it off). Besides the demo's own Firefox session, which has no detail,
the speakers have two more sessions named `Firefox` with a tab title as
their detail; one of them moves on to another title every five seconds.
With both `--no-detail` and `--no-icons` it is a plain v7 server.

### Building a client against the icon extension

The mock server serves the v8 `icons` feature by default (`--no-icons` turns
it off). Its demo provider draws a coloured
rounded square per app at whatever size is asked for, and resolves icon ids
in the background like a real one: a session it has not seen before appears
without an `iconId` and gains it in a state about half a second later.
Sessions on the default device at startup are resolved before you can
connect; switch the default device to the headphones or the headset to see
the delayed path. See
[docs/PROTOCOL.md](docs/PROTOCOL.md#protocol-v8-extensions) for the format.

To poke at it by hand, run it on a spare port and connect with `nc`:

```sh
cargo run -p pcvc-server --example mock_server -- --port 3001 --no-mdns
nc localhost 3001
```

The first line is a full state with `"features":["icons","detail"]` and an
`iconId` on every session. Paste a request using one of them and the reply lines come
back, one per id:

```json
{"protocolVersion":7,"iconRequest":{"ids":["<iconId>","nosuchid"],"size":96}}
```

A few lines of Python do the whole round trip and save the PNGs:

```python
import base64, json, socket

s = socket.create_connection(("localhost", 3001))
lines = s.makefile("r", encoding="utf-8")
state = json.loads(lines.readline())
assert "icons" in state.get("features", [])
while not all("iconId" in x for x in state["defaultDevice"]["sessions"]):
    state = json.loads(lines.readline())
# Sessions can share an icon; the server answers each distinct id once.
ids = list(dict.fromkeys(x["iconId"] for x in state["defaultDevice"]["sessions"]))

request = {"protocolVersion": 7, "iconRequest": {"ids": ids, "size": 96}}
s.sendall((json.dumps(request) + "\n").encode())
answered = 0
while answered < len(ids):
    line = json.loads(lines.readline())
    if "defaultDevice" in line:  # a full state; icon replies have "icon"
        continue
    icon = line["icon"]
    answered += 1
    if "error" in icon:
        print(icon["id"], icon["error"])
        continue
    with open(f"{icon['id']}.png", "wb") as f:
        f.write(base64.b64decode(icon["data"]))
    print(icon["id"], f"{icon['size']}px")
```

A second `nc` connected at the same time only ever sees full states: icon
replies go to the client that asked.

## Linux

The Linux backend talks to the PulseAudio protocol, so it works with
PulseAudio itself and with PipeWire through `pipewire-pulse` (the default on
current distributions).

- Build: `libpulse` development files and `pkg-config` (Debian/Ubuntu:
  `libpulse-dev`, Fedora: `pulseaudio-libs-devel`, Arch: `libpulse`).
- Run: a PulseAudio or `pipewire-pulse` server in the user session.

Sinks are the devices, the default sink is the default device, and the
application streams playing to it are its sessions (event sounds and
streams created by server modules are left out). Changing the default device
also moves the streams on the old default to the new one, as on Windows.

To control this machine from a phone before the desktop app is ready:

```sh
cargo run -p pcvc-server --example pulse_server
```

It takes `--port N`, `--no-mdns` and `--no-detail`. `cargo run -p pcvc-audio
--example pulse_dump` prints what the backend sees and the events it emits.

The tests against the running sound server are ignored by default. They only
change the volume of a silent stream and a temporary null sink they create,
but they make the null sink the default for up to 1.5 seconds (a watchdog
thread restores the default after that), which moves the streams on the
real default sink there and back (see
`crates/pcvc-audio/tests/pulse_live.rs`):

```sh
cargo test -p pcvc-audio --test pulse_live -- --ignored --test-threads=1
```

## Windows audio backend

`pcvc_audio::WindowsBackend` drives Windows Core Audio: active playback
devices, the default device's master volume and sessions, default-device
switching, and change notifications. On Windows:

```powershell
cargo run -p pcvc-audio --example windows_dump      # print the state, then every change
cargo run -p pcvc-server --example windows_server   # serve the phones from the real mixer
```

It has not been run on Windows yet; [docs/WINDOWS-TESTING.md](docs/WINDOWS-TESTING.md)
is the checklist to verify it.

## Crates

| Crate | Purpose |
| --- | --- |
| `pcvc-protocol` | Wire models (v7 and the v8 icon extension), line framing, and the pure rule that turns a client update into backend commands. Depends only on serde, serde_json and base64. |
| `pcvc-audio` | `AudioBackend` trait, domain types, `AudioEvent`, `MockBackend`, and the platform backends behind `cfg` (`PulseBackend` on Linux, `WindowsBackend` on Windows). |
| `pcvc-server` | Tokio TCP server, coalescing state broadcaster, mDNS advertiser, and the `ServerHandle` API for front ends. |
| `pcvc-app` | The `pcvolumecontrol` binary: iced daemon, tray icon, Mixer/Status/Settings window, config, start on login, single instance. See [its README](crates/pcvc-app/README.md). |

The protocol is specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

MIT, see [LICENSE](LICENSE).
