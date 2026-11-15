use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::broadcast;

use super::worker::{self, Msg, Request, Shared};
use crate::{AudioBackend, AudioError, AudioEvent, AudioState};

/// How long [`WindowsBackend::new`] waits for the COM thread to start.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a command waits for the COM thread.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long dropping the backend waits for the COM thread to clean up.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// [`AudioBackend`] over Windows Core Audio.
///
/// A dedicated thread in the multithreaded COM apartment owns every Core Audio
/// object. It keeps a cached [`AudioState`], which [`snapshot`] returns
/// without a COM call, and refreshes it when Core Audio reports a change.
/// Commands are sent to that thread and wait for it. Dropping the backend
/// unregisters every callback and stops the thread.
///
/// Device ids are endpoint GUIDs (lowercase, no braces) and session ids are
/// Core Audio session instance identifiers, the formats the C# host used.
///
/// [`snapshot`]: AudioBackend::snapshot
pub struct WindowsBackend {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// Disconnects when the COM thread exits.
    exited: Mutex<Receiver<()>>,
}

impl WindowsBackend {
    /// Starts the COM thread and reads the initial state.
    pub fn new() -> Result<Self, AudioError> {
        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::sync_channel(1);
        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("pcvc-core-audio".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let tx = tx.clone();
                move || {
                    // Dropped last, also when unwinding.
                    let _exit = exit_tx;
                    worker::run(shared, tx, rx, init_tx);
                }
            })
            .map_err(|e| AudioError::Backend(Box::new(e)))?;
        match init_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                tx,
                shared,
                thread: Some(thread),
                exited: Mutex::new(exit_rx),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(RecvTimeoutError::Timeout) => {
                // The thread is stuck in COM; leave it to exit on its own.
                let _ = tx.send(Msg::Shutdown);
                Err(AudioError::Backend(
                    "timed out starting the Core Audio thread".into(),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(AudioError::Backend("the Core Audio thread panicked".into()))
            }
        }
    }

    fn request(&self, request: Request) -> Result<(), AudioError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.tx
            .send(Msg::Command(request, reply_tx))
            .map_err(|_| stopped())?;
        match reply_rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(AudioError::Backend(
                "timed out waiting for the Core Audio thread".into(),
            )),
            Err(RecvTimeoutError::Disconnected) => Err(stopped()),
        }
    }
}

fn stopped() -> AudioError {
    AudioError::Backend("the Core Audio thread has stopped".into())
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        let exited = self
            .exited
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        match exited.recv_timeout(SHUTDOWN_TIMEOUT) {
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
            // Stuck in a COM call: leave the thread to finish on its own
            // rather than hang whoever drops the backend.
            Ok(()) | Err(RecvTimeoutError::Timeout) => {
                tracing::warn!("Core Audio thread did not stop in time, detaching it");
            }
        }
    }
}

impl AudioBackend for WindowsBackend {
    fn snapshot(&self) -> Result<AudioState, AudioError> {
        if !self.shared.is_alive() {
            return Err(stopped());
        }
        Ok(self.shared.state().clone())
    }

    fn set_default_device(&self, device_id: &str) -> Result<(), AudioError> {
        self.request(Request::DefaultDevice {
            device_id: device_id.to_owned(),
        })
    }

    fn set_master(&self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError> {
        self.request(Request::Master { volume, muted })
    }

    fn set_session(&self, session_id: &str, volume: f32, muted: bool) -> Result<(), AudioError> {
        self.request(Request::Session {
            session_id: session_id.to_owned(),
            volume,
            muted,
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.shared.events.subscribe()
    }
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WindowsBackend>();
};
