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
use crate::mixer::Target;

struct Harness {
    app: App,
    backend: Arc<MockBackend>,
    /// Entered around every update, as iced does. Nothing drives it, so the
    /// server and watcher tasks spawned at boot never actually run.
    runtime: tokio::runtime::Runtime,
}

impl Harness {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let backend = Arc::new(MockBackend::demo());
        let (inbox, inbox_rx) = inbox::channel();
        let config = Config::default();
        let (app, _) = App::boot(Boot {
            config,
            config_path: None,
            show_window: false,
            backend: Arc::clone(&backend) as Arc<dyn AudioBackend>,
            icon_resolver: Arc::new(crate::app_icons::NoIcons),
            server_icons: None,
            inbox,
            inbox_rx,
            logs: LogBuffer::new(10),
        });
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

    fn run(&mut self, messages: Vec<Message>) {
        let _guard = self.runtime.enter();
        for message in messages {
            let _ = self.app.update(message);
        }
    }

    /// Pretends a window at scale 1 is open, so the Mixer shows icons.
    fn open_window(&mut self) -> window::Id {
        let id = window::Id::unique();
        self.app.window = Some(id);
        self.run(vec![Message::ScaleFactor(id, 1.0)]);
        id
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
    // A device and a session with names too long for their columns, and a
    // muted session.
    let mut device = h.backend.snapshot().unwrap().default_device.unwrap();
    device.name = "Arctis Nova Pro Wireless Analog Stereo".into();
    device.sessions[2].muted = true;
    device.sessions.push(pcvc_audio::Session::new(
        MockBackend::session_id(&device.id, "cpal"),
        "cpal-playback-660845 with a much longer stream name",
        70.0,
    ));
    h.backend.add_device(device).unwrap();
    h.refresh_audio();
    let id = h.open_window();
    // One icon found, three misses (fallback tiles) and one still pending.
    let sessions = h.sessions();
    let app = icons::rasterize(icons::APP, style::APP_ICON as u32).unwrap();
    h.resolve_icon(
        &sessions[0],
        Some(image::Handle::from_rgba(app.width, app.height, app.pixels)),
    );
    h.resolve_icon(&sessions[1], None);
    h.resolve_icon(&sessions[2], None);
    h.resolve_icon(&sessions[4], None);
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
}

/// Renders the window's Mixer page for the real sound server's current
/// streams, with icons resolved from this machine's desktop entries and
/// themes, into `PCVC_SNAPSHOT_DIR` (`real-mixer-*.png`), and prints what
/// each stream resolved to. Only reads: no audio state is changed. Run with
/// `--ignored --nocapture`.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "reads the real sound server and icon themes"]
fn render_real_mixer() {
    use crate::app_icons::linux::{Dirs, LinuxResolver};

    let Ok(dir) = std::env::var("PCVC_SNAPSHOT_DIR") else {
        return;
    };
    let state = pcvc_audio::PulseBackend::new().unwrap().snapshot().unwrap();
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
        println!("{:?} {:?} -> {outcome}", session.name, session.app);
        let icon = found
            .map(|(_, f, _)| image::Handle::from_rgba(f.rgba.width, f.rgba.height, f.rgba.pixels));
        h.resolve_icon(session, icon);
    }
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with("real-mixer-") && name.ends_with(".png") {
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
}
