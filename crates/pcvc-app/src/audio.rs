//! Audio backend selection, the backend watcher and the command worker.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use pcvc_audio::{AudioBackend, MockBackend};
use pcvc_protocol::Command;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::inbox::{Inbound, Inbox};

/// Which audio backend to run the app against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// The platform's audio system.
    System,
    /// The scripted [`MockBackend`].
    Mock,
}

/// Creates the backend for `choice`. When the system backend cannot start,
/// the app runs with no devices so it never presents the mock's scripted
/// sessions as real ones.
pub fn create_backend(choice: BackendChoice) -> Arc<dyn AudioBackend> {
    match choice {
        BackendChoice::Mock => {
            tracing::info!("using the mock audio backend");
            Arc::new(MockBackend::demo())
        }
        BackendChoice::System => match system_backend() {
            Ok(backend) => backend,
            Err(e) => {
                tracing::error!("the system audio backend failed to start: {e}");
                Arc::new(MockBackend::new(Vec::new(), None).expect("an empty mock is always valid"))
            }
        },
    }
}

/// How many times the system backend is tried at startup, one second apart.
/// At login the app can start before the sound server accepts connections.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const SYSTEM_BACKEND_ATTEMPTS: u32 = 5;

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn system_backend() -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    let mut attempt = 1;
    loop {
        match open_system_backend() {
            Ok(backend) => return Ok(backend),
            Err(e) if attempt < SYSTEM_BACKEND_ATTEMPTS => {
                tracing::warn!("system audio backend not ready (attempt {attempt}): {e}");
                attempt += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(target_os = "linux")]
fn open_system_backend() -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    tracing::info!("using the PulseAudio backend");
    Ok(Arc::new(pcvc_audio::PulseBackend::new()?))
}

#[cfg(target_os = "windows")]
fn open_system_backend() -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    tracing::info!("using the Core Audio backend");
    Ok(Arc::new(pcvc_audio::WindowsBackend::new()?))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn system_backend() -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    tracing::warn!("no system audio backend for this platform; using the mock backend");
    Ok(Arc::new(MockBackend::demo()))
}

/// How long backend events are gathered before the state is re-read.
const COALESCE: Duration = Duration::from_millis(15);

/// Forwards a fresh snapshot to the inbox on start and after every backend
/// change, independently of whether the server runs.
pub fn spawn_watcher(backend: Arc<dyn AudioBackend>, inbox: Inbox) {
    let mut events = backend.subscribe();
    tokio::spawn(async move {
        loop {
            let snapshot = {
                let backend = Arc::clone(&backend);
                tokio::task::spawn_blocking(move || backend.snapshot()).await
            };
            match snapshot {
                Ok(Ok(state)) => {
                    if !inbox.send(Inbound::Audio(state)) {
                        return;
                    }
                }
                Ok(Err(e)) => tracing::warn!("reading the audio state failed: {e}"),
                Err(e) => tracing::error!("audio snapshot task failed: {e}"),
            }
            match events.recv().await {
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
            tokio::time::sleep(COALESCE).await;
            while let Ok(_) | Err(TryRecvError::Lagged(_)) = events.try_recv() {}
        }
    });
}

/// Applies commands to the backend in order on a dedicated thread, so a
/// blocking backend call never stalls the UI and later volumes never overtake
/// earlier ones.
#[derive(Debug, Clone)]
pub struct CommandSender(mpsc::Sender<Command>);

impl CommandSender {
    pub fn spawn(backend: Arc<dyn AudioBackend>) -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        std::thread::Builder::new()
            .name("audio-commands".into())
            .spawn(move || {
                for command in rx {
                    if let Err(e) = backend.apply(&command) {
                        tracing::warn!("audio command {command:?} failed: {e}");
                    }
                }
            })
            .expect("spawning the audio command thread");
        Self(tx)
    }

    pub fn send(&self, command: Command) {
        if self.0.send(command).is_err() {
            tracing::error!("audio command thread is gone");
        }
    }
}
