//! The provider behind a real server, spoken to over TCP.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use pcvc_audio::{AppHint, AudioBackend, Device, MockBackend};
use pcvc_protocol::{FullState, IconMessage, IconReply};
use pcvc_server::{ServerConfig, ServerHandle};
use resvg::tiny_skia::Pixmap;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::timeout;

use super::tests::{FakeResolver, session};
use super::*;

const TIMEOUT: Duration = Duration::from_secs(5);
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];

fn backend() -> Arc<dyn AudioBackend> {
    let device = Device::new("dev", "Speakers", 50.0)
        .with_session(session("Alpha", "vector"))
        .with_session(session("Beta", "raster"))
        .with_session(Session::new("dev|Gamma", "Gamma", 50.0).with_app(AppHint {
            process_binary: Some(Arc::from("gamma")),
            ..AppHint::default()
        }));
    Arc::new(MockBackend::new(vec![device], Some("dev")).unwrap())
}

fn icons() -> Option<Arc<dyn IconProvider>> {
    let resolver: Arc<dyn Resolver> =
        FakeResolver::new(&[("vector", RED, None), ("raster", GREEN, Some(32))]);
    for_server(true, &resolver)
}

struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = timeout(TIMEOUT, TcpStream::connect(addr))
            .await
            .unwrap()
            .unwrap();
        let (read, write) = stream.into_split();
        Self {
            lines: BufReader::new(read).lines(),
            write,
        }
    }

    async fn line(&mut self) -> String {
        timeout(TIMEOUT, self.lines.next_line())
            .await
            .expect("a line in time")
            .unwrap()
            .expect("an open connection")
    }

    /// Reads full states until every session named in `names` has an icon
    /// id, returning the ids by session name.
    async fn icon_ids(&mut self, names: &[&str]) -> (FullState, HashMap<String, IconId>) {
        loop {
            let state: FullState = serde_json::from_str(&self.line().await).unwrap();
            let ids: HashMap<String, IconId> = state
                .default_device
                .sessions
                .iter()
                .filter_map(|s| Some((s.name.clone(), s.icon_id.clone()?)))
                .collect();
            if names.iter().all(|n| ids.contains_key(*n)) {
                return (state, ids);
            }
        }
    }

    async fn send(&mut self, line: String) {
        self.write.write_all(line.as_bytes()).await.unwrap();
        self.write.write_all(b"\n").await.unwrap();
    }

    /// The next `count` icon replies, skipping full states.
    async fn replies(&mut self, count: usize) -> Vec<IconReply> {
        let mut replies = Vec::new();
        while replies.len() < count {
            let line = self.line().await;
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("icon").is_some() {
                let message: IconMessage = serde_json::from_value(value).unwrap();
                replies.push(message.icon);
            }
        }
        replies
    }
}

fn decode(reply: &IconReply) -> (u32, Pixmap) {
    let IconReply::Image {
        size, mime, data, ..
    } = reply
    else {
        panic!("not an image: {reply:?}");
    };
    assert_eq!(mime, "image/png");
    let pixmap = Pixmap::decode_png(&BASE64.decode(data).unwrap()).unwrap();
    assert_eq!((pixmap.width(), pixmap.height()), (*size, *size));
    (*size, pixmap)
}

fn config() -> ServerConfig {
    ServerConfig::new(Ipv4Addr::LOCALHOST.into(), 0).with_mdns(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phone_clients_get_icons() {
    let server = ServerHandle::start(backend(), config().with_icons(icons().unwrap()))
        .await
        .unwrap();
    let mut client = Client::connect(server.local_addr()).await;
    let (state, ids) = client.icon_ids(&["Alpha", "Beta"]).await;
    assert_eq!(state.features, ["icons"]);
    assert!(!ids.contains_key("Gamma"), "no icon found for it");

    let request = serde_json::json!({
        "protocolVersion": 7,
        "iconRequest": {
            "ids": [ids["Alpha"], ids["Beta"], "0123456789abcdef"],
            "size": 128,
        },
    });
    client.send(request.to_string()).await;
    let replies = client.replies(3).await;
    assert_eq!(replies[0].id(), ids["Alpha"].as_str());
    let (size, pixmap) = decode(&replies[0]);
    assert_eq!(size, 128);
    let p = pixmap.pixel(64, 64).unwrap();
    assert_eq!((p.red(), p.green(), p.blue()), (255, 0, 0));
    assert_eq!(
        decode(&replies[1]).0,
        64,
        "a 32 px raster is not scaled up to 128"
    );
    assert!(matches!(&replies[2], IconReply::Error { error, .. } if error == "notFound"));
    server.stop().await;
}

/// The controller gives the provider and the details switch to the first
/// start and to restarts.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_server_start_serves_icons() {
    use iced::futures::StreamExt;

    use crate::inbox::{self, Inbound};
    use crate::server_ctl::{ServerController, ServerStatus, ServerUpdate};

    for (icons, details, expected) in [
        (icons(), false, vec!["icons"]),
        (icons(), true, vec!["icons", "detail"]),
        (None, false, vec![]),
    ] {
        let (inbox, receiver) = inbox::channel();
        let mut updates = receiver.take_stream().unwrap();
        let controller = ServerController::spawn(backend(), icons, details, inbox);
        for _ in 0..2 {
            controller.start(config());
            let addr = loop {
                let update = timeout(TIMEOUT, updates.next()).await.unwrap().unwrap();
                if let Inbound::Server(ServerUpdate::Status(ServerStatus::Running(addr))) = update {
                    break addr;
                }
            };
            let mut client = Client::connect(addr).await;
            let line = client.line().await;
            let state: FullState = serde_json::from_str(&line).unwrap();
            assert_eq!(state.features, expected);
            if expected.is_empty() {
                assert!(!line.contains("features") && !line.contains("iconId"));
            }
        }
        controller.shutdown();
    }
}
