# PcVolumeControlServer

The PC side of [PcVolumeControl](https://github.com/PcVolumeControl), rewritten
in Rust. It owns the audio stack, serves the iOS and Android apps over TCP
(protocol v7, compatible with the existing apps), and advertises itself over
mDNS. It replaces the C# `PcVolumeControlWindows` server.

Status: the protocol, server, a mock audio backend and the Linux
(PulseAudio) backend are in place. The Windows (Core Audio) backend and the
desktop GUI come next.

## Build and test

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The mDNS test needs working multicast and is ignored by default:

```sh
cargo test -p pcvc-server --test mdns -- --ignored
```

## Try it with a phone

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

## Crates

| Crate | Purpose |
| --- | --- |
| `pcvc-protocol` | Wire models, line framing, and the pure rule that turns a client update into backend commands. Depends only on serde. |
| `pcvc-audio` | `AudioBackend` trait, domain types, `AudioEvent`, `MockBackend`, and the platform backends behind `cfg` (`PulseBackend` on Linux). |
| `pcvc-server` | Tokio TCP server, coalescing state broadcaster, mDNS advertiser, and the `ServerHandle` API for front ends. |

The protocol is specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

MIT
