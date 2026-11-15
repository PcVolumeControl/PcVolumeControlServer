//! PulseAudio backend, also serving PipeWire through `pipewire-pulse`.
//!
//! Sinks are devices (id = sink name, name = description), the server's
//! default sink is the default device, and the user streams (sink-inputs) on
//! it are its sessions, with ids of the form `"{sink name}|{sink-input
//! index}"`. A worker thread owns the main loop and the connection; the
//! public [`PulseBackend`] talks to it over channels and serves snapshots
//! from a cache the worker keeps up to date.

mod conn;
mod model;
mod worker;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pcvc_protocol::Command;
use tokio::sync::broadcast;

use crate::{AudioBackend, AudioError, AudioEvent, AudioState};

const EVENT_CAPACITY: usize = 64;

/// How long the worker may spend on a command, counted from when it was
/// requested. A command still queued after this is dropped unapplied.
const COMMAND_BUDGET: Duration = Duration::from_secs(4);

/// How long a caller waits for the worker's answer. Longer than
/// [`COMMAND_BUDGET`], so a command never takes effect after its caller was
/// told it failed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// State shared between the worker and [`PulseBackend`].
struct Shared {
    state: Mutex<AudioState>,
    events: broadcast::Sender<AudioEvent>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, AudioState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

enum Request {
    Command {
        command: Command,
        deadline: Instant,
        reply: mpsc::Sender<Result<(), AudioError>>,
    },
    Shutdown,
}

/// [`AudioBackend`] for PulseAudio and PipeWire (via `pipewire-pulse`).
///
/// [`snapshot`](AudioBackend::snapshot) never blocks on the sound server: it
/// returns a cache that is refreshed whenever the server reports a change, so
/// the effect of a command shows up shortly after the call returns, together
/// with the matching [`AudioEvent`]s. If the connection drops, the worker
/// reconnects with backoff; meanwhile snapshots return the last known state
/// and commands fail.
pub struct PulseBackend {
    shared: Arc<Shared>,
    requests: mpsc::Sender<Request>,
    /// Writing a byte wakes the worker's main loop.
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl PulseBackend {
    /// Connects to the user's sound server and reads the initial state. Fails
    /// if the server cannot be reached.
    pub fn new() -> Result<Self, AudioError> {
        let shared = Arc::new(Shared {
            state: Mutex::new(AudioState::default()),
            events: broadcast::channel(EVENT_CAPACITY).0,
        });
        let (wake, wake_rx) = UnixStream::pair().map_err(backend_error)?;
        wake.set_nonblocking(true).map_err(backend_error)?;
        wake_rx.set_nonblocking(true).map_err(backend_error)?;
        let (requests, requests_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("pulse".into())
            .spawn(move || worker::run(worker_shared, requests_rx, wake_rx, ready_tx))
            .map_err(backend_error)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                requests,
                wake,
                worker: Some(worker),
            }),
            Ok(Err(e)) => {
                let _ = worker.join();
                Err(e)
            }
            Err(_) => {
                let _ = worker.join();
                Err(backend_error("PulseAudio worker exited during startup"))
            }
        }
    }

    fn wake(&self) {
        // A full buffer means a wake-up is already pending.
        let _ = (&self.wake).write(&[1]);
    }

    fn request(&self, command: Command) -> Result<(), AudioError> {
        let (reply, reply_rx) = mpsc::channel();
        self.requests
            .send(Request::Command {
                command,
                deadline: Instant::now() + COMMAND_BUDGET,
                reply,
            })
            .map_err(|_| backend_error("PulseAudio worker has stopped"))?;
        self.wake();
        match reply_rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(backend_error("timed out waiting for the PulseAudio worker"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(backend_error("PulseAudio worker has stopped"))
            }
        }
    }
}

impl Drop for PulseBackend {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        self.wake();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl std::fmt::Debug for PulseBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulseBackend").finish_non_exhaustive()
    }
}

fn backend_error(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> AudioError {
    AudioError::Backend(e.into())
}

impl AudioBackend for PulseBackend {
    fn snapshot(&self) -> Result<AudioState, AudioError> {
        Ok(self.shared.lock().clone())
    }

    fn set_default_device(&self, device_id: &str) -> Result<(), AudioError> {
        self.request(Command::SetDefaultDevice {
            device_id: device_id.to_owned(),
        })
    }

    fn set_master(&self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError> {
        self.request(Command::SetMaster { volume, muted })
    }

    fn set_session(&self, session_id: &str, volume: f32, muted: bool) -> Result<(), AudioError> {
        self.request(Command::SetSession {
            session_id: session_id.to_owned(),
            volume,
            muted,
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.shared.events.subscribe()
    }
}
