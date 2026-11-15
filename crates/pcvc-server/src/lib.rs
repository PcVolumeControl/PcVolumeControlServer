//! PcVolumeControl host server.
//!
//! [`ServerHandle::start`] binds a TCP listener and serves protocol v7 clients
//! from an [`AudioBackend`]: every client gets the full state on connect and
//! again after every backend change, and client updates are turned into
//! backend commands. The handle exposes the current audio state, the client
//! list, the mDNS status and an event stream for a GUI or a headless front
//! end.
//!
//! With an [`IconProvider`] in the [`ServerConfig`], the server also speaks
//! the v8 icon extension: it advertises the `icons` feature, tags sessions
//! with icon ids and answers icon requests. Without one, its output is plain
//! v7.

mod broadcaster;
mod client;
mod icons;
mod mdns;
mod state;

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use pcvc_audio::{AudioBackend, AudioState};
use pcvc_protocol::{FEATURE_ICONS, FullState};
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use icons::{IconImage, IconProvider};
pub use mdns::SERVICE_TYPE as MDNS_SERVICE_TYPE;
pub use pcvc_protocol::IconId;
pub use state::to_full_state;

/// Default TCP port, as used by the shipped clients.
pub const DEFAULT_PORT: u16 = 3000;

/// How long backend events are gathered before the state is rebuilt and
/// broadcast.
pub const DEFAULT_COALESCE: Duration = Duration::from_millis(15);

const EVENT_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct ServerConfig {
    pub bind: IpAddr,
    pub port: u16,
    /// Advertise the service over mDNS while the listener runs.
    pub mdns: bool,
    pub coalesce: Duration,
    /// Serves app icons (the v8 `icons` feature) when set.
    pub icons: Option<Arc<dyn IconProvider>>,
}

impl ServerConfig {
    /// A config with mDNS enabled, the default coalescing window and no icon
    /// provider.
    pub fn new(bind: IpAddr, port: u16) -> Self {
        Self {
            bind,
            port,
            mdns: true,
            coalesce: DEFAULT_COALESCE,
            icons: None,
        }
    }

    pub fn with_mdns(mut self, enabled: bool) -> Self {
        self.mdns = enabled;
        self
    }

    pub fn with_icons(mut self, provider: Arc<dyn IconProvider>) -> Self {
        self.icons = Some(provider);
        self
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("mdns", &self.mdns)
            .field("coalesce", &self.coalesce)
            .field("icons", &self.icons.is_some())
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdnsStatus {
    /// Not requested, or withdrawn.
    Disabled,
    /// Registration requested; the service has not been announced yet.
    Starting,
    /// Announced under this instance name, which differs from the requested
    /// one if the mDNS stack had to resolve a name conflict.
    Registered {
        instance: String,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// Unique for the lifetime of the server.
    pub id: u64,
    pub addr: SocketAddr,
    pub connected_since: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The client closed the connection.
    Closed,
    /// The client sent malformed input or the wrong protocol version.
    Protocol(String),
    Io(String),
    /// Writing a state to the client did not finish in time.
    WriteTimeout,
    ServerStopped,
}

/// Things a front end may want to show or log. Only events after
/// [`ServerHandle::start`] returns can be observed.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    ClientConnected(ClientInfo),
    ClientDisconnected {
        client: ClientInfo,
        reason: DisconnectReason,
    },
    /// Reading the state or applying a client command failed, or the backend's
    /// event stream ended.
    BackendError(String),
    Stopped,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to bind {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
}

/// The last published full state and its encoded line.
struct Frame {
    state: FullState,
    line: String,
}

/// State shared by the server's tasks.
struct Shared {
    backend: Arc<dyn AudioBackend>,
    icons: Option<Arc<dyn IconProvider>>,
    /// `None` while there is no default device. Each client's writer follows
    /// this channel, so a slow client skips intermediate states rather than
    /// queueing them.
    frames: watch::Sender<Option<Arc<Frame>>>,
    state: watch::Sender<AudioState>,
    /// Asks the broadcaster to re-send the state even if it has not changed.
    resync: Notify,
    clients: watch::Sender<Vec<ClientInfo>>,
    mdns: watch::Sender<MdnsStatus>,
    events: broadcast::Sender<ServerEvent>,
    next_client_id: AtomicU64,
    cancel: CancellationToken,
}

impl Shared {
    fn emit(&self, event: ServerEvent) {
        // An error only means nobody is subscribed.
        let _ = self.events.send(event);
    }

    /// Protocol extensions this server advertises.
    fn features(&self) -> Vec<&'static str> {
        self.icons.iter().map(|_| FEATURE_ICONS).collect()
    }
}

/// A running server. Dropping the handle stops the server without waiting for
/// its tasks (the mDNS withdrawal still runs if the runtime stays up);
/// [`ServerHandle::stop`] waits.
pub struct ServerHandle {
    shared: Arc<Shared>,
    tracker: TaskTracker,
    local_addr: SocketAddr,
}

impl ServerHandle {
    /// Binds the listener, takes the initial snapshot and starts serving.
    pub async fn start(
        backend: Arc<dyn AudioBackend>,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        let addr = SocketAddr::new(config.bind, config.port);
        let bind_err = |source| ServerError::Bind { addr, source };
        let listener = TcpListener::bind(addr).await.map_err(bind_err)?;
        let local_addr = listener.local_addr().map_err(bind_err)?;

        // Subscribe before the first snapshot so no change falls in between.
        let audio_events = backend.subscribe();
        let icon_events = config.icons.as_ref().map(|icons| icons.subscribe());
        let shared = Arc::new(Shared {
            backend,
            icons: config.icons,
            frames: watch::Sender::new(None),
            state: watch::Sender::new(AudioState::default()),
            resync: Notify::new(),
            clients: watch::Sender::new(Vec::new()),
            mdns: watch::Sender::new(MdnsStatus::Disabled),
            events: broadcast::Sender::new(EVENT_CAPACITY),
            next_client_id: AtomicU64::new(0),
            cancel: CancellationToken::new(),
        });
        let initial_ok = broadcaster::rebuild(&shared, true).await;

        let tracker = TaskTracker::new();
        tracker.spawn(broadcaster::run(
            Arc::clone(&shared),
            audio_events,
            icon_events,
            config.coalesce,
            initial_ok,
        ));
        tracker.spawn(accept_loop(Arc::clone(&shared), listener, tracker.clone()));
        if config.mdns {
            tracker.spawn(mdns::run(
                Arc::clone(&shared),
                config.bind,
                local_addr.port(),
            ));
        }

        tracing::info!(%local_addr, "server listening");
        Ok(Self {
            shared,
            tracker,
            local_addr,
        })
    }

    /// The bound address, useful when started on port 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The latest backend snapshot, updated after every coalesced change.
    pub fn state(&self) -> watch::Receiver<AudioState> {
        self.shared.state.subscribe()
    }

    /// Currently connected clients.
    pub fn clients(&self) -> watch::Receiver<Vec<ClientInfo>> {
        self.shared.clients.subscribe()
    }

    /// mDNS advertisement state.
    pub fn mdns(&self) -> watch::Receiver<MdnsStatus> {
        self.shared.mdns.subscribe()
    }

    pub fn events(&self) -> broadcast::Receiver<ServerEvent> {
        self.shared.events.subscribe()
    }

    /// Stops listening, disconnects every client, withdraws the mDNS
    /// advertisement and waits for all tasks.
    pub async fn stop(self) {
        self.shared.cancel.cancel();
        self.tracker.close();
        self.tracker.wait().await;
        tracing::info!(local_addr = %self.local_addr, "server stopped");
        self.shared.emit(ServerEvent::Stopped);
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

async fn accept_loop(shared: Arc<Shared>, listener: TcpListener, tracker: TaskTracker) {
    loop {
        let accepted = tokio::select! {
            () = shared.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, addr)) => {
                let info = ClientInfo {
                    id: shared.next_client_id.fetch_add(1, Ordering::Relaxed),
                    addr,
                    connected_since: SystemTime::now(),
                };
                tracker.spawn(client::run(Arc::clone(&shared), stream, info));
            }
            Err(e) => {
                // Usually descriptor exhaustion; back off instead of spinning.
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
