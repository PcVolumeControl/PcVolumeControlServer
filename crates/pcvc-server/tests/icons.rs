//! The v8 icon extension, served from a scripted [`IconProvider`].

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pcvc_audio::{AudioBackend, MockBackend, Session};
use pcvc_protocol::{FullState, encode_full_state};
use pcvc_server::{IconId, IconImage, IconProvider, ServerConfig, ServerHandle, to_full_state};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before concluding that nothing more is coming.
const QUIET: Duration = Duration::from_millis(300);

/// Knows icon ids for the session names it has been given, and renders any
/// id it handed out as fake PNG bytes `png:<id>:<size>`, at most 128 pixels.
struct TestProvider {
    ids: Mutex<HashMap<String, IconId>>,
    png_sizes: Mutex<Vec<u32>>,
    changed: broadcast::Sender<()>,
}

impl TestProvider {
    fn new(names: &[&str]) -> Arc<Self> {
        let provider = Arc::new(Self {
            ids: Mutex::new(HashMap::new()),
            png_sizes: Mutex::new(Vec::new()),
            changed: broadcast::Sender::new(16),
        });
        for name in names {
            provider.learn(name);
        }
        provider
    }

    /// The id the provider uses for a session name.
    fn id_for(name: &str) -> IconId {
        let digits: String = name.bytes().map(|b| format!("{b:02x}")).collect::<String>();
        IconId::new(format!("t{}", &digits[..digits.len().min(60)])).unwrap()
    }

    fn learn(&self, name: &str) {
        self.ids
            .lock()
            .unwrap()
            .insert(name.to_owned(), Self::id_for(name));
    }

    fn notify(&self) {
        let _ = self.changed.send(());
    }

    fn png(id: &str, size: u32) -> Vec<u8> {
        format!("png:{id}:{size}").into_bytes()
    }
}

impl IconProvider for TestProvider {
    fn icon_id(&self, session: &Session) -> Option<IconId> {
        self.ids.lock().unwrap().get(&session.name).cloned()
    }

    fn icon_png(&self, id: &IconId, size: u32) -> Option<IconImage> {
        self.png_sizes.lock().unwrap().push(size);
        let known = self.ids.lock().unwrap().values().any(|i| i == id);
        known.then(|| {
            let size = size.min(128);
            IconImage {
                size,
                png: Self::png(id.as_str(), size).into(),
            }
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }
}

const DEMO_NAMES: [&str; 4] = ["Firefox", "Spotify", "Discord", "Musique \u{e9}t\u{e9}"];

async fn start(provider: Option<Arc<TestProvider>>) -> (Arc<MockBackend>, ServerHandle) {
    let mock = Arc::new(MockBackend::demo());
    let backend: Arc<dyn AudioBackend> = mock.clone();
    let mut config = ServerConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).with_mdns(false);
    if let Some(provider) = provider {
        config = config.with_icons(provider);
    }
    let server = ServerHandle::start(backend, config).await.unwrap();
    (mock, server)
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    async fn send(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
    }

    /// The next line, verbatim.
    async fn read_line(&mut self) -> String {
        let mut line = String::new();
        let n = timeout(TIMEOUT, self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a line")
            .unwrap();
        assert!(n > 0, "connection closed");
        line
    }

    async fn read_value(&mut self) -> Value {
        serde_json::from_str(&self.read_line().await).unwrap()
    }

    /// The next line, which must be a full state.
    async fn read_state(&mut self) -> FullState {
        let value = self.read_value().await;
        assert!(value.get("icon").is_none(), "unexpected icon line {value}");
        serde_json::from_value(value).unwrap()
    }

    /// Asserts that nothing arrives for a while.
    async fn assert_quiet(&mut self) {
        let mut line = String::new();
        let read = timeout(QUIET, self.reader.read_line(&mut line)).await;
        assert!(read.is_err(), "unexpected line {line}");
    }
}

fn icon_id<'a>(state: &'a FullState, name: &str) -> Option<&'a str> {
    state
        .default_device
        .sessions
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no session {name}"))
        .icon_id
        .as_ref()
        .map(IconId::as_str)
}

#[tokio::test]
async fn without_a_provider_the_output_is_plain_v7() {
    let (mock, server) = start(None).await;
    let mut client = Client::connect(server.local_addr()).await;
    let line = client.read_line().await;
    let expected = encode_full_state(&to_full_state(&mock.snapshot().unwrap()).unwrap());
    assert_eq!(line, expected);
    assert!(!line.contains("features"), "{line}");
    assert!(!line.contains("iconId"), "{line}");
    server.stop().await;
}

#[tokio::test]
async fn provider_adds_features_and_icon_ids() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    let line = client.read_line().await;
    let state: FullState = serde_json::from_str(&line).unwrap();
    assert_eq!(state.features, ["icons"]);
    for name in DEMO_NAMES {
        assert_eq!(
            icon_id(&state, name),
            Some(TestProvider::id_for(name).as_str())
        );
    }

    // Apart from the extension keys, the line is the v7 one.
    let mut stripped = state.clone();
    stripped.features.clear();
    for session in &mut stripped.default_device.sessions {
        session.icon_id = None;
    }
    let plain = to_full_state(&mock.snapshot().unwrap()).unwrap();
    assert_eq!(encode_full_state(&stripped), encode_full_state(&plain));
    server.stop().await;
}

#[tokio::test]
async fn sessions_without_a_known_icon_have_no_icon_id() {
    let provider = TestProvider::new(&["Firefox"]);
    let (_mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    let value = client.read_value().await;
    let sessions = value["defaultDevice"]["sessions"].as_array().unwrap();
    for session in sessions {
        let has_id = session.get("iconId").is_some();
        assert_eq!(has_id, session["name"] == "Firefox", "{session}");
    }
    server.stop().await;
}

#[tokio::test]
async fn an_icon_id_found_later_is_broadcast_once() {
    let provider = TestProvider::new(&["Firefox"]);
    let (_mock, server) = start(Some(provider.clone())).await;
    let mut a = Client::connect(server.local_addr()).await;
    let mut b = Client::connect(server.local_addr()).await;
    for client in [&mut a, &mut b] {
        let state = client.read_state().await;
        assert_eq!(icon_id(&state, "Spotify"), None);
    }

    provider.learn("Spotify");
    provider.notify();
    for client in [&mut a, &mut b] {
        let state = client.read_state().await;
        assert_eq!(
            icon_id(&state, "Spotify"),
            Some(TestProvider::id_for("Spotify").as_str())
        );
    }

    // A signal that changes nothing is not broadcast.
    provider.notify();
    a.assert_quiet().await;
    b.assert_quiet().await;
    server.stop().await;
}

#[tokio::test]
async fn new_sessions_are_tagged() {
    let provider = TestProvider::new(&["VLC"]);
    let (mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    let speakers = mock.snapshot().unwrap().default_device.unwrap().id;
    let id = MockBackend::session_id(&speakers, "vlc");
    mock.add_session(&speakers, Session::new(id, "VLC", 20.0))
        .unwrap();
    let state = client.read_state().await;
    assert_eq!(
        icon_id(&state, "VLC"),
        Some(TestProvider::id_for("VLC").as_str())
    );
    server.stop().await;
}

#[tokio::test]
async fn v7_updates_still_apply_with_a_provider() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    let state = client.read_state().await;
    client
        .send(&format!(
            r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{}","masterVolume":12.0}}}}"#,
            state.default_device.device_id
        ))
        .await;
    let state = client.read_state().await;
    assert_eq!(state.default_device.master_volume, 12.0);
    assert_eq!(state.features, ["icons"]);
    server.stop().await;
}
