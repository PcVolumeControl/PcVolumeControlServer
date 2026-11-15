//! The v8 session details: an optional session member with no feature name.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use pcvc_audio::{AudioBackend, MockBackend, Session};
use pcvc_protocol::{FullState, encode_full_state};
use pcvc_server::{IconId, IconImage, IconProvider, ServerConfig, ServerHandle, to_full_state};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast;
use tokio::time::{Instant, timeout};

const TIMEOUT: Duration = Duration::from_secs(5);
const SPEAKERS: &str = "0f4090a9-dee2-4563-ba29-0ad6b93d9e22";

async fn start(details: bool, sessions: &[Session]) -> (Arc<MockBackend>, ServerHandle) {
    start_with(None, details, sessions).await
}

async fn start_with(
    icons: Option<Arc<dyn IconProvider>>,
    details: bool,
    sessions: &[Session],
) -> (Arc<MockBackend>, ServerHandle) {
    let mock = Arc::new(MockBackend::demo());
    for session in sessions {
        mock.add_session(SPEAKERS, session.clone()).unwrap();
    }
    let backend: Arc<dyn AudioBackend> = mock.clone();
    let mut config = ServerConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .with_mdns(false)
        .with_details(details);
    if let Some(icons) = icons {
        config = config.with_icons(icons);
    }
    let server = ServerHandle::start(backend, config).await.unwrap();
    (mock, server)
}

/// A provider that knows no icons.
struct NoIcons(broadcast::Sender<()>);

impl IconProvider for NoIcons {
    fn icon_id(&self, _session: &Session) -> Option<IconId> {
        None
    }

    fn icon_png(&self, _id: &IconId, _size: u32) -> Option<IconImage> {
        None
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.0.subscribe()
    }
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    /// Held open: the server hangs up on a client that closes it.
    _writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let (read, writer) = TcpStream::connect(addr).await.unwrap().into_split();
        Self {
            reader: BufReader::new(read),
            _writer: writer,
        }
    }

    async fn read_line(&mut self) -> String {
        let mut line = String::new();
        let n = timeout(TIMEOUT, self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a state")
            .unwrap();
        assert!(n > 0, "connection closed");
        line
    }

    async fn read_state(&mut self) -> FullState {
        serde_json::from_str(&self.read_line().await).unwrap()
    }
}

fn tab(app: &str, detail: &str) -> Session {
    Session::new(MockBackend::session_id(SPEAKERS, app), "Firefox", 50.0).with_detail(detail)
}

fn details(state: &FullState) -> Vec<(&str, Option<&str>)> {
    let sessions = &state.default_device.sessions;
    sessions
        .iter()
        .map(|s| (s.name.as_str(), s.detail.as_deref()))
        .collect()
}

#[tokio::test]
async fn details_are_sent_only_when_enabled_and_are_not_a_feature() {
    let tabs = [tab("tab", "Inbox - Mail"), tab("same", " firefox ")];
    // Without an icon provider and with details off, the line is plain v7.
    let (mock, plain) = start(false, &tabs).await;
    let mut client = Client::connect(plain.local_addr()).await;
    let line = client.read_line().await;
    assert_eq!(
        line,
        encode_full_state(&to_full_state(&mock.snapshot().unwrap()).unwrap())
    );
    assert!(
        !line.contains("detail") && !line.contains("features"),
        "{line}"
    );
    plain.stop().await;

    let (_mock, server) = start(true, &tabs).await;
    let mut client = Client::connect(server.local_addr()).await;
    let line = client.read_line().await;
    assert!(!line.contains("features"), "{line}");
    let state: FullState = serde_json::from_str(&line).unwrap();
    assert_eq!(
        details(&state)[4..],
        [("Firefox", Some("Inbox - Mail")), ("Firefox", None)]
    );
    server.stop().await;
}

#[tokio::test]
async fn features_list_icons_whether_or_not_details_are_sent() {
    for details_on in [false, true] {
        let icons: Arc<dyn IconProvider> = Arc::new(NoIcons(broadcast::Sender::new(1)));
        let (_mock, server) = start_with(Some(icons), details_on, &[tab("tab", "Inbox")]).await;
        let mut client = Client::connect(server.local_addr()).await;
        let state = client.read_state().await;
        assert_eq!(state.features, ["icons"]);
        let detail = details_on.then_some("Inbox");
        assert_eq!(details(&state).last(), Some(&("Firefox", detail)));
        server.stop().await;
    }
}

#[tokio::test]
async fn detail_only_changes_are_rate_limited() {
    let (mock, server) = start(true, &[]).await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    mock.add_session(SPEAKERS, tab("tab", "one")).unwrap();
    let state = client.read_state().await;
    let sent = Instant::now();
    assert_eq!(details(&state).last(), Some(&("Firefox", Some("one"))));

    // Nothing but the detail changes: held until a second after the last
    // broadcast, then sent once with the latest title.
    let id = MockBackend::session_id(SPEAKERS, "tab");
    for title in ["two", "three", "four"] {
        mock.set_session_detail(&id, Some(title)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A client that connects meanwhile starts with the newest title.
    let mut late = Client::connect(server.local_addr()).await;
    let first = late.read_state().await;
    assert_eq!(details(&first).last(), Some(&("Firefox", Some("four"))));

    let state = client.read_state().await;
    let waited = sent.elapsed();
    assert!(waited >= Duration::from_millis(800), "{waited:?}");
    assert_eq!(details(&state).last(), Some(&("Firefox", Some("four"))));
    assert_eq!(
        late.read_state().await,
        state,
        "the broadcast reaches it too"
    );

    // Any other change goes out at once and carries the pending detail.
    mock.set_session_detail(&id, Some("five")).unwrap();
    let changed = Instant::now();
    mock.set_master(Some(3.0), None).unwrap();
    let state = client.read_state().await;
    assert!(changed.elapsed() < Duration::from_millis(500));
    assert_eq!(state.default_device.master_volume, 3.0);
    assert_eq!(details(&state).last(), Some(&("Firefox", Some("five"))));
    server.stop().await;
}
