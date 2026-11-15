# pcvc-app

The `pcvolumecontrol` desktop host: an [iced](https://iced.rs) daemon with a
tray icon, the server from `pcvc-server`, a window with Mixer, Status and
Settings pages, and a mini mixer window opened from the tray. User-facing
documentation is in the repository README.

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

- **Tray** (`tray.rs`): `tray-icon`'s global event handlers turn left,
  middle and double clicks (with the icon's rectangle or the click's
  position) and menu choices into `Inbound::Tray(TrayAction)`. After every
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
`serve_icons = false` in the config, there is no resolver for the platform,
or its lookup thread cannot start) and hands it to `ServerController`, which
gives it to every server start and restart; `serve_details` reaches the
server the same way. The provider looks each
application up at 64 px on its own thread (misses again after two minutes,
as `IconCache` decides) and names the icon by the SHA-256 of those pixels.
Each render at another size first resolves the application at 64 px again
to check that it still draws the id; see the module documentation.

## Mixer

Slider drags send at most one command per 30 ms per control, always send
the final value (on release, or from a scheduled wake-up), and keep showing
the user's value until the backend reports it back or 300 ms pass
(`mixer.rs`). Mute toggles are shown immediately.

## Mini mixer

A second window (`app/mini.rs`) with the Mixer's device picker, master row
and application rows. Both views build their rows with the same functions
in `app/mixer.rs` and the same `Mixer` model, at a `style::Density`
(`REGULAR` for the page, `COMPACT` for the mini mixer, whose rows have a
fixed height so `mini::height` can size the window before it is drawn).
Audio snapshots, icon results and scale factors reach both windows; icons
are rendered at the largest scale factor of the open windows.

`mini::MiniWindow` decides when it opens and closes, from events carrying
the time (tray toggle, double click, opened, created, focus, unfocus,
Escape, close, closed), so the rules are tested with a fake clock:

- an unfocus is ignored until the window has had focus, and within
  `OPEN_GRACE` of opening (it asks for focus again once instead);
- a click on the tray icon within `TOGGLE_DEBOUNCE` of a close on losing
  focus is the click that took the focus, so it leaves the window closed
  (the menu's Mixer item is exempt: opening the menu took the focus);
- clicks within `DOUBLE_CLICK_QUIET` of a double click belong to it;
- a close before iced has created the window waits until it exists, since
  iced drops window actions for unknown windows.

`placement::anchoring` finds what to place it against: the icon's rectangle
and Win32 monitor info on Windows; the click or pointer and RandR monitors
on X11; nothing on Wayland. `placement::place` is the pure geometry, in
physical pixels. iced takes logical positions and the platform converts
them at the window's scale, known only once the window exists, so on
Windows the window opens hidden and is moved and shown once created; on X11,
where window managers place a window as it maps and only honour the
position it was created with, it opens at the position for the last known
scale and is moved again if its own differs. Debug logs
(`RUST_LOG=pcvolumecontrol=debug`) record tray clicks, opening, creation,
placement, focus, unfocus and each close with its reason.

## Updates

`update::Worker` runs checks and installs on its own thread and posts
`Inbound::Update` events; `update::Updater` is the state the Status page and
tray show, and `app/updates.rs` connects the two and schedules automatic
checks (`update::schedule`, polled every minute while they are on).
`update::http` allows only HTTPS to GitHub's hosts, follows redirects by
hand, and caps sizes and times; tests swap in a loopback policy and
endpoints.

Installing (Windows, installed copies of official builds) goes: download
into a new directory under the local data dir, verify against `SHA256SUMS`
(and its minisign signature when `assets/update-key.pub` has a key) through
a handle that denies writes, `Event::Verified`, the app steps aside (stops
the server, saves the config, closes its windows and drops the tray icon)
and answers `Worker::proceed` once the server has stopped (the worker
starts anyway after 15 s), the worker starts the installer and waits for
it. The installer stops this process and
restarts the new version because it found it running; if the installer
ends instead, `Event::InstallFailed` brings the app back.
`build.rs` sets `cfg(pcvc_official_build)` when the release workflow sets
`PCVC_OFFICIAL_BUILD=1`.

## Where things live

| Module | Purpose |
| --- | --- |
| `app/` | State, `update`, subscriptions, the three pages and the mini mixer view |
| `mini.rs` | When the mini mixer opens and closes, and its height |
| `placement.rs` | Where the mini mixer opens, per platform |
| `audio.rs` | `create_backend` (platform backends plug in here), watcher, command thread |
| `config.rs`, `paths.rs` | TOML settings and per-user directories |
| `autostart.rs` | Start on login via `auto-launch` |
| `logging.rs` | stderr, rolling file and the ring buffer behind the Status log |
| `style.rs` | Spacing, sizes and widget styles over iced's built-in themes |
| `icons.rs` | Embedded SVG icons, rasterised with resvg |
| `app_icons/` | Application icons for the Mixer: lookup per platform, rasterising, the result cache and the fallback tile |
| `icon_provider/` | The same icons for phone clients: the server's `IconProvider`, content-derived ids, rendering per requested size |
| `update/` | Release checks, verified downloads, the installer, install kind, schedule and update state |
