use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use pcvc_audio::{AudioBackend, MockBackend, Session};
use pcvc_protocol::{APPLICATION_VERSION, FullState, MAX_LINE_LEN};
use pcvc_server::{DisconnectReason, ServerConfig, ServerEvent, ServerHandle};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(5);
const SPEAKERS: &str = "0f4090a9-dee2-4563-ba29-0ad6b93d9e22";
const HEADSET: &str = "c98e4030-926b-4e62-8c83-1c529574cc51";

async fn start() -> (Arc<MockBackend>, ServerHandle) {
    let mock = Arc::new(MockBackend::demo());
    let backend: Arc<dyn AudioBackend> = mock.clone();
    let config = ServerConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).with_mdns(false);
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

    /// Sends a line the server is expected to hang up on. The server may
    /// close the socket before the whole line is written, so write errors
    /// are ignored.
    async fn send_rejected(&mut self, line: &str) {
        let _ = self.writer.write_all(line.as_bytes()).await;
        let _ = self.writer.write_all(b"\n").await;
    }

    async fn read_state(&mut self) -> FullState {
        let mut line = String::new();
        let n = timeout(TIMEOUT, self.reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a state")
            .unwrap();
        assert!(n > 0, "connection closed");
        assert!(line.ends_with('\n'));
        assert!(!line.contains("features"), "{line}");
        let state: FullState = serde_json::from_str(&line).unwrap();
        assert_eq!(state.protocol_version, 8);
        state
    }

    /// Reads states until one satisfies `pred`; earlier broadcasts may still be
    /// in flight.
    async fn read_until(&mut self, mut pred: impl FnMut(&FullState) -> bool) -> FullState {
        timeout(TIMEOUT, async {
            loop {
                let state = self.read_state().await;
                if pred(&state) {
                    return state;
                }
            }
        })
        .await
        .expect("timed out waiting for a matching state")
    }

    /// Reads (and discards) until the server closes the connection.
    async fn assert_closed(&mut self) {
        let mut buf = Vec::new();
        match timeout(TIMEOUT, self.reader.read_to_end(&mut buf))
            .await
            .expect("server did not close the connection")
        {
            Ok(_) => {}
            // A close with unread input pending surfaces as a reset on Unix
            // and as an abort on Windows.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}

fn session<'a>(state: &'a FullState, name: &str) -> &'a pcvc_protocol::SessionState {
    state
        .default_device
        .sessions
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no session {name}"))
}

#[tokio::test]
async fn new_client_gets_full_state() {
    let (_mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    let state = client.read_state().await;
    assert_eq!(state.application_version, APPLICATION_VERSION);
    assert_eq!(state.device_ids.len(), 3);
    assert_eq!(state.default_device.device_id, SPEAKERS);
    assert_eq!(state.default_device.master_volume, 80.0);
    assert_eq!(state.default_device.sessions.len(), 4);
    // Non-ASCII names arrive intact as UTF-8.
    session(&state, "Musique \u{e9}t\u{e9}");
    server.stop().await;
}

#[tokio::test]
async fn session_update_is_applied_and_broadcast() {
    let (_mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    let state = client.read_state().await;
    let spotify = session(&state, "Spotify").id.clone();

    // As the Swift client sends it: with a name, integral volume.
    let update = serde_json::json!({
        "protocolVersion": 8,
        "defaultDevice": {
            "sessions": [{ "name": "Spotify", "id": spotify, "volume": 12, "muted": true }],
            "deviceId": SPEAKERS
        }
    });
    client.send(&update.to_string()).await;
    let state = client
        .read_until(|s| session(s, "Spotify").volume == 12.0)
        .await;
    assert!(session(&state, "Spotify").muted);
    assert_eq!(session(&state, "Firefox").volume, 100.0);
    server.stop().await;
}

#[tokio::test]
async fn master_update_is_applied_and_broadcast() {
    let (_mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    client
        .send(&format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterMuted":true,"masterVolume":33.5}}}}"#
        ))
        .await;
    let state = client
        .read_until(|s| s.default_device.master_volume == 33.5)
        .await;
    assert!(state.default_device.master_muted);
    server.stop().await;
}

#[tokio::test]
async fn default_device_switch_is_applied_and_broadcast() {
    let (_mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    client
        .send(&format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#
        ))
        .await;
    let state = client
        .read_until(|s| s.default_device.device_id == HEADSET)
        .await;
    assert_eq!(state.default_device.name, "Headset (Mock USB Audio)");
    assert_eq!(state.default_device.sessions.len(), 1);
    assert_eq!(state.default_device.sessions[0].name, "Zoom");
    server.stop().await;
}

#[tokio::test]
async fn rejected_update_triggers_resync() {
    let (_mock, server) = start().await;
    let mut events = server.events();
    let mut client = Client::connect(server.local_addr()).await;
    let before = client.read_state().await;

    client
        .send(r#"{"protocolVersion":8,"defaultDevice":{"deviceId":"no-such-device"}}"#)
        .await;
    assert_eq!(client.read_state().await, before);
    assert!(matches!(
        events.recv().await.unwrap(),
        ServerEvent::ClientConnected(_)
    ));
    assert!(matches!(
        events.recv().await.unwrap(),
        ServerEvent::BackendError(_)
    ));
    server.stop().await;
}

#[tokio::test]
async fn unchanged_state_is_not_resent() {
    let (mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    let state = client.read_state().await;

    // Replacing a device with an identical copy emits events but changes
    // nothing visible.
    let device = mock.snapshot().unwrap().default_device.unwrap();
    mock.add_device(device).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    mock.set_master(Some(1.0), None).unwrap();
    let next = client.read_state().await;
    assert_eq!(next.default_device.master_volume, 1.0);
    assert_ne!(next, state);
    server.stop().await;
}

#[tokio::test]
async fn backend_changes_are_broadcast() {
    let (mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    let id = MockBackend::session_id(SPEAKERS, "vlc");
    mock.add_session(SPEAKERS, Session::new(id, "VLC", 20.0))
        .unwrap();
    let state = client
        .read_until(|s| s.default_device.sessions.iter().any(|s| s.name == "VLC"))
        .await;
    assert_eq!(session(&state, "VLC").volume, 20.0);
    server.stop().await;
}

#[tokio::test]
async fn session_ids_need_not_contain_the_device_id() {
    let (mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    mock.add_session(SPEAKERS, Session::new("opaque-42", "VLC", 20.0))
        .unwrap();
    client
        .read_until(|s| {
            s.default_device
                .sessions
                .iter()
                .any(|s| s.id == "opaque-42")
        })
        .await;
    let update = serde_json::json!({
        "protocolVersion": 8,
        "defaultDevice": {
            "deviceId": SPEAKERS,
            "sessions": [{ "id": "opaque-42", "volume": 55.0, "muted": true }]
        }
    });
    client.send(&update.to_string()).await;
    let state = client
        .read_until(|s| session(s, "VLC").volume == 55.0)
        .await;
    assert!(session(&state, "VLC").muted);
    server.stop().await;
}

#[tokio::test]
async fn bursts_are_coalesced() {
    let (mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;

    for volume in 1..=50 {
        mock.set_master(Some(volume as f32), None).unwrap();
    }
    let mut received = 0;
    client
        .read_until(|s| {
            received += 1;
            s.default_device.master_volume == 50.0
        })
        .await;
    assert!(received < 50, "{received} states for 50 changes");
    server.stop().await;
}

#[tokio::test]
async fn all_clients_receive_broadcasts() {
    let (_mock, server) = start().await;
    let mut a = Client::connect(server.local_addr()).await;
    let mut b = Client::connect(server.local_addr()).await;
    a.read_state().await;
    b.read_state().await;

    a.send(&format!(
        r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterVolume":5.0}}}}"#
    ))
    .await;
    a.read_until(|s| s.default_device.master_volume == 5.0)
        .await;
    b.read_until(|s| s.default_device.master_volume == 5.0)
        .await;
    server.stop().await;
}

#[tokio::test]
async fn garbage_closes_the_client() {
    let (_mock, server) = start().await;
    let mut events = server.events();
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    client.send("garbage").await;
    client.assert_closed().await;

    let reason = timeout(TIMEOUT, async {
        loop {
            if let ServerEvent::ClientDisconnected { reason, .. } = events.recv().await.unwrap() {
                return reason;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(reason, DisconnectReason::Protocol(_)),
        "{reason:?}"
    );
    server.stop().await;
}

#[tokio::test]
async fn wrong_version_closes_the_client() {
    let (_mock, server) = start().await;
    for line in [
        format!(r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#),
        format!(r#"{{"protocolVersion":7.0,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#),
        format!(r#"{{"protocolVersion":6,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#),
        format!(r#"{{"version":8,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#),
    ] {
        let mut client = Client::connect(server.local_addr()).await;
        client.read_state().await;
        client.send(&line).await;
        client.assert_closed().await;
    }
    // Neither message was applied.
    let mut client = Client::connect(server.local_addr()).await;
    assert_eq!(client.read_state().await.default_device.device_id, SPEAKERS);
    server.stop().await;
}

#[tokio::test]
async fn overlong_line_closes_the_client() {
    let (_mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    client.send_rejected(&"x".repeat(MAX_LINE_LEN + 1)).await;
    client.assert_closed().await;
    server.stop().await;
}

#[tokio::test]
async fn blank_lines_are_ignored() {
    let (_mock, server) = start().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.read_state().await;
    client.send("").await;
    client.send("\r").await;
    client
        .send(&format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterMuted":true}}}}"#
        ))
        .await;
    client.read_until(|s| s.default_device.master_muted).await;
    server.stop().await;
}

#[tokio::test]
async fn handle_tracks_clients_and_state() {
    let (mock, server) = start().await;
    let mut clients = server.clients();
    let mut state = server.state();
    assert_eq!(state.borrow().default_device.as_ref().unwrap().id, SPEAKERS);

    let client = Client::connect(server.local_addr()).await;
    let local = client.writer.local_addr().unwrap();
    let list = timeout(TIMEOUT, clients.wait_for(|c| c.len() == 1))
        .await
        .unwrap()
        .unwrap()
        .clone();
    assert_eq!(list[0].addr, local);

    mock.set_default_device(HEADSET).unwrap();
    timeout(
        TIMEOUT,
        state.wait_for(|s| s.default_device.as_ref().is_some_and(|d| d.id == HEADSET)),
    )
    .await
    .unwrap()
    .unwrap();

    drop(client);
    timeout(TIMEOUT, clients.wait_for(Vec::is_empty))
        .await
        .unwrap()
        .unwrap();
    server.stop().await;
}

#[tokio::test]
async fn stop_disconnects_clients_and_closes_listener() {
    let (_mock, server) = start().await;
    let addr = server.local_addr();
    let mut events = server.events();
    let mut client = Client::connect(addr).await;
    client.read_state().await;

    server.stop().await;
    client.assert_closed().await;
    assert!(TcpStream::connect(addr).await.is_err());

    let mut saw_stopped = false;
    while let Ok(event) = events.try_recv() {
        saw_stopped |= event == ServerEvent::Stopped;
    }
    assert!(saw_stopped);
}

#[tokio::test]
async fn no_default_device_sends_nothing_until_one_appears() {
    let mock = Arc::new(MockBackend::new(vec![], None).unwrap());
    let backend: Arc<dyn AudioBackend> = mock.clone();
    let config = ServerConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).with_mdns(false);
    let server = ServerHandle::start(backend, config).await.unwrap();
    let mut client = Client::connect(server.local_addr()).await;

    let mut line = String::new();
    assert!(
        timeout(
            Duration::from_millis(200),
            client.reader.read_line(&mut line)
        )
        .await
        .is_err(),
        "unexpected {line}"
    );

    mock.add_device(pcvc_audio::Device::new("dev", "Dev", 40.0))
        .unwrap();
    let state = client.read_state().await;
    assert_eq!(state.default_device.device_id, "dev");
    server.stop().await;
}

#[tokio::test]
async fn client_can_pick_a_default_device_when_there_is_none() {
    let devices = vec![pcvc_audio::Device::new("a", "A", 10.0)];
    let mock = Arc::new(MockBackend::new(devices, None).unwrap());
    let backend: Arc<dyn AudioBackend> = mock.clone();
    let config = ServerConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).with_mdns(false);
    let server = ServerHandle::start(backend, config).await.unwrap();
    let mut client = Client::connect(server.local_addr()).await;

    client
        .send(r#"{"protocolVersion":8,"defaultDevice":{"deviceId":"a"}}"#)
        .await;
    assert_eq!(client.read_state().await.default_device.device_id, "a");
    server.stop().await;
}
