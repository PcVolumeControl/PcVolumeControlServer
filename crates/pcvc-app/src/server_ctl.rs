//! Owns the [`ServerHandle`] on a tokio task so starts, stops and restarts
//! are serialised, and forwards everything the server reports to the inbox.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use pcvc_audio::AudioBackend;
use pcvc_server::{ClientInfo, MdnsStatus, ServerConfig, ServerEvent, ServerHandle};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};

use crate::inbox::{Inbound, Inbox};

/// Upper bound on a graceful stop (the mDNS goodbye is the slow part).
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running(SocketAddr),
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum ServerUpdate {
    Status(ServerStatus),
    Clients(Vec<ClientInfo>),
    Mdns(MdnsStatus),
    Event(ServerEvent),
    /// The server is stopped for good after [`ServerController::shutdown`].
    ShutDown,
}

#[derive(Debug)]
enum Request {
    /// Starts the server, restarting it if it runs.
    Start(ServerConfig),
    Stop,
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct ServerController(mpsc::UnboundedSender<Request>);

impl ServerController {
    /// Spawns the controller task; it must run inside a tokio runtime.
    pub fn spawn(backend: Arc<dyn AudioBackend>, inbox: Inbox) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(backend, rx, inbox));
        Self(tx)
    }

    pub fn start(&self, config: ServerConfig) {
        self.request(Request::Start(config));
    }

    pub fn stop(&self) {
        self.request(Request::Stop);
    }

    /// Stops the server and ends the controller; the inbox then receives
    /// [`ServerUpdate::ShutDown`].
    pub fn shutdown(&self) {
        self.request(Request::Shutdown);
    }

    fn request(&self, request: Request) {
        if let Err(e) = self.0.send(request) {
            tracing::error!("server controller is gone, dropped {:?}", e.0);
        }
    }
}

struct Running {
    handle: ServerHandle,
    clients: watch::Receiver<Vec<ClientInfo>>,
    mdns: watch::Receiver<MdnsStatus>,
    events: broadcast::Receiver<ServerEvent>,
}

impl Running {
    async fn next_update(&mut self) -> ServerUpdate {
        loop {
            tokio::select! {
                Ok(()) = self.clients.changed() => {
                    return ServerUpdate::Clients(self.clients.borrow_and_update().clone());
                }
                Ok(()) = self.mdns.changed() => {
                    return ServerUpdate::Mdns(self.mdns.borrow_and_update().clone());
                }
                event = self.events.recv() => match event {
                    Ok(event) => return ServerUpdate::Event(event),
                    Err(RecvError::Lagged(n)) => tracing::debug!("missed {n} server events"),
                    Err(RecvError::Closed) => std::future::pending::<()>().await,
                },
            }
        }
    }
}

async fn run(
    backend: Arc<dyn AudioBackend>,
    mut requests: mpsc::UnboundedReceiver<Request>,
    inbox: Inbox,
) {
    let send = |update| {
        inbox.send(Inbound::Server(update));
    };
    let mut running: Option<Running> = None;
    loop {
        let request = match running.as_mut() {
            Some(server) => tokio::select! {
                request = requests.recv() => request,
                update = server.next_update() => {
                    send(update);
                    continue;
                }
            },
            None => requests.recv().await,
        };
        match request {
            Some(Request::Start(config)) => {
                stop(&mut running, &send).await;
                send(ServerUpdate::Status(ServerStatus::Starting));
                match ServerHandle::start(Arc::clone(&backend), config).await {
                    Ok(handle) => {
                        let server = Running {
                            clients: handle.clients(),
                            mdns: handle.mdns(),
                            events: handle.events(),
                            handle,
                        };
                        send(ServerUpdate::Status(ServerStatus::Running(
                            server.handle.local_addr(),
                        )));
                        send(ServerUpdate::Clients(server.clients.borrow().clone()));
                        send(ServerUpdate::Mdns(server.mdns.borrow().clone()));
                        running = Some(server);
                    }
                    Err(e) => {
                        tracing::error!("{e}");
                        send(ServerUpdate::Status(ServerStatus::Failed(e.to_string())));
                    }
                }
            }
            Some(Request::Stop) => stop(&mut running, &send).await,
            Some(Request::Shutdown) | None => {
                stop(&mut running, &send).await;
                send(ServerUpdate::ShutDown);
                return;
            }
        }
    }
}

async fn stop(running: &mut Option<Running>, send: &impl Fn(ServerUpdate)) {
    let Some(server) = running.take() else {
        return;
    };
    if tokio::time::timeout(STOP_TIMEOUT, server.handle.stop())
        .await
        .is_err()
    {
        tracing::warn!("server did not stop within {STOP_TIMEOUT:?}");
    }
    send(ServerUpdate::Clients(Vec::new()));
    send(ServerUpdate::Mdns(MdnsStatus::Disabled));
    send(ServerUpdate::Status(ServerStatus::Stopped));
}
