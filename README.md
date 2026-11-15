# PcVolumeControlServer

The PC side of [PcVolumeControl](https://github.com/PcVolumeControl), rewritten
in Rust. It owns the audio stack, serves the iOS and Android apps over TCP
(protocol v7, compatible with the existing apps), and advertises itself over
mDNS. It replaces the C# `PcVolumeControlWindows` server.

Status: the protocol, server, desktop app and a mock audio backend are in
place. Windows (Core Audio) and Linux (PulseAudio) backends come next; until
then the app runs on the mock backend.

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
install -Dm755 pcvolumecontrol ~/.local/bin/pcvolumecontrol
install -Dm644 pcvolumecontrol.desktop ~/.local/share/applications/pcvolumecontrol.desktop
install -Dm644 pcvolumecontrol.svg ~/.local/share/icons/hicolor/scalable/apps/pcvolumecontrol.svg
```

It needs the PulseAudio client library (`libpulse.so.0`, also provided on
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
| `--mock-audio` | Use the scripted mock backend instead of the system audio backend (currently the mock either way). |
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

## Try it with a phone

Run the desktop app above, or the server alone without a GUI:

```sh
cargo run -p pcvc-server --example mock_server
```

This listens on `0.0.0.0:3000` with a scripted set of devices and sessions and
advertises `_pcvolumecontrol._tcp`. Point the iOS or Android app at this
machine (or let it discover the server). Options: `--port N`, `--no-mdns`;
logging follows `RUST_LOG`.

## Crates

| Crate | Purpose |
| --- | --- |
| `pcvc-protocol` | Wire models, line framing, and the pure rule that turns a client update into backend commands. Depends only on serde. |
| `pcvc-audio` | `AudioBackend` trait, domain types, `AudioEvent`, and `MockBackend`. Platform backends will live here behind `cfg`. |
| `pcvc-server` | Tokio TCP server, coalescing state broadcaster, mDNS advertiser, and the `ServerHandle` API for front ends. |
| `pcvc-app` | The `pcvolumecontrol` binary: iced daemon, tray icon, Mixer/Status/Settings window, config, start on login, single instance. See [its README](crates/pcvc-app/README.md). |

The protocol is specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

MIT, see [LICENSE](LICENSE).
