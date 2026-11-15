use std::sync::Arc;
use std::time::Duration;

use pcvc_audio::AudioEvent;
use pcvc_protocol::{FullState, encode_full_state};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::{Instant, sleep_until};

use crate::state::to_full_state_with;
use crate::{Frame, ServerEvent, Shared};

/// Delay before retrying a failed snapshot, and the polling period once the
/// backend's event stream has ended.
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Least time between a broadcast and the next one whose only change is in
/// session details, so a title that keeps changing (a ticker, a scrolling
/// title) cannot flood clients. Other changes are never held back, and they
/// carry the latest details.
pub(crate) const DETAIL_INTERVAL: Duration = Duration::from_secs(1);

/// The outcome of [`rebuild`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebuilt {
    /// The snapshot failed.
    Failed,
    /// Published, or nothing to publish.
    Done,
    /// Only details changed, too soon after the last broadcast: rebuild again
    /// at this instant.
    Held(Instant),
}

/// Rebuilds and publishes the state at most once per `coalesce` window after
/// any backend or icon provider event. The window starts at the first event,
/// so a continuous stream of events (a slider being dragged) still produces
/// regular updates. `last_sent` is when a state was last broadcast.
pub(crate) async fn run(
    shared: Arc<Shared>,
    mut events: broadcast::Receiver<AudioEvent>,
    mut icon_events: Option<broadcast::Receiver<()>>,
    coalesce: Duration,
    initial_ok: bool,
    mut last_sent: Option<Instant>,
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
            event = recv_icon_event(&mut icon_events) => match event {
                Ok(()) | Err(RecvError::Lagged(_)) => schedule(&mut deadline, coalesce),
                Err(RecvError::Closed) => {
                    tracing::warn!("icon provider closed its event stream");
                    icon_events = None;
                }
            },
            () = shared.resync.notified() => {
                force = true;
                schedule(&mut deadline, coalesce);
            }
            // The future is built even when the branch is disabled, hence the
            // placeholder instant.
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = match rebuild(&shared, force, &mut last_sent).await {
                    Rebuilt::Failed => Some(Instant::now() + RETRY_INTERVAL),
                    Rebuilt::Done => {
                        force = false;
                        (!events_open).then(|| Instant::now() + RETRY_INTERVAL)
                    }
                    Rebuilt::Held(at) => Some(at),
                };
            }
        }
    }
}

async fn recv_icon_event(events: &mut Option<broadcast::Receiver<()>>) -> Result<(), RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// Moves the deadline to `delay` from now unless one is already sooner.
fn schedule(deadline: &mut Option<Instant>, delay: Duration) {
    let at = Instant::now() + delay;
    *deadline = Some(deadline.map_or(at, |d| d.min(at)));
}

/// Snapshots the backend and publishes the state and its encoded line. An
/// unchanged state is only re-sent to clients when `force` is set, and a
/// state whose only change is in details is held back until
/// [`DETAIL_INTERVAL`] after `last_sent` unless `force` is set.
pub(crate) async fn rebuild(
    shared: &Shared,
    force: bool,
    last_sent: &mut Option<Instant>,
) -> Rebuilt {
    let backend = Arc::clone(&shared.backend);
    let icons = shared.icons.clone();
    let details = shared.details;
    let built = tokio::task::spawn_blocking(move || {
        let snapshot = backend.snapshot()?;
        let state = to_full_state_with(&snapshot, icons.as_deref(), details);
        Ok::<_, pcvc_audio::AudioError>((snapshot, state))
    })
    .await;
    let (snapshot, state) = match built {
        Ok(Ok(built)) => built,
        Ok(Err(e)) => {
            tracing::warn!("audio snapshot failed: {e}");
            shared.emit(ServerEvent::BackendError(format!("snapshot failed: {e}")));
            return Rebuilt::Failed;
        }
        Err(e) => {
            tracing::error!("audio snapshot panicked: {e}");
            shared.emit(ServerEvent::BackendError(format!("snapshot panicked: {e}")));
            return Rebuilt::Failed;
        }
    };
    let frame = state.map(|state| {
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
    if let Some(until) = last_sent.map(|sent| sent + DETAIL_INTERVAL)
        && !force
        && Instant::now() < until
        && let (Some(old), Some(new)) = (shared.frames.borrow().as_deref(), frame.as_deref())
        && only_details_differ(&old.state, &new.state)
    {
        return Rebuilt::Held(until);
    }
    let sent = shared.frames.send_if_modified(|current| {
        let changed = force || current.as_ref().map(|f| &f.line) != frame.as_ref().map(|f| &f.line);
        *current = frame;
        changed
    });
    if sent {
        *last_sent = Some(Instant::now());
    }
    Rebuilt::Done
}

fn only_details_differ(old: &FullState, new: &FullState) -> bool {
    let without_details = |state: &FullState| {
        let mut state = state.clone();
        for session in &mut state.default_device.sessions {
            session.detail = None;
        }
        state
    };
    old != new && without_details(old) == without_details(new)
}
