//! Headless view tests: render views with `iced_test`, interact with them
//! and feed the resulting messages back through `update`.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use iced_test::simulator;
use pcvc_audio::{AudioBackend, AudioState, MockBackend};
use pcvc_server::ClientInfo;

use tracing_subscriber::layer::SubscriberExt;

use super::*;
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
        let config = Config {
            port: 0,
            mdns: false,
            ..Config::default()
        };
        let (app, _) = App::boot(Boot {
            config,
            show_window: false,
            backend: Arc::clone(&backend) as Arc<dyn AudioBackend>,
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

/// Renders every page in both themes into `PCVC_SNAPSHOT_DIR` for a manual
/// look. Run with `--ignored`.
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
    let id = window::Id::unique();
    h.app.window = Some(id);
    for page in [Page::Mixer, Page::Status, Page::Settings] {
        h.app.page = page;
        for (name, theme) in [("light", Theme::Light), ("dark", Theme::Dark)] {
            let mut ui = iced_test::Simulator::with_size(
                iced::Settings::default(),
                Size::new(720.0, 560.0),
                h.app.view(id),
            );
            let path = format!("{dir}/{}-{name}.png", format!("{page:?}").to_lowercase());
            let _ = std::fs::remove_file(&path);
            ui.snapshot(&theme).unwrap().matches_image(&path).unwrap();
        }
    }
}
