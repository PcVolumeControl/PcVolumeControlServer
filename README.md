# PcVolumeControlServer

The PC side of [PcVolumeControl](https://github.com/PcVolumeControl), rewritten
in Rust. It owns the audio stack, serves the iOS and Android apps over TCP
(protocol v7, compatible with the existing apps), and advertises itself over
mDNS. It replaces the C# `PcVolumeControlWindows` server.

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

### Application icons

Each row in the Mixer shows the application's icon. Lookups run in the
background and only while the window is open; a row shows an empty tile
until its lookup finishes, and a tile with the first letter of the
application's name when nothing was found. Icons are only shown locally;
the phones get the same state as before. Nothing is fetched from the
network.

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

## Try it with a phone

Run the desktop app above, or the server alone without a GUI:

```sh
cargo run -p pcvc-server --example mock_server
```

This listens on `0.0.0.0:3000` with a scripted set of devices and sessions and
advertises `_pcvolumecontrol._tcp`. Point the iOS or Android app at this
machine (or let it discover the server). Options: `--port N`, `--no-mdns`;
logging follows `RUST_LOG`.

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

It takes the same options as `mock_server`. `cargo run -p pcvc-audio
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
| `pcvc-protocol` | Wire models, line framing, and the pure rule that turns a client update into backend commands. Depends only on serde. |
| `pcvc-audio` | `AudioBackend` trait, domain types, `AudioEvent`, `MockBackend`, and the platform backends behind `cfg` (`PulseBackend` on Linux, `WindowsBackend` on Windows). |
| `pcvc-server` | Tokio TCP server, coalescing state broadcaster, mDNS advertiser, and the `ServerHandle` API for front ends. |
| `pcvc-app` | The `pcvolumecontrol` binary: iced daemon, tray icon, Mixer/Status/Settings window, config, start on login, single instance. See [its README](crates/pcvc-app/README.md). |

The protocol is specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

MIT, see [LICENSE](LICENSE).
