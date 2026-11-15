//! Headless view tests: render views with `iced_test`, interact with them
//! and feed the resulting messages back through `update`.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use iced_test::{Simulator, simulator};
use pcvc_audio::{AudioBackend, AudioState, MockBackend};
use pcvc_server::ClientInfo;

use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::app_icons::{IconKey, Resolved};
use crate::config::ThemeChoice;
use crate::inbox;
use crate::mini::{Body, height};
use crate::mixer::Target;

pub(super) struct Harness {
    pub(super) app: App,
    backend: Arc<MockBackend>,
    /// Entered around every update, as iced does. Nothing drives it, so the
    /// server and watcher tasks spawned at boot never actually run.
    runtime: tokio::runtime::Runtime,
}

impl Harness {
    pub(super) fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let backend = Arc::new(MockBackend::demo());
        let (inbox, inbox_rx) = inbox::channel();
        let config = Config::default();
        let (mut app, _) = App::boot(Boot {
            config,
            config_path: None,
            show_window: false,
            backend: Arc::clone(&backend) as Arc<dyn AudioBackend>,
            icon_resolver: Arc::new(crate::app_icons::NoIcons),
            server_icons: None,
            server_details: false,
            inbox,
            inbox_rx,
            logs: LogBuffer::new(10),
            update_context: None,
            update_state: None,
        });
        // The real locator would read this machine's display.
        app.mini_locator = None;
        let mut harness = Self {
            app,
            backend,
            runtime,
        };
        harness.refresh_audio();
        harness
    }

    fn refresh_audio(&mut self) {
        let state = self.backend.snapshot().unwrap();
        self.run(vec![Message::Inbound(Inbound::Audio(state))]);
    }

    pub(super) fn run(&mut self, messages: Vec<Message>) {
        let _guard = self.runtime.enter();
        for message in messages {
            let _ = self.app.update(message);
        }
    }

    /// Pretends a window at scale 1 is open, so the Mixer shows icons.
    pub(super) fn open_window(&mut self) -> window::Id {
        let id = window::Id::unique();
        self.app.window = Some(id);
        self.run(vec![Message::ScaleFactor(id, 1.0)]);
        id
    }

    /// Opens the mini mixer with `action`, as the platform creates it at
    /// scale 1.
    fn open_mini_with(&mut self, action: TrayAction) -> window::Id {
        self.run(vec![Message::Inbound(Inbound::Tray(action))]);
        let id = self.app.mini.id().expect("the action opens the mini mixer");
        self.run(vec![Message::Mini(MiniMessage::Created(id, 1.0))]);
        id
    }

    /// Opens the mini mixer with a left click on the tray icon.
    fn open_mini(&mut self) -> window::Id {
        self.open_mini_with(TrayAction::Click(tray_icon::MouseButton::Left, None))
    }

    /// The mini mixer's view at its window size.
    fn mini_ui(&self) -> Simulator<'_, Message> {
        Simulator::with_size(
            iced::Settings::default(),
            Size::new(style::MINI_WIDTH, self.app.mini_height),
            self.app.mini_view(),
        )
    }

    fn sessions(&self) -> Vec<pcvc_audio::Session> {
        self.backend
            .snapshot()
            .unwrap()
            .default_device
            .unwrap()
            .sessions
    }

    /// Delivers an icon lookup result as the icon worker would.
    fn resolve_icon(&mut self, session: &pcvc_audio::Session, icon: Option<image::Handle>) {
        let key = IconKey::new(session, self.app.icon_size().unwrap());
        self.run(vec![Message::Inbound(Inbound::AppIcon(Resolved {
            key,
            icon,
        }))]);
    }

    /// Waits for the command thread to bring the backend into a state.
    fn wait_for(&mut self, done: impl Fn(&AudioState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done(&self.backend.snapshot().unwrap()) {
            assert!(Instant::now() < deadline, "backend never reached the state");
            std::thread::sleep(Duration::from_millis(5));
        }
        self.refresh_audio();
    }
}

#[test]
fn mixer_lists_live_sessions_and_mutes() {
    let mut h = Harness::new();
    let mut ui = simulator(h.app.mixer_view());
    for name in ["Master", "Firefox", "Spotify", "Musique \u{e9}t\u{e9}"] {
        ui.find(name).unwrap();
    }
    // The first "Mute" is the master's.
    ui.click("Mute").unwrap();
    let messages = ui.into_messages().collect();
    h.run(messages);
    h.wait_for(|s| s.default_device.as_ref().unwrap().muted);

    simulator(h.app.mixer_view()).find("Unmute").unwrap();

    // A session added by the backend shows up after the next snapshot.
    let device = h.backend.snapshot().unwrap().default_device.unwrap().id;
    h.backend
        .add_session(
            &device,
            pcvc_audio::Session::new(MockBackend::session_id(&device, "vlc"), "VLC", 30.0),
        )
        .unwrap();
    h.refresh_audio();
    simulator(h.app.mixer_view()).find("VLC").unwrap();
}

fn icon_id(kind: &str, session: &pcvc_audio::Session) -> iced::widget::Id {
    iced::widget::Id::from(format!("app-icon-{kind}:{}", session.id))
}

#[test]
fn mixer_rows_show_icon_fallback_and_placeholder() {
    let mut h = Harness::new();
    h.open_window();
    let sessions = h.sessions();
    let (firefox, spotify, discord) = (&sessions[0], &sessions[1], &sessions[2]);
    let red = image::Handle::from_rgba(1, 1, vec![255, 0, 0, 255]);
    h.resolve_icon(firefox, Some(red));
    h.resolve_icon(spotify, None);

    let mut ui = simulator(h.app.mixer_view());
    let icon = ui.find(icon_id("image", firefox)).unwrap().bounds();
    let tile = ui.find(icon_id("fallback", spotify)).unwrap().bounds();
    let pending = ui.find(icon_id("pending", discord)).unwrap().bounds();
    assert!(ui.find(icon_id("image", spotify)).is_err());
    ui.find("S").unwrap();
    for b in [icon, tile, pending] {
        assert_eq!((b.width, b.height), (style::APP_ICON, style::APP_ICON));
        assert_eq!(b.x, icon.x, "icons share a column");
    }
    // Rows are evenly spaced whatever their icon state.
    assert_eq!(tile.y - icon.y, pending.y - tile.y);
    let names: Vec<f32> = ["Firefox", "Spotify", "Discord"]
        .iter()
        .map(|n| ui.find(*n).unwrap().bounds().x)
        .collect();
    assert!(
        names
            .iter()
            .all(|&x| x == names[0] && x > icon.x + style::APP_ICON)
    );

    // The master row has no icon.
    let master = ui.find("Master").unwrap().bounds();
    assert!(master.y < icon.y);

    // A new scale factor keeps the found icon up while the sharper one is
    // looked up; the miss stays a letter tile.
    drop(ui);
    let id = h.app.window.unwrap();
    h.run(vec![Message::ScaleFactor(id, 2.0)]);
    let mut ui = simulator(h.app.mixer_view());
    ui.find(icon_id("image", firefox)).unwrap();
    ui.find(icon_id("pending", spotify)).unwrap();
}

#[test]
fn mixer_long_names_keep_the_columns() {
    let mut h = Harness::new();
    h.open_window();
    let device = h.backend.snapshot().unwrap().default_device.unwrap().id;
    let long = "A very long application name that does not fit in its column at all";
    h.backend
        .add_session(
            &device,
            pcvc_audio::Session::new(MockBackend::session_id(&device, "long"), long, 31.0),
        )
        .unwrap();
    h.refresh_audio();
    let sessions = h.sessions();
    let long_session = sessions.iter().find(|s| s.name == long).unwrap();
    h.resolve_icon(long_session, None);

    let mut ui = Simulator::with_size(iced::Settings::default(), MIN_SIZE, h.app.mixer_view());
    let tile = ui.find(icon_id("fallback", long_session)).unwrap().bounds();
    assert_eq!(tile.height, style::APP_ICON);
    // The percentages stay in one column: the long name pushes nothing.
    let long_percent = ui.find("31%").unwrap().bounds();
    let spotify_percent = ui.find("65%").unwrap().bounds();
    assert_eq!(long_percent.x, spotify_percent.x);
    assert_eq!(long_percent.width, spotify_percent.width);
    // The name is shortened with an ellipsis and leaves room for the slider.
    let (name, bounds) = ui
        .find(|c: iced_test::selector::Candidate<'_>| match c {
            iced_test::selector::Candidate::Text {
                content, bounds, ..
            } if content.starts_with("A very") => Some((content.to_owned(), bounds)),
            _ => None,
        })
        .unwrap();
    assert!(name.ends_with('\u{2026}'), "{name:?}");
    assert!(bounds.x + bounds.width + 100.0 < long_percent.x);
    let spotify = ui.find("Spotify").unwrap().bounds();
    assert_eq!(bounds.x, spotify.x);
}

#[test]
fn mixer_columns_line_up() {
    let mut h = Harness::new();
    h.open_window();
    for size in [MIN_SIZE, Size::new(1200.0, 700.0)] {
        let mut ui = Simulator::with_size(iced::Settings::default(), size, h.app.mixer_view());
        let output = ui.find("Output device").unwrap().bounds();
        let master = ui.find("Master").unwrap().bounds();
        let master_percent = ui.find("80%").unwrap().bounds();
        let session_percent = ui.find("65%").unwrap().bounds();
        let spotify = ui.find("Spotify").unwrap().bounds();
        assert_eq!(output.x, master.x);
        assert_eq!(master_percent.x, session_percent.x);
        assert_eq!(master_percent.width, session_percent.width);
        // Icons start where the master label does.
        let icon = ui
            .find(icon_id("pending", &h.sessions()[1]))
            .unwrap()
            .bounds();
        assert_eq!(icon.x, master.x);
        assert!(spotify.x > icon.x + style::APP_ICON);
    }
}

#[test]
fn mixer_session_mute_shows_unmute() {
    let mut h = Harness::new();
    let spotify = h.sessions()[1].id.clone();
    h.run(vec![Message::Mixer(MixerMessage::ToggleMute(
        Target::Session(spotify),
    ))]);
    let mut ui = simulator(h.app.mixer_view());
    // One session offers Unmute, the master and the others Mute.
    ui.find("Unmute").unwrap();
    ui.find("Mute").unwrap();
}

#[test]
fn mixer_slide_reaches_backend() {
    let mut h = Harness::new();
    let spotify = h
        .backend
        .snapshot()
        .unwrap()
        .default_device
        .unwrap()
        .sessions[1]
        .id
        .clone();
    let target = Target::Session(spotify.clone());
    h.run(vec![
        Message::Mixer(MixerMessage::Slide(target.clone(), 20.0)),
        Message::Mixer(MixerMessage::Slide(target.clone(), 21.0)),
        Message::Mixer(MixerMessage::Release(target)),
    ]);
    h.wait_for(|s| {
        let session = s.default_device.as_ref().unwrap().sessions[1].clone();
        session.id == spotify && session.volume == 21.0 && !session.muted
    });
    simulator(h.app.mixer_view()).find("21%").unwrap();
}

#[test]
fn status_shows_addresses_and_clients() {
    let mut h = Harness::new();
    let addr: SocketAddr = "0.0.0.0:3000".parse().unwrap();
    h.run(vec![
        Message::Inbound(Inbound::Server(ServerUpdate::Status(
            ServerStatus::Running(addr),
        ))),
        Message::Inbound(Inbound::Server(ServerUpdate::Clients(vec![ClientInfo {
            id: 0,
            addr: "192.168.1.30:50000".parse().unwrap(),
            connected_since: SystemTime::now(),
        }]))),
    ]);
    // Replaces the real interfaces read when the server came up.
    h.app.local_ips = vec![Ipv4Addr::new(192, 168, 1, 20)];
    let mut ui = simulator(h.app.status_view());
    for needle in [
        "Online",
        "1 client connected",
        "192.168.1.20:3000",
        "192.168.1.30:50000",
    ] {
        ui.find(needle).unwrap();
    }
    ui.click("Stop server").unwrap();
    assert!(matches!(
        ui.into_messages().collect::<Vec<_>>().as_slice(),
        [Message::ToggleServer]
    ));
}

#[test]
fn settings_validate_and_save() {
    let mut h = Harness::new();
    h.run(vec![Message::Settings(SettingsMessage::Port("0".into()))]);
    let mut ui = simulator(h.app.settings_view());
    ui.find("Port must be a number from 1 to 65535").unwrap();
    ui.click("Save").unwrap();
    assert_eq!(ui.into_messages().count(), 0, "Save is disabled");

    h.run(vec![
        Message::Settings(SettingsMessage::Port("4x0a00".into())),
        Message::Settings(SettingsMessage::Theme(ThemeChoice::Dark)),
    ]);
    assert_eq!(h.app.settings.port, "4000");
    let mut ui = simulator(h.app.settings_view());
    ui.click("Save").unwrap();
    let messages = ui.into_messages().collect();
    h.run(messages);
    assert_eq!(h.app.config.port, 4000);
    assert_eq!(h.app.config.theme, ThemeChoice::Dark);
    assert_eq!(h.app.notice, Some(Notice::Saved));
    simulator(h.app.settings_view()).find("Saved").unwrap();
}

/// Renders every page in both themes into `PCVC_SNAPSHOT_DIR` for a manual
/// look. Run with `--ignored`. Earlier renders (`mixer-*.png`,
/// `status-*.png`, `settings-*.png`) in the directory are replaced.
#[test]
#[ignore = "writes PNG files for manual inspection"]
fn render_pages() {
    let Ok(dir) = std::env::var("PCVC_SNAPSHOT_DIR") else {
        return;
    };
    let mut h = Harness::new();
    h.run(vec![
        Message::Inbound(Inbound::Server(ServerUpdate::Status(
            ServerStatus::Running("0.0.0.0:3000".parse().unwrap()),
        ))),
        Message::Inbound(Inbound::Server(ServerUpdate::Mdns(
            pcvc_server::MdnsStatus::Registered {
                instance: "desktop-pcvolumecontrol".into(),
            },
        ))),
        Message::Inbound(Inbound::Server(ServerUpdate::Clients(vec![ClientInfo {
            id: 0,
            addr: "192.168.1.30:50000".parse().unwrap(),
            connected_since: SystemTime::now() - Duration::from_secs(185),
        }]))),
    ]);
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(crate::logging::ring_layer(h.app.logs.clone())),
        || {
            tracing::info!(target: "pcvc_server", local_addr = "0.0.0.0:3000", "server listening");
            tracing::info!(target: "pcvc_server::client", addr = "192.168.1.30:50000", "client connected");
        },
    );
    h.app.local_ips = vec![Ipv4Addr::new(192, 168, 1, 20), Ipv4Addr::new(10, 8, 0, 2)];
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        let ours = ["mixer-", "status-", "settings-"]
            .iter()
            .any(|p| name.starts_with(p));
        if ours && name.ends_with(".png") {
            std::fs::remove_file(&path).unwrap();
        }
    }
    use_detail_device(&mut h);
    let id = h.open_window();
    resolve_detail_icons(&mut h);
    let default = WindowSize::default();
    let sizes = [
        ("", Size::new(default.width, default.height)),
        ("-min", MIN_SIZE),
    ];
    for page in [Page::Mixer, Page::Status, Page::Settings] {
        h.app.page = page;
        for (suffix, size) in sizes {
            for (name, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
                let mut ui = iced_test::Simulator::with_size(
                    iced::Settings::default(),
                    size,
                    h.app.view(id),
                );
                let page = format!("{page:?}").to_lowercase();
                let path = format!("{dir}/{page}-{name}{suffix}.png");
                ui.snapshot(&theme).unwrap().matches_image(&path).unwrap();
            }
        }
    }
    // Snapshots are drawn at scale 2; the Mixer at scale 1 too.
    h.app.page = Page::Mixer;
    for (name, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
        let size = Size::new(default.width, default.height);
        let path = format!("{dir}/mixer-{name}-1x.png");
        render_png(h.app.view(id), size, 1.0, &theme, &path);
    }
}

/// Renders the window's Mixer page for the real sound server's current
/// streams, with icons resolved from this machine's desktop entries and
/// themes, into `PCVC_SNAPSHOT_DIR` (`real-mixer-*.png`), the mini mixer
/// too (`real-mini-*.png`), and prints what each stream resolved to. Only
/// reads: no audio state is changed. Run with `--ignored --nocapture`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "reads the real sound server and icon themes"]
fn render_real_mixer() {
    use crate::app_icons::linux::{Dirs, LinuxResolver};

    let Ok(dir) = std::env::var("PCVC_SNAPSHOT_DIR") else {
        return;
    };
    let state = pcvc_audio::PulseBackend::with_details(true)
        .unwrap()
        .snapshot()
        .unwrap();
    let mut h = Harness::new();
    h.run(vec![Message::Inbound(Inbound::Audio(state.clone()))]);
    let id = h.open_window();
    // Renders are drawn at twice the logical size, so icons are too.
    h.run(vec![Message::ScaleFactor(id, 2.0)]);
    let resolver = LinuxResolver::new(Dirs::from_env());
    for session in state.default_device.iter().flat_map(|d| &d.sessions) {
        let key = IconKey::new(session, h.app.icon_size().unwrap());
        // A panicking lookup is a miss, as in the app's icon worker.
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolver.find(&key)))
            .ok()
            .flatten();
        let outcome = match &found {
            Some((path, _, via)) => format!("{via:?} {}", path.display()),
            None => "letter tile".to_owned(),
        };
        println!(
            "{:?} ({:?}) {:?} -> {outcome}",
            session.name, session.detail, session.app
        );
        let icon = found
            .map(|(_, f, _)| image::Handle::from_rgba(f.rgba.width, f.rgba.height, f.rgba.pixels));
        h.resolve_icon(session, icon);
    }
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        let ours = name.starts_with("real-mixer-") || name.starts_with("real-mini-");
        if ours && name.ends_with(".png") {
            std::fs::remove_file(&path).unwrap();
        }
    }
    h.app.page = Page::Mixer;
    let default = WindowSize::default();
    for (name, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            Size::new(default.width, default.height),
            h.app.view(id),
        );
        let path = format!("{dir}/real-mixer-{name}.png");
        assert!(ui.snapshot(&theme).unwrap().matches_image(&path).unwrap());
    }
    let mini = h.open_mini();
    for (name, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
        let size = Size::new(style::MINI_WIDTH, h.app.mini_height);
        let path = format!("{dir}/real-mini-{name}.png");
        render_png(h.app.view(mini), size, 2.0, &theme, &path);
    }
}

/// Replaces the default device with one whose name is too long for the
/// picker and whose sessions show details: three Firefox tabs (one title
/// right-to-left, one with an emoji), Spotify without a detail, a muted
/// Discord with one, a session whose name and detail are both too long, and
/// two tabs on the same page.
fn use_detail_device(h: &mut Harness) {
    let mut device = h.backend.snapshot().unwrap().default_device.unwrap();
    device.name = "Arctis Nova Pro Wireless Analog Stereo".into();
    let firefox = device.sessions[0].clone();
    let tab = |app: &str, detail: &str, volume: f32| pcvc_audio::Session {
        id: MockBackend::session_id(&device.id, app),
        volume,
        ..firefox.clone().with_detail(detail)
    };
    let mut discord = device.sessions[2].clone().with_detail("#general - Voice");
    discord.muted = true;
    device.sessions = vec![
        tab("firefox", "Big Buck Bunny - YouTube", 100.0),
        tab(
            "firefox-2",
            "\u{5d7}\u{5d3}\u{5e9}\u{5d5}\u{5ea} - \u{5e2}\u{5e8}\u{5d5}\u{5e5} 12",
            72.0,
        ),
        tab(
            "firefox-3",
            "Lofi beats \u{1f3a7} to relax to - YouTube",
            45.0,
        ),
        device.sessions[1].clone(),
        discord,
        pcvc_audio::Session::new(
            MockBackend::session_id(&device.id, "cpal"),
            "cpal-playback-660845 with a much longer stream name",
            70.0,
        )
        .with_detail("A very long detail, as long as a stream title can be, that fits nowhere"),
        tab("firefox-4", "Radio Paradise - Main Mix", 30.0),
        tab("firefox-5", "Radio Paradise - Main Mix", 30.0),
    ];
    h.backend.add_device(device);
    h.refresh_audio();
}

/// The Firefox icon found, misses (letter tiles) for Spotify and Discord,
/// and the long session still pending.
fn resolve_detail_icons(h: &mut Harness) {
    let sessions = h.sessions();
    let app = icons::rasterize(icons::APP, style::APP_ICON as u32).unwrap();
    h.resolve_icon(
        &sessions[0],
        Some(image::Handle::from_rgba(app.width, app.height, app.pixels)),
    );
    h.resolve_icon(&sessions[3], None);
    h.resolve_icon(&sessions[4], None);
}

/// Adds a session to the default device and delivers the new snapshot.
fn add_session(h: &mut Harness, app: &str, name: &str, volume: f32) {
    let device = h.backend.snapshot().unwrap().default_device.unwrap().id;
    let id = MockBackend::session_id(&device, app);
    h.backend
        .add_session(&device, pcvc_audio::Session::new(id, name, volume))
        .unwrap();
    h.refresh_audio();
}

const LONG_NAME: &str = "A very long application name that does not fit in its column at all";

/// The demo sessions plus one with a long name, with Discord muted.
fn mini_harness() -> Harness {
    let mut h = Harness::new();
    let mut device = h.backend.snapshot().unwrap().default_device.unwrap();
    device.sessions[2].muted = true;
    h.backend.add_device(device);
    add_session(&mut h, "long", LONG_NAME, 31.0);
    h
}

/// Vertical centre of the text found by `selector`.
fn middle(ui: &mut Simulator<'_, Message>, selector: &str) -> f32 {
    let bounds = ui.find(selector).unwrap().bounds();
    bounds.y + bounds.height / 2.0
}

#[test]
fn mini_opens_beside_the_full_window_and_fits_its_rows() {
    let mut h = Harness::new();
    let main = h.open_window();
    let mini = h.open_mini();
    assert_ne!(main, mini);
    assert_eq!(h.app.window, Some(main));
    assert_eq!(h.app.mini_height, height(Body::Rows(4)));
    assert_eq!(h.app.title(mini), "PcVolumeControl Mixer");
    // Each window shows its own view.
    simulator(h.app.view(main)).find("Status").unwrap();
    let mut ui = simulator(h.app.view(mini));
    ui.find("Output device").unwrap();
    assert!(ui.find("Status").is_err());
    drop(ui);

    // Both follow the audio state; the mini mixer grows and shrinks.
    add_session(&mut h, "vlc", "VLC", 30.0);
    assert_eq!(h.app.mini_height, height(Body::Rows(5)));
    simulator(h.app.view(main)).find("VLC").unwrap();
    simulator(h.app.view(mini)).find("VLC").unwrap();
    for session in h.sessions() {
        h.backend.remove_session(&session.id).unwrap();
    }
    h.refresh_audio();
    assert_eq!(h.app.mini_height, height(Body::Rows(0)));

    // Icons are rendered for the sharpest open window.
    h.run(vec![Message::ScaleFactor(mini, 2.0)]);
    assert_eq!(h.app.icon_size(), Some(48));
    h.run(vec![Message::Escape(mini)]);
    assert_eq!(h.app.mini.id(), None);
    assert_eq!(h.app.icon_size(), Some(24));
    assert_eq!(h.app.window, Some(main));
}

#[test]
fn mini_closes_and_opens_the_full_window() {
    let mut h = Harness::new();
    let tray = |action| Message::Inbound(Inbound::Tray(action));

    // The menu item toggles it; Escape elsewhere, or while its device list
    // is open, does nothing.
    let mini = h.open_mini_with(TrayAction::Mixer);
    h.run(vec![Message::Escape(window::Id::unique())]);
    assert!(h.app.mini.is(mini));
    h.run(vec![
        Message::Mini(MiniMessage::PickerMenu(true)),
        Message::Escape(mini),
    ]);
    assert!(h.app.mini.is(mini));
    h.run(vec![tray(TrayAction::Mixer)]);
    assert_eq!(h.app.mini.id(), None);

    // Middle clicks toggle it too.
    let middle = TrayAction::Click(tray_icon::MouseButton::Middle, None);
    h.open_mini_with(middle);
    h.run(vec![tray(middle)]);
    assert_eq!(h.app.mini.id(), None);

    // Its button opens the full window and closes it.
    h.open_mini();
    h.run(vec![Message::Mini(MiniMessage::OpenFull)]);
    assert_eq!(h.app.mini.id(), None);
    let main = h.app.window.unwrap();

    // Focus changes of the full window leave it alone.
    let mini = h.open_mini();
    h.run(vec![
        Message::WindowFocus(mini, true),
        Message::WindowFocus(main, false),
    ]);
    assert!(h.app.mini.is(mini));
    h.run(vec![tray(TrayAction::Mixer)]);

    // Closing it never quits, even without a tray; the close button too.
    let mini = h.open_mini();
    h.run(vec![Message::WindowCloseRequested(mini)]);
    assert_eq!(h.app.mini.id(), None);
    assert!(!h.app.quitting);
    h.open_mini();
    h.run(vec![Message::Mini(MiniMessage::Close)]);
    assert_eq!(h.app.mini.id(), None);

    // A second launch shows the full window and leaves the mini mixer.
    let mini = h.open_mini();
    h.run(vec![Message::Inbound(Inbound::ShowRequested)]);
    assert!(h.app.mini.is(mini));
    assert_eq!(h.app.window, Some(main));

    // A double click opens the full window and closes the mini mixer.
    h.run(vec![tray(TrayAction::DoubleClick)]);
    assert_eq!(h.app.mini.id(), None);
    assert_eq!(h.app.window, Some(main));
}

#[test]
fn mini_fits_while_loading_and_without_a_device() {
    let mut h = Harness::new();
    h.app.audio = None;
    h.open_mini();
    assert_eq!(h.app.mini_height, height(Body::Loading));
    let mut ui = h.mini_ui();
    let note = middle(&mut ui, "Reading audio devices...");
    let bottom = note + style::MINI_ROW / 2.0 + style::MINI_PADDING.bottom;
    assert!((bottom - h.app.mini_height).abs() < 0.5, "{bottom}");
    drop(ui);

    let mut state = h.backend.snapshot().unwrap();
    state.default_device = None;
    h.run(vec![Message::Inbound(Inbound::Audio(state))]);
    assert_eq!(h.app.mini_height, height(Body::NoDevice));
    let mut ui = h.mini_ui();
    ui.find("Output device").unwrap();
    let note = middle(&mut ui, "No default playback device");
    let bottom = note + style::MINI_ROW / 2.0 + style::MINI_PADDING.bottom;
    assert!((bottom - h.app.mini_height).abs() < 0.5, "{bottom}");
}

#[test]
fn mini_resizes_only_once_created() {
    let mut h = Harness::new();
    h.run(vec![Message::Inbound(Inbound::Tray(TrayAction::Mixer))]);
    let mini = h.app.mini.id().unwrap();
    // A new application before the window exists: sized when it does.
    add_session(&mut h, "vlc", "VLC", 30.0);
    assert_eq!(h.app.mini_height, height(Body::Rows(4)));
    h.run(vec![Message::Mini(MiniMessage::Created(mini, 1.0))]);
    assert_eq!(h.app.mini_height, height(Body::Rows(5)));
}

/// A 1920x1080 monitor at 150 % with a bottom taskbar, and the anchoring
/// for an icon on it.
fn fake_locator(click: Option<placement::ScreenRect>) -> Option<Anchoring> {
    let bounds = iced::Rectangle::new(iced::Point::ORIGIN, Size::new(1920.0, 1080.0));
    let monitor = placement::Monitor {
        bounds,
        work: iced::Rectangle {
            height: 1032.0,
            ..bounds
        },
        scale: Some(1.5),
    };
    Some(Anchoring::new(click?, monitor))
}

#[test]
fn mini_is_anchored_to_the_tray_click() {
    let mut h = Harness::new();
    h.app.mini_locator = Some(fake_locator);
    // The full window has reported 1.25: positions are first converted at
    // that scale.
    let main = h.open_window();
    h.run(vec![Message::ScaleFactor(main, 1.25)]);
    let icon = iced::Rectangle::new(iced::Point::new(1500.0, 1040.0), Size::new(32.0, 40.0));
    let click = TrayAction::Click(tray_icon::MouseButton::Left, Some(icon));
    h.run(vec![Message::Inbound(Inbound::Tray(click))]);
    let mini = h.app.mini.id().unwrap();
    let anchoring = fake_locator(Some(icon));
    assert_eq!(h.app.mini_anchoring, anchoring);
    assert_eq!(h.app.mini_position_scale, 1.25);
    // Created at its own scale, which moves it (or, on Windows, places it
    // while hidden).
    h.run(vec![Message::Mini(MiniMessage::Created(mini, 1.0))]);
    assert_eq!(h.app.window_scale(mini), 1.0);
    // Moving to the 150 % monitor makes its scale the window's own.
    h.run(vec![Message::ScaleFactor(mini, 1.5)]);
    assert_eq!(h.app.mini_anchoring.unwrap().monitor.scale, None);
    // The menu has no click to anchor to.
    h.run(vec![Message::Escape(mini)]);
    h.run(vec![Message::Inbound(Inbound::Tray(TrayAction::Mixer))]);
    assert_eq!(h.app.mini_anchoring, None);
}

#[test]
fn mini_shows_the_mixer_rows() {
    let mut h = mini_harness();
    h.open_mini();
    let sessions = h.sessions();
    let red = image::Handle::from_rgba(1, 1, vec![255, 0, 0, 255]);
    h.resolve_icon(&sessions[0], Some(red));
    h.resolve_icon(&sessions[1], None);
    assert_eq!(h.app.mini_height, height(Body::Rows(5)));

    let mut ui = h.mini_ui();
    for name in ["Output device", "Master", "Firefox", "Spotify", "Discord"] {
        ui.find(name).unwrap();
    }
    ui.find(icon_id("image", &sessions[0])).unwrap();
    ui.find(icon_id("fallback", &sessions[1])).unwrap();
    // Discord is muted: the only Unmute.
    let unmute = ui.find("Unmute").unwrap().bounds();
    let discord = middle(&mut ui, "Discord");
    assert!((unmute.y + unmute.height / 2.0 - discord).abs() < 1.0);
    // The long name is shortened; the columns line up.
    let (name, bounds) = ui
        .find(|c: iced_test::selector::Candidate<'_>| match c {
            iced_test::selector::Candidate::Text {
                content, bounds, ..
            } if content.starts_with("A very") => Some((content.to_owned(), bounds)),
            _ => None,
        })
        .unwrap();
    assert!(name.ends_with('\u{2026}'), "{name:?}");
    let spotify = ui.find("Spotify").unwrap().bounds();
    assert_eq!(bounds.x, spotify.x);
    let long_percent = ui.find("31%").unwrap().bounds();
    let master_percent = ui.find("80%").unwrap().bounds();
    assert_eq!(long_percent.x, master_percent.x);
    assert!(long_percent.x + long_percent.width < style::MINI_WIDTH);
    // Rows are evenly spaced, and the window is as tall as its rows.
    let rows: Vec<f32> = ["100%", "65%", "40%", "55%", "31%"]
        .iter()
        .map(|p| middle(&mut ui, p))
        .collect();
    let step = style::MINI_APP_ROW + style::COMPACT.row_spacing;
    for pair in rows.windows(2) {
        assert!((pair[1] - pair[0] - step).abs() < 0.5, "{rows:?}");
    }
    let bottom = rows[4] + style::MINI_APP_ROW / 2.0 + style::MINI_PADDING.bottom;
    assert!((bottom - h.app.mini_height).abs() < 0.5, "{bottom}");
}

#[test]
fn mini_without_applications() {
    let mut h = Harness::new();
    for session in h.sessions() {
        h.backend.remove_session(&session.id).unwrap();
    }
    h.refresh_audio();
    h.open_mini();
    assert_eq!(h.app.mini_height, height(Body::Rows(0)));
    let mut ui = h.mini_ui();
    ui.find("Master").unwrap();
    let note = middle(&mut ui, "No applications are playing audio");
    let bottom = note + style::MINI_ROW / 2.0 + style::MINI_PADDING.bottom;
    assert!((bottom - h.app.mini_height).abs() < 0.5, "{bottom}");
}

/// Messages from clicking at `point`.
fn click_at(mut ui: Simulator<'_, Message>, point: iced::Point) -> Vec<String> {
    ui.point_at(point);
    ui.simulate(iced_test::simulator::click());
    ui.into_messages().map(|m| format!("{m:?}")).collect()
}

/// Where the Spotify row's slider middle and mute button are in a view
/// whose rows are `width` wide at `density`.
fn spotify_controls(
    ui: &mut Simulator<'_, Message>,
    h: &Harness,
    width: f32,
    density: &style::Density,
) -> (iced::Point, iced::Point) {
    let icon = ui
        .find(icon_id("pending", &h.sessions()[1]))
        .unwrap()
        .bounds();
    let percent = ui.find("65%").unwrap().bounds();
    let y = percent.y + percent.height / 2.0;
    let cols = mixer::MixerColumns::new(width, density);
    let slider_start = icon.x + cols.label + density.spacing;
    let slider_end = percent.x - density.spacing;
    let mute = percent.x + percent.width + density.spacing + density.mute_width / 2.0;
    (
        iced::Point::new((slider_start + slider_end) / 2.0, y),
        iced::Point::new(mute, y),
    )
}

#[test]
fn mini_sends_the_same_messages_as_the_mixer() {
    let mut h = Harness::new();
    h.open_window();
    h.open_mini();
    let full_size = Size::new(800.0, 600.0);
    let full_ui = || Simulator::with_size(iced::Settings::default(), full_size, h.app.mixer_view());
    let mini_width = style::MINI_WIDTH - style::MINI_PADDING.left - style::MINI_PADDING.right;

    // The master's mute button comes first in both.
    let clicks = |mut ui: Simulator<'_, Message>| {
        ui.click("Mute").unwrap();
        ui.into_messages()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>()
    };
    let master = clicks(full_ui());
    assert_eq!(master, ["Mixer(ToggleMute(Master))"]);
    assert_eq!(clicks(h.mini_ui()), master);

    let (full_slider, full_mute) =
        spotify_controls(&mut full_ui(), &h, full_size.width, &style::REGULAR);
    let (mini_slider, mini_mute) =
        spotify_controls(&mut h.mini_ui(), &h, mini_width, &style::COMPACT);
    let mute = click_at(full_ui(), full_mute);
    assert_eq!(mute.len(), 1);
    assert!(mute[0].contains("ToggleMute(Session("), "{mute:?}");
    assert_eq!(click_at(h.mini_ui(), mini_mute), mute);
    let slide = click_at(full_ui(), full_slider);
    assert_eq!(slide.len(), 2, "{slide:?}");
    assert!(slide[0].contains("Slide(Session(") && slide[0].contains("50.0"));
    assert!(slide[1].contains("Release(Session("));
    assert_eq!(click_at(h.mini_ui(), mini_slider), slide);
}

/// Renders `element` at `size` and `scale` into a PNG at `path`.
/// `Simulator::snapshot` always renders at scale 2.
fn render_png(element: Element<'_, Message>, size: Size, scale: f32, theme: &Theme, path: &str) {
    use iced::theme::Base;
    use iced_test::core::renderer::{Headless, Style};
    use iced_test::runtime::user_interface::{Cache, UserInterface};
    use resvg::tiny_skia::{IntSize, Pixmap};

    let mut renderer = iced_test::futures::futures::executor::block_on(
        <iced_test::renderer::Renderer as Headless>::new(
            iced::Font::with_name("Fira Sans"),
            iced::Pixels(style::TEXT_DEFAULT),
            None,
        ),
    )
    .unwrap();
    let mut ui = UserInterface::build(element, size, Cache::default(), &mut renderer);
    let cursor = iced::mouse::Cursor::Unavailable;
    let redraw = iced::Event::Window(window::Event::RedrawRequested(iced::time::Instant::now()));
    let _ = ui.update(
        &[redraw],
        cursor,
        &mut renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Vec::new(),
    );
    let base = theme.base();
    ui.draw(
        &mut renderer,
        theme,
        &Style {
            text_color: base.text_color,
        },
        cursor,
    );
    let (width, height) = (
        (size.width * scale).round() as u32,
        (size.height * scale).round() as u32,
    );
    let rgba = renderer.screenshot(iced::Size::new(width, height), scale, base.background_color);
    // Opaque pixels read the same premultiplied.
    let pixmap = Pixmap::from_vec(rgba, IntSize::from_wh(width, height).unwrap()).unwrap();
    pixmap.save_png(path).unwrap();
}

/// Renders the mini mixer in both themes at scales 1 and 2 into
/// `PCVC_SNAPSHOT_DIR` (`mini-*.png`) for a manual look. Run with
/// `--ignored`.
#[test]
#[ignore = "writes PNG files for manual inspection"]
fn render_mini() {
    let Ok(dir) = std::env::var("PCVC_SNAPSHOT_DIR") else {
        return;
    };
    let mut h = Harness::new();
    use_detail_device(&mut h);
    let id = h.open_mini();
    resolve_detail_icons(&mut h);
    let mut empty = Harness::new();
    for session in empty.sessions() {
        empty.backend.remove_session(&session.id).unwrap();
    }
    empty.refresh_audio();
    let empty_id = empty.open_mini();
    for (name, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
        for scale in [1.0, 2.0] {
            let size = Size::new(style::MINI_WIDTH, h.app.mini_height);
            let path = format!("{dir}/mini-{name}-{scale}x.png");
            render_png(h.app.view(id), size, scale, &theme, &path);
            let size = Size::new(style::MINI_WIDTH, empty.app.mini_height);
            let path = format!("{dir}/mini-empty-{name}-{scale}x.png");
            render_png(empty.app.view(empty_id), size, scale, &theme, &path);
        }
    }
}

/// The Mixer page at a size with room for every row.
fn full_ui(h: &Harness) -> Simulator<'_, Message> {
    Simulator::with_size(
        iced::Settings::default(),
        Size::new(800.0, 900.0),
        h.app.mixer_view(),
    )
}

/// The content and bounds of the first text below `below` whose content
/// satisfies `matches`.
fn text_bounds(
    ui: &mut Simulator<'_, Message>,
    below: f32,
    matches: impl Fn(&str) -> bool + Send,
) -> (String, iced::Rectangle) {
    ui.find(move |c: iced_test::selector::Candidate<'_>| match c {
        iced_test::selector::Candidate::Text {
            content, bounds, ..
        } if bounds.y > below && matches(content) => Some((content.to_owned(), bounds)),
        _ => None,
    })
    .unwrap()
}

/// Delivers the backend's state as the watcher does, returning what the
/// update asks the runtime to do.
fn deliver_audio(h: &mut Harness) -> Task<Message> {
    let state = h.backend.snapshot().unwrap();
    let _guard = h.runtime.enter();
    h.app.update(Message::Inbound(Inbound::Audio(state)))
}

#[test]
fn rows_keep_their_height_as_details_come_and_go() {
    let mut h = Harness::new();
    h.open_window();
    h.open_mini();
    let sessions = h.sessions();
    let (firefox, discord) = (&sessions[0].id, &sessions[2].id);
    let percents = ["100%", "65%", "40%", "55%"];
    let rows = |mut ui: Simulator<'_, Message>| percents.map(|p| middle(&mut ui, p));
    let full = rows(full_ui(&h));
    let mini = rows(h.mini_ui());
    let mini_height = h.app.mini_height;
    // Rows are evenly spaced at each density's fixed height.
    for (rows, step) in [
        (
            full,
            style::REGULAR.name_height + style::REGULAR.row_spacing,
        ),
        (mini, style::MINI_APP_ROW + style::COMPACT.row_spacing),
    ] {
        for pair in rows.windows(2) {
            assert!((pair[1] - pair[0] - step).abs() < 0.5, "{rows:?}");
        }
    }
    h.run(vec![Message::Mixer(MixerMessage::ToggleMute(
        Target::Session(discord.clone()),
    ))]);
    h.wait_for(|s| s.default_device.as_ref().unwrap().sessions[2].muted);
    // Short enough not to be shortened in either.
    let steps = [
        Some("Big Buck Bunny"),
        Some("Main Mix"),
        None,
        Some("Big Buck Bunny"),
    ];
    for detail in steps {
        h.backend.set_session_detail(firefox, detail).unwrap();
        h.backend
            .set_session_detail(discord, detail.map(|_| "#general"))
            .unwrap();
        let task = deliver_audio(&mut h);
        assert_eq!(task.units(), 0, "the mini mixer keeps its size");
        assert_eq!(h.app.mini_height, mini_height);
        assert_eq!(rows(full_ui(&h)), full);
        assert_eq!(rows(h.mini_ui()), mini);
        for (mut ui, percents) in [(full_ui(&h), full), (h.mini_ui(), mini)] {
            // A name without a detail is centred in its row.
            assert!((middle(&mut ui, "Spotify") - percents[1]).abs() < 0.5);
            let name = ui.find("Firefox").unwrap().bounds();
            match detail {
                None => {
                    assert!((name.center_y() - percents[0]).abs() < 0.5);
                    assert!(ui.find("#general").is_err());
                }
                Some(detail) => {
                    // Name above the middle, detail below, in one column.
                    let below = ui.find(detail).unwrap().bounds();
                    assert!(name.center_y() < percents[0] && below.center_y() > percents[0]);
                    assert_eq!(below.x, name.x);
                    assert!(below.height < name.height);
                    assert!(below.y >= name.y + name.height - 0.5);
                    // The muted row shows its detail beneath its name.
                    let muted = ui.find("#general").unwrap().bounds();
                    assert!(muted.center_y() > percents[2]);
                    ui.find("Unmute").unwrap();
                }
            }
        }
    }
}

#[test]
fn long_details_are_shortened_within_the_name_column() {
    let mut h = Harness::new();
    use_detail_device(&mut h);
    h.open_window();
    h.open_mini();
    let mini_width = style::MINI_WIDTH - style::MINI_PADDING.left - style::MINI_PADDING.right;
    let views = [
        (
            Simulator::with_size(iced::Settings::default(), MIN_SIZE, h.app.mixer_view()),
            mixer::MixerColumns::new(MIN_SIZE.width, &style::REGULAR),
        ),
        (
            h.mini_ui(),
            mixer::MixerColumns::new(mini_width, &style::COMPACT),
        ),
    ];
    for (mut ui, cols) in views {
        let (name, name_bounds) = text_bounds(&mut ui, 0.0, |c| c.starts_with("cpal-"));
        let (detail, detail_bounds) = text_bounds(&mut ui, 0.0, |c| c.starts_with("A very"));
        for (text, bounds) in [(&name, name_bounds), (&detail, detail_bounds)] {
            assert!(text.ends_with('\u{2026}'), "{text:?}");
            assert!(bounds.width <= cols.name + 0.5, "{text:?} {bounds:?}");
        }
        assert_eq!(detail_bounds.x, name_bounds.x);
        // Right-to-left and emoji details are shown as they are.
        let (rtl, _) = text_bounds(&mut ui, 0.0, |c| c.contains('\u{5d7}'));
        assert!(rtl.starts_with('\u{5d7}'), "{rtl:?}");
        text_bounds(&mut ui, 0.0, |c| c.starts_with("Lofi beats"));
        // Two tabs on the same page are two rows.
        let (_, first) = text_bounds(&mut ui, 0.0, |c| c.starts_with("Radio Paradise"));
        let (_, second) = text_bounds(&mut ui, first.y, |c| c.starts_with("Radio Paradise"));
        assert!(second.y >= first.y + first.height);
    }
    // The mini mixer is as tall as its eight rows, which all fit.
    assert_eq!(h.app.mini_height, height(Body::Rows(8)));
    assert!(h.app.mini_height < style::MINI_MAX_HEIGHT);
    let mut ui = h.mini_ui();
    let (_, first) = text_bounds(&mut ui, 0.0, |c| c == "30%");
    let (_, last) = text_bounds(&mut ui, first.y, |c| c == "30%");
    let bottom = last.center_y() + style::MINI_APP_ROW / 2.0 + style::MINI_PADDING.bottom;
    assert!((bottom - h.app.mini_height).abs() < 0.5, "{bottom}");
}

#[test]
fn detail_changes_keep_a_held_volume() {
    let mut h = Harness::new();
    h.open_window();
    h.open_mini();
    let spotify = h.sessions()[1].clone();
    let red = image::Handle::from_rgba(1, 1, vec![255, 0, 0, 255]);
    h.resolve_icon(&spotify, Some(red));
    let target = Target::Session(spotify.id.clone());
    // The first value goes out, the second is held by the throttle.
    h.run(vec![
        Message::Mixer(MixerMessage::Slide(target.clone(), 20.0)),
        Message::Mixer(MixerMessage::Slide(target.clone(), 21.0)),
    ]);
    let timer = h.app.mixer_timer;
    assert!(timer.is_some());
    for detail in [Some("Song One"), Some("Song Two"), None] {
        h.backend.set_session_detail(&spotify.id, detail).unwrap();
        let task = deliver_audio(&mut h);
        assert_eq!(task.units(), 0);
        assert_eq!(h.app.mixer.volume(&target, spotify.volume), 21.0);
        assert_eq!(h.app.mixer_timer, timer);
        simulator(h.app.mixer_view()).find("21%").unwrap();
        let mut ui = h.mini_ui();
        ui.find("21%").unwrap();
        // The icon is kept, not looked up again.
        ui.find(icon_id("image", &spotify)).unwrap();
    }
}
