//! Audio backend selection, the backend watcher and the command worker.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use pcvc_audio::{AudioBackend, AudioEvent, MockBackend};
use pcvc_protocol::Command;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::time::Instant;

use crate::inbox::{Inbound, Inbox};

/// Which audio backend to run the app against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// The platform's audio system.
    System,
    /// The scripted [`MockBackend`].
    Mock,
}

/// Creates the backend for `choice`, with session details if `details` is
/// set and the backend has them. When the system backend cannot start, the
/// app runs with no devices so it never presents the mock's scripted
/// sessions as real ones.
pub fn create_backend(choice: BackendChoice, details: bool) -> Arc<dyn AudioBackend> {
    match choice {
        BackendChoice::Mock => {
            tracing::info!("using the mock audio backend");
            Arc::new(MockBackend::demo())
        }
        BackendChoice::System => match system_backend(details) {
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
fn system_backend(details: bool) -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    let mut attempt = 1;
    loop {
        match open_system_backend(details) {
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
fn open_system_backend(details: bool) -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    tracing::info!("using the PulseAudio backend");
    Ok(Arc::new(pcvc_audio::PulseBackend::with_details(details)?))
}

/// Core Audio has no session details.
#[cfg(target_os = "windows")]
fn open_system_backend(_details: bool) -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    tracing::info!("using the Core Audio backend");
    Ok(Arc::new(pcvc_audio::WindowsBackend::new()?))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn system_backend(_details: bool) -> Result<Arc<dyn AudioBackend>, pcvc_audio::AudioError> {
    tracing::warn!("no system audio backend for this platform; using the mock backend");
    Ok(Arc::new(MockBackend::demo()))
}

/// How long backend events are gathered before the state is re-read.
const COALESCE: Duration = Duration::from_millis(15);

/// The shortest time between a snapshot and one taken because only a
/// session's detail changed, so a title that keeps changing costs at most
/// one snapshot (and redraw) a second.
const DETAIL_INTERVAL: Duration = Duration::from_secs(1);

/// Forwards a fresh snapshot to the inbox on start and after every backend
/// change, independently of whether the server runs.
pub fn spawn_watcher(backend: Arc<dyn AudioBackend>, inbox: Inbox) {
    spawn_watcher_with(backend, inbox, DETAIL_INTERVAL);
}

fn spawn_watcher_with(backend: Arc<dyn AudioBackend>, inbox: Inbox, detail_interval: Duration) {
    let mut events = backend.subscribe();
    tokio::spawn(async move {
        loop {
            let taken_at = Instant::now();
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
            // A change in a detail alone is read `detail_interval` after
            // the last snapshot; any other change at once, with the latest
            // details.
            let mut detail_due = None;
            loop {
                let event = match detail_due {
                    None => events.recv().await,
                    Some(due) => match tokio::time::timeout_at(due, events.recv()).await {
                        Ok(event) => event,
                        Err(_) => break,
                    },
                };
                match event {
                    Ok(AudioEvent::SessionDetailChanged { .. }) => {
                        detail_due.get_or_insert(taken_at + detail_interval);
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => break,
                    Err(RecvError::Closed) => return,
                }
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

#[cfg(test)]
mod tests {
    use iced::futures::StreamExt;
    use iced::futures::channel::mpsc::UnboundedReceiver;

    use super::*;
    use crate::inbox::{self, Inbound};

    const INTERVAL: Duration = Duration::from_millis(300);

    /// The next snapshot the watcher sends within `wait`.
    async fn next(
        rx: &mut UnboundedReceiver<Inbound>,
        wait: Duration,
    ) -> Option<pcvc_audio::AudioState> {
        match tokio::time::timeout(wait, rx.next()).await {
            Ok(Some(Inbound::Audio(state))) => Some(state),
            Ok(other) => panic!("unexpected {other:?}"),
            Err(_) => None,
        }
    }

    fn detail(state: &pcvc_audio::AudioState) -> Option<&str> {
        state.default_device.as_ref().unwrap().sessions[0]
            .detail
            .as_deref()
    }

    #[tokio::test]
    async fn detail_changes_are_read_at_most_once_an_interval() {
        let mock = Arc::new(MockBackend::demo());
        let backend: Arc<dyn AudioBackend> = mock.clone();
        let (inbox, receiver) = inbox::channel();
        let mut rx = receiver.take_stream().unwrap();
        spawn_watcher_with(backend, inbox, INTERVAL);
        let first = next(&mut rx, Duration::from_secs(2)).await.unwrap();
        let session = first.default_device.unwrap().sessions[0].clone();

        // A burst of titles is one snapshot, an interval after the last
        // one, with the latest title.
        let start = Instant::now();
        for title in ["One", "Two", "Three"] {
            mock.set_session_detail(&session.id, Some(title)).unwrap();
        }
        let state = next(&mut rx, 2 * INTERVAL).await.unwrap();
        assert!(start.elapsed() >= INTERVAL / 2, "{:?}", start.elapsed());
        assert_eq!(detail(&state), Some("Three"));
        assert!(next(&mut rx, INTERVAL / 2).await.is_none());

        // Any other change is read at once and brings the latest detail.
        mock.set_session_detail(&session.id, Some("Four")).unwrap();
        let start = Instant::now();
        mock.set_session(&session.id, 12.0, false).unwrap();
        let state = next(&mut rx, INTERVAL).await.unwrap();
        assert!(start.elapsed() < INTERVAL / 2, "{:?}", start.elapsed());
        assert_eq!(detail(&state), Some("Four"));
        let device = state.default_device.unwrap();
        assert_eq!(device.sessions[0].volume, 12.0);
        assert!(next(&mut rx, 2 * INTERVAL).await.is_none());
    }
}
