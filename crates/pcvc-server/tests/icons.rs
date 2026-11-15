//! Application icons, served from a scripted [`IconProvider`] or by a server
//! without one.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use pcvc_audio::{AudioBackend, MockBackend, Session};
use pcvc_protocol::{FullState, IconMessage, IconReply, encode_full_state};
use pcvc_server::{
    DisconnectReason, IconId, IconImage, IconProvider, ServerConfig, ServerEvent, ServerHandle,
    to_full_state,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before concluding that nothing more is coming.
const QUIET: Duration = Duration::from_millis(300);

/// Knows icon ids for the session names it has been given, and renders any
/// id it handed out as fake PNG bytes `png:<id>:<size>` (plus `padding`
/// zero bytes), at most 128 pixels. `icon_png` waits while the gate is
/// closed.
struct TestProvider {
    ids: Mutex<HashMap<String, IconId>>,
    png_sizes: Mutex<Vec<u32>>,
    padding: AtomicUsize,
    gate: (Mutex<bool>, Condvar),
    changed: broadcast::Sender<()>,
}

impl TestProvider {
    fn new(names: &[&str]) -> Arc<Self> {
        let provider = Arc::new(Self {
            ids: Mutex::new(HashMap::new()),
            png_sizes: Mutex::new(Vec::new()),
            padding: AtomicUsize::new(0),
            gate: (Mutex::new(true), Condvar::new()),
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

    fn set_gate(&self, open: bool) {
        *self.gate.0.lock().unwrap() = open;
        self.gate.1.notify_all();
    }

    /// Holds `icon_png` calls until the guard is dropped. Dropping it on a
    /// failed assertion too keeps the runtime from waiting forever on a
    /// blocked call.
    fn close_gate(self: &Arc<Self>) -> GateGuard {
        self.set_gate(false);
        GateGuard(Arc::clone(self))
    }

    fn png_calls(&self) -> usize {
        self.png_sizes.lock().unwrap().len()
    }

    fn png(id: &str, size: u32) -> Vec<u8> {
        format!("png:{id}:{size}").into_bytes()
    }
}

struct GateGuard(Arc<TestProvider>);

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.0.set_gate(true);
    }
}

impl IconProvider for TestProvider {
    fn icon_id(&self, session: &Session) -> Option<IconId> {
        self.ids.lock().unwrap().get(&session.name).cloned()
    }

    fn icon_png(&self, id: &IconId, size: u32) -> Option<IconImage> {
        self.png_sizes.lock().unwrap().push(size);
        let (open, wake) = &self.gate;
        drop(
            wake.wait_while(open.lock().unwrap(), |open| !*open)
                .unwrap(),
        );
        let known = self.ids.lock().unwrap().values().any(|i| i == id);
        known.then(|| {
            let size = size.min(128);
            let mut png = Self::png(id.as_str(), size);
            png.resize(png.len() + self.padding.load(Ordering::Relaxed), 0);
            IconImage {
                size,
                png: png.into(),
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
        Self::from_stream(TcpStream::connect(addr).await.unwrap())
    }

    /// A client whose socket buffers as little as the OS allows, so that the
    /// server's writes back up quickly when it stops reading.
    async fn connect_small_buffer(addr: SocketAddr) -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        Self::from_stream(socket.connect(addr).await.unwrap())
    }

    fn from_stream(stream: TcpStream) -> Self {
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

    /// Reads until nothing has arrived for a while, asserting that every line
    /// is a full state. Returns the states.
    async fn drain_states(&mut self) -> Vec<FullState> {
        let mut states = Vec::new();
        loop {
            let mut line = String::new();
            match timeout(QUIET, self.reader.read_line(&mut line)).await {
                Err(_) => return states,
                Ok(read) => assert!(read.unwrap() > 0, "connection closed"),
            }
            let value: Value = serde_json::from_str(&line).unwrap();
            assert!(value.get("defaultDevice").is_some(), "not a state: {line}");
            assert!(value.get("icon").is_none(), "icon line: {line}");
            states.push(serde_json::from_value(value).unwrap());
        }
    }

    /// Reads until the next icon reply, skipping full states.
    async fn read_icon(&mut self) -> IconReply {
        loop {
            let value = self.read_value().await;
            if value.get("defaultDevice").is_some() {
                assert!(value.get("icon").is_none(), "{value}");
                continue;
            }
            let message: IconMessage = serde_json::from_value(value).unwrap();
            assert_eq!(message.protocol_version, 8);
            return message.icon;
        }
    }

    /// Writes a line the server is expected to hang up on, ignoring write
    /// errors, and waits for the close.
    async fn assert_rejected(&mut self, line: &str) {
        let _ = self.writer.write_all(line.as_bytes()).await;
        let _ = self.writer.write_all(b"\n").await;
        let mut buf = Vec::new();
        match timeout(TIMEOUT, self.reader.read_to_end(&mut buf))
            .await
            .expect("server did not close the connection")
        {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

fn icon_request(ids: &[&str], size: Option<Value>) -> String {
    let mut request = json!({ "ids": ids });
    if let Some(size) = size {
        request["size"] = size;
    }
    json!({ "protocolVersion": 8, "iconRequest": request }).to_string()
}

/// The id, actual size and decoded PNG of an image reply.
fn image(reply: &IconReply) -> (&str, u32, Vec<u8>) {
    match reply {
        IconReply::Image {
            id,
            size,
            mime,
            data,
        } => {
            assert_eq!(mime, "image/png");
            (id, *size, BASE64.decode(data).unwrap())
        }
        IconReply::Error { .. } => panic!("not an image: {reply:?}"),
    }
}

fn assert_not_found(reply: &IconReply, expected_id: &str) {
    assert_eq!(reply, &IconReply::not_found(expected_id));
}

async fn disconnect_reason(events: &mut broadcast::Receiver<ServerEvent>) -> DisconnectReason {
    timeout(TIMEOUT, async {
        loop {
            if let ServerEvent::ClientDisconnected { reason, .. } = events.recv().await.unwrap() {
                return reason;
            }
        }
    })
    .await
    .expect("no disconnect event")
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
async fn without_a_provider_there_are_no_icon_ids() {
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
async fn provider_adds_icon_ids() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    let line = client.read_line().await;
    assert!(!line.contains("features"), "{line}");
    let state: FullState = serde_json::from_str(&line).unwrap();
    for name in DEMO_NAMES {
        assert_eq!(
            icon_id(&state, name),
            Some(TestProvider::id_for(name).as_str())
        );
    }
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
async fn updates_apply_with_a_provider() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    let state = client.read_state().await;
    client
        .send(&format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{}","masterVolume":12.0}}}}"#,
            state.default_device.device_id
        ))
        .await;
    let state = client.read_state().await;
    assert_eq!(state.default_device.master_volume, 12.0);
    server.stop().await;
}

#[tokio::test]
async fn replies_arrive_in_order_and_only_at_the_requester() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (mock, server) = start(Some(provider)).await;
    let mut events = server.events();
    let mut a = Client::connect(server.local_addr()).await;
    let mut b = Client::connect(server.local_addr()).await;
    a.read_state().await;
    b.read_state().await;

    let spotify = TestProvider::id_for("Spotify");
    let firefox = TestProvider::id_for("Firefox");
    let (spotify, firefox) = (spotify.as_str(), firefox.as_str());
    a.send(&icon_request(
        &[spotify, "unknown0", "Not-An-Id!", firefox, spotify],
        Some(json!(96)),
    ))
    .await;
    // A state change while the replies are in flight reaches both clients.
    mock.set_master(Some(3.0), None).unwrap();

    let replies = [
        a.read_icon().await,
        a.read_icon().await,
        a.read_icon().await,
        a.read_icon().await,
    ];
    let (id, size, png) = image(&replies[0]);
    assert_eq!((id, size), (spotify, 96));
    assert_eq!(png, TestProvider::png(spotify, 96));
    assert_not_found(&replies[1], "unknown0");
    assert_not_found(&replies[2], "Not-An-Id!");
    let (id, size, png) = image(&replies[3]);
    assert_eq!((id, size), (firefox, 96));
    assert_eq!(png, TestProvider::png(firefox, 96));
    // The duplicate is not answered again: only states follow.
    a.drain_states().await;
    // The other client sees the state change and nothing but states.
    let b_states = b.drain_states().await;
    assert!(
        b_states
            .iter()
            .any(|s| s.default_device.master_volume == 3.0)
    );

    let requested = timeout(TIMEOUT, async {
        loop {
            if let ServerEvent::IconsRequested { count, size, .. } = events.recv().await.unwrap() {
                return (count, size);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(requested, (4, 96));
    server.stop().await;
}

#[tokio::test]
async fn sizes_are_clamped_before_reaching_the_provider() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider.clone())).await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    let zoom = "Zoom";
    provider.learn(zoom);
    let id = TestProvider::id_for(zoom);

    // (requested, passed to the provider, actual size of the reply)
    for (size, asked, actual) in [
        (Some(json!(1000)), 256, 128),
        (Some(json!(1)), 16, 16),
        (Some(json!(-3)), 16, 16),
        (Some(json!(100.4)), 100, 100),
        (None, 64, 64),
        (Some(Value::Null), 64, 64),
    ] {
        client
            .send(&icon_request(&[id.as_str()], size.clone()))
            .await;
        let reply = client.read_icon().await;
        let (_, reply_size, png) = image(&reply);
        assert_eq!(
            provider.png_sizes.lock().unwrap().last(),
            Some(&asked),
            "{size:?}"
        );
        assert_eq!(reply_size, actual, "{size:?}");
        assert_eq!(png, TestProvider::png(id.as_str(), actual));
    }
    server.stop().await;
}

#[tokio::test]
async fn an_update_and_a_request_in_one_message_both_apply() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    let state = client.read_state().await;
    let firefox = TestProvider::id_for("Firefox");
    client
        .send(
            &json!({
                "protocolVersion": 8,
                "defaultDevice": {
                    "deviceId": state.default_device.device_id,
                    "masterVolume": 21.0
                },
                "iconRequest": { "ids": [firefox.as_str()] }
            })
            .to_string(),
        )
        .await;

    let (mut saw_state, mut saw_icon) = (false, false);
    while !(saw_state && saw_icon) {
        let value = client.read_value().await;
        if value.get("icon").is_some() {
            let message: IconMessage = serde_json::from_value(value).unwrap();
            assert_eq!(image(&message.icon).0, firefox.as_str());
            saw_icon = true;
        } else {
            let state: FullState = serde_json::from_value(value).unwrap();
            saw_state |= state.default_device.master_volume == 21.0;
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn without_a_provider_every_id_is_not_found() {
    let (_mock, server) = start(None).await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    client.send(&icon_request(&["abc", "def"], None)).await;
    assert_not_found(&client.read_icon().await, "abc");
    assert_not_found(&client.read_icon().await, "def");
    client.drain_states().await;
    server.stop().await;
}

#[tokio::test]
async fn an_empty_request_gets_no_reply() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider)).await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    client.send(&icon_request(&[], None)).await;
    client.assert_quiet().await;
    // Still connected and served.
    client.send(&icon_request(&["abc"], None)).await;
    assert_not_found(&client.read_icon().await, "abc");
    server.stop().await;
}

#[tokio::test]
async fn too_many_ids_in_one_request_drop_the_client() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider.clone())).await;
    let mut events = server.events();
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    let ids: Vec<String> = (0..33).map(|i| format!("id{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    client.assert_rejected(&icon_request(&ids, None)).await;
    let reason = disconnect_reason(&mut events).await;
    assert!(
        matches!(&reason, DisconnectReason::Protocol(m) if m.contains("33 ids")),
        "{reason:?}"
    );
    assert_eq!(provider.png_calls(), 0);
    server.stop().await;
}

#[tokio::test]
async fn malformed_requests_drop_the_client() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let (_mock, server) = start(Some(provider)).await;
    for line in [
        r#"{"protocolVersion":8,"iconRequest":"abc"}"#,
        r#"{"protocolVersion":8,"iconRequest":{"ids":"abc"}}"#,
        r#"{"protocolVersion":8,"iconRequest":{"ids":["a"],"size":"64"}}"#,
    ] {
        let mut client = Client::connect(server.local_addr()).await;
        client.read_state().await;
        client.assert_rejected(line).await;
    }
    server.stop().await;
}

#[tokio::test]
async fn up_to_64_ids_may_be_outstanding_and_are_answered_in_order() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let gate = provider.close_gate();
    let (_mock, server) = start(Some(provider.clone())).await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    let ids: Vec<String> = (0..64).map(|i| format!("id{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    client.send(&icon_request(&ids[..32], None)).await;
    client.send(&icon_request(&ids[32..], None)).await;
    client.assert_quiet().await;
    drop(gate);
    for id in ids {
        assert_not_found(&client.read_icon().await, id);
    }
    assert_eq!(provider.png_calls(), 64);
    server.stop().await;
}

#[tokio::test]
async fn going_over_64_outstanding_ids_drops_the_client() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let gate = provider.close_gate();
    let (_mock, server) = start(Some(provider.clone())).await;
    let mut events = server.events();
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    let ids: Vec<String> = (0..65).map(|i| format!("id{i}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    client.send(&icon_request(&ids[..32], None)).await;
    client.send(&icon_request(&ids[32..64], None)).await;
    client
        .assert_rejected(&icon_request(&ids[64..], None))
        .await;
    let reason = disconnect_reason(&mut events).await;
    assert!(
        matches!(&reason, DisconnectReason::Protocol(m) if m.contains("outstanding")),
        "{reason:?}"
    );
    // At most one provider call was in progress for the client.
    assert!(provider.png_calls() <= 1);
    drop(gate);
    server.stop().await;
}

#[tokio::test]
async fn a_client_that_stops_reading_does_not_hold_up_others() {
    let provider = TestProvider::new(&DEMO_NAMES);
    provider.padding.store(500 * 1024, Ordering::Relaxed);
    let (mock, server) = start(Some(provider.clone())).await;
    let mut clients = server.clients();

    // Asks for 64 large replies (about 44 MB of lines) and never reads them.
    let mut stalled = Client::connect_small_buffer(server.local_addr()).await;
    stalled.read_state().await;
    let ids: Vec<IconId> = DEMO_NAMES.iter().map(|n| TestProvider::id_for(n)).collect();
    let ids: Vec<&str> = ids.iter().map(IconId::as_str).collect();
    for _ in 0..16 {
        stalled.send(&icon_request(&ids, Some(json!(256)))).await;
    }

    let mut other = Client::connect(server.local_addr()).await;
    other.read_state().await;
    for volume in 1..=5 {
        mock.set_master(Some(volume as f32), None).unwrap();
        let state = other.read_state().await;
        assert_eq!(state.default_device.master_volume, volume as f32);
    }
    other.drain_states().await;

    // The stalled client's replies stopped at what the socket buffers hold:
    // the server did not render all 64 ahead of the writer.
    let calls = provider.png_calls();
    assert!(calls < 64, "{calls} icons rendered for a stalled client");
    assert_eq!(clients.borrow_and_update().len(), 2);
    server.stop().await;
}

#[tokio::test]
async fn provider_calls_are_capped_across_clients() {
    let provider = TestProvider::new(&DEMO_NAMES);
    let gate = provider.close_gate();
    let (_mock, server) = start(Some(provider.clone())).await;
    let firefox = TestProvider::id_for("Firefox");

    // Clients that request and then hang up leave their provider calls
    // running.
    let mut leaving = Vec::new();
    for _ in 0..10 {
        let mut client = Client::connect(server.local_addr()).await;
        client.read_state().await;
        client.send(&icon_request(&[firefox.as_str()], None)).await;
        leaving.push(client);
    }
    timeout(TIMEOUT, async {
        while provider.png_calls() < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("provider calls did not start");
    drop(leaving);

    let mut waiting = Client::connect(server.local_addr()).await;
    waiting.read_state().await;
    waiting.send(&icon_request(&[firefox.as_str()], None)).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(provider.png_calls(), 4);

    drop(gate);
    let reply = waiting.read_icon().await;
    let (id, _, _) = image(&reply);
    assert_eq!(id, firefox.as_str());
    server.stop().await;
}
