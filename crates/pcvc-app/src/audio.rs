//! Audio backend selection, the backend watcher and the command worker.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use pcvc_audio::{AudioBackend, MockBackend};
use pcvc_protocol::Command;
use tokio::sync::broadcast::error::RecvError;

use crate::inbox::{Inbound, Inbox};

/// Which audio backend to run the app against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// The platform's audio system.
    System,
    /// The scripted [`MockBackend`].
    Mock,
}

/// Creates the backend for `choice`. Platform backends plug in here.
pub fn create_backend(choice: BackendChoice) -> Arc<dyn AudioBackend> {
    match choice {
        BackendChoice::Mock => {
            tracing::info!("using the mock audio backend");
            Arc::new(MockBackend::demo())
        }
        BackendChoice::System => {
            tracing::warn!("no system audio backend for this platform yet; using the mock backend");
            Arc::new(MockBackend::demo())
        }
    }
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
            while events.try_recv().is_ok() {}
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
