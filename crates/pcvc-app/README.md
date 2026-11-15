# pcvc-app

The `pcvolumecontrol` desktop host: an [iced](https://iced.rs) daemon with a
tray icon, the server from `pcvc-server`, and one window with Mixer, Status
and Settings pages. User-facing documentation is in the repository README.

## Message flow

Everything that happens outside iced reaches `App::update` through one
channel, the inbox (`inbox.rs`), drained by a single subscription:

```
tray click / menu  --tray-icon event handlers--+
server controller  --ServerUpdate--------------+
audio watcher      --AudioState----------------+--> Inbox --> Subscription --> update
second launch      --ShowRequested-------------+
Ctrl-C / SIGTERM   --QuitRequested-------------+
```

- **Tray** (`tray.rs`): `tray-icon`'s global event handlers turn a left
  click and menu choices into `Inbound::Tray(TrayAction)`. After every
  update the app computes a `TrayModel` (icon variant, tooltip, menu labels
  and check state) and hands it to the tray, which applies what changed. On
  Linux the tray (ksni over D-Bus) lives on its own thread, because ksni
  blocks on a tokio runtime of its own and `update` already runs inside
  iced's. On Windows the icon is created on the UI thread from the first
  `update`, after the event loop has started, as Win32 requires.
- **Server** (`server_ctl.rs`): a tokio task owns the `ServerHandle` and
  serialises start, stop and restart requests. It forwards status, client
  list, mDNS state and server events. Quit asks it to shut down, which
  stops the server (mDNS sends its goodbye), and exits once it reports
  `ShutDown`.
- **Audio** (`audio.rs`): a watcher task subscribes to the backend's events
  and sends a fresh snapshot after each burst of changes, whether or not the
  server runs, so the Mixer is always live. Mixer commands go to the backend
  on a dedicated thread, in order (`CommandSender`).
- **Single instance** (`instance.rs`): the first launch listens on a local
  socket; a later launch sends `show` and exits.

Widget messages (`Message::Mixer`, `Message::Settings`, navigation) come
from the view as usual.

## Application icons

`app_icons::IconWorker` resolves icons on its own thread, one at a time, and
posts each result to the inbox (`Inbound::AppIcon`). The app requests the
icons of the shown sessions after each audio snapshot and when the window's
scale factor becomes known or changes; `IconCache` keeps results and misses
(up to 256 keys) so each application is looked up once. The resolver is
injected through `Boot`, which lets the view tests use `NoIcons` and deliver
results by hand. The key holds everything the lookup reads (name, hints,
pixel size, and on Linux the process id), so equal keys always resolve
alike.

The same resolver instance backs `icon_provider::ResolverIcons`, the
server's `IconProvider` for phone clients, so both share its desktop entry
index and icon theme listings. `main` builds the provider (unless
`serve_icons = false` in the config, or there is no resolver for the
platform) and hands it to `ServerController`, which gives it to every
server start and restart. The provider resolves each application once at
64 px on its own thread, names the icon by the SHA-256 of those pixels,
and renders other sizes on request; see the module documentation.

## Mixer

Slider drags send at most one command per 30 ms per control, always send
the final value (on release, or from a scheduled wake-up), and keep showing
the user's value until the backend reports it back or 300 ms pass
(`mixer.rs`). Mute toggles are shown immediately.

## Where things live

| Module | Purpose |
| --- | --- |
| `app/` | State, `update`, subscriptions and the three views |
| `audio.rs` | `create_backend` (platform backends plug in here), watcher, command thread |
| `config.rs`, `paths.rs` | TOML settings and per-user directories |
| `autostart.rs` | Start on login via `auto-launch` |
| `logging.rs` | stderr, rolling file and the ring buffer behind the Status log |
| `style.rs` | Spacing, sizes and widget styles over iced's built-in themes |
| `icons.rs` | Embedded SVG icons, rasterised with resvg |
| `app_icons/` | Application icons for the Mixer: lookup per platform, rasterising, the result cache and the fallback tile |
| `icon_provider/` | The same icons for phone clients: the server's `IconProvider`, content-derived ids, rendering per requested size |
