use std::sync::Arc;
use std::time::Duration;

use pcvc_audio::AudioEvent;
use pcvc_protocol::encode_full_state;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::{Instant, sleep_until};

use crate::{Frame, ServerEvent, Shared, to_full_state};

/// Delay before retrying a failed snapshot, and the polling period once the
/// backend's event stream has ended.
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Rebuilds and publishes the state at most once per `coalesce` window after
/// any backend event. The window starts at the first event, so a continuous
/// stream of events (a slider being dragged) still produces regular updates.
pub(crate) async fn run(
    shared: Arc<Shared>,
    mut events: broadcast::Receiver<AudioEvent>,
    coalesce: Duration,
    initial_ok: bool,
) {
    let mut deadline = (!initial_ok).then(|| Instant::now() + RETRY_INTERVAL);
    let mut events_open = true;
    let mut force = !initial_ok;
    loop {
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            event = events.recv(), if events_open => match event {
                Ok(_) | Err(RecvError::Lagged(_)) => schedule(&mut deadline, coalesce),
                Err(RecvError::Closed) => {
                    tracing::warn!("audio backend closed its event stream, polling instead");
                    shared.emit(ServerEvent::BackendError(
                        "audio event stream ended, polling for changes".into(),
                    ));
                    events_open = false;
                    schedule(&mut deadline, coalesce);
                }
            },
            () = shared.resync.notified() => {
                force = true;
                schedule(&mut deadline, coalesce);
            }
            // The future is built even when the branch is disabled, hence the
            // placeholder instant.
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                let ok = rebuild(&shared, force).await;
                force &= !ok;
                deadline = (!ok || !events_open).then(|| Instant::now() + RETRY_INTERVAL);
            }
        }
    }
}

/// Moves the deadline to `delay` from now unless one is already sooner.
fn schedule(deadline: &mut Option<Instant>, delay: Duration) {
    let at = Instant::now() + delay;
    *deadline = Some(deadline.map_or(at, |d| d.min(at)));
}

/// Snapshots the backend and publishes the state and its encoded line. An
/// unchanged state is only re-sent to clients when `force` is set. Returns
/// false if the snapshot failed.
pub(crate) async fn rebuild(shared: &Shared, force: bool) -> bool {
    let backend = Arc::clone(&shared.backend);
    let snapshot = match tokio::task::spawn_blocking(move || backend.snapshot()).await {
        Ok(Ok(state)) => state,
        Ok(Err(e)) => {
            tracing::warn!("audio snapshot failed: {e}");
            shared.emit(ServerEvent::BackendError(format!("snapshot failed: {e}")));
            return false;
        }
        Err(e) => {
            tracing::error!("audio snapshot panicked: {e}");
            shared.emit(ServerEvent::BackendError(format!("snapshot panicked: {e}")));
            return false;
        }
    };
    let frame = to_full_state(&snapshot).map(|state| {
        let line = encode_full_state(&state);
        Arc::new(Frame { state, line })
    });
    if frame.is_none() {
        tracing::debug!("no default playback device, nothing to broadcast");
    }
    shared.state.send_if_modified(|current| {
        let changed = *current != snapshot;
        *current = snapshot;
        changed
    });
    shared.frames.send_if_modified(|current| {
        let changed = force || current.as_ref().map(|f| &f.line) != frame.as_ref().map(|f| &f.line);
        *current = frame;
        changed
    });
    true
}
