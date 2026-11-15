use std::sync::Arc;
use std::time::Duration;

use pcvc_audio::AudioEvent;
use pcvc_protocol::encode_full_state;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::{Instant, sleep_until};

use crate::{ServerEvent, Shared, to_full_state};

/// Rebuilds and publishes the state at most once per `coalesce` window after
/// any backend event. The window starts at the first event, so a continuous
/// stream of events (a slider being dragged) still produces regular updates.
pub(crate) async fn run(
    shared: Arc<Shared>,
    mut events: broadcast::Receiver<AudioEvent>,
    coalesce: Duration,
) {
    let mut deadline: Option<Instant> = None;
    let mut events_open = true;
    loop {
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            event = events.recv(), if events_open => match event {
                Ok(_) | Err(RecvError::Lagged(_)) => {
                    deadline.get_or_insert_with(|| Instant::now() + coalesce);
                }
                Err(RecvError::Closed) => {
                    tracing::warn!("audio backend closed its event stream");
                    events_open = false;
                }
            },
            // The future is built even when the branch is disabled, hence the
            // placeholder instant.
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                rebuild(&shared).await;
            }
        }
    }
}

/// Snapshots the backend and publishes the state and its encoded line.
pub(crate) async fn rebuild(shared: &Shared) {
    let backend = Arc::clone(&shared.backend);
    let snapshot = match tokio::task::spawn_blocking(move || backend.snapshot()).await {
        Ok(Ok(state)) => state,
        Ok(Err(e)) => {
            tracing::warn!("audio snapshot failed: {e}");
            shared.emit(ServerEvent::BackendError(format!("snapshot failed: {e}")));
            return;
        }
        Err(e) => {
            tracing::error!("audio snapshot panicked: {e}");
            shared.emit(ServerEvent::BackendError(format!("snapshot panicked: {e}")));
            return;
        }
    };
    let frame = to_full_state(&snapshot).map(|full| Arc::from(encode_full_state(&full)));
    if frame.is_none() {
        tracing::debug!("no default playback device, nothing to broadcast");
    }
    shared.state.send_replace(snapshot);
    shared.frames.send_replace(frame);
}
