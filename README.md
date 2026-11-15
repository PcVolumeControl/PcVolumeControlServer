# PcVolumeControlServer

The PC side of [PcVolumeControl](https://github.com/PcVolumeControl), rewritten
in Rust. It owns the audio stack, serves the iOS and Android apps over TCP
(protocol v7, compatible with the existing apps), and advertises itself over
mDNS. It replaces the C# `PcVolumeControlWindows` server.

Status: the protocol, server, a mock audio backend and a Windows (Core Audio)
backend, not yet run on Windows, are in place. The Linux (PulseAudio) backend
and the desktop GUI come next.

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
| `pcvc-audio` | `AudioBackend` trait, domain types, `AudioEvent`, and `MockBackend`. Platform backends will live here behind `cfg`. |
| `pcvc-server` | Tokio TCP server, coalescing state broadcaster, mDNS advertiser, and the `ServerHandle` API for front ends. |

The protocol is specified in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

MIT
