//! The thread that owns the PulseAudio main loop and connection.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use libpulse_binding::mainloop::api::Mainloop as MainloopTrait;
use libpulse_binding::mainloop::events::io::{FlagSet as IoEventFlagSet, IoEvent};
use libpulse_binding::mainloop::standard::Mainloop;
use pcvc_protocol::Command;

use super::conn::{self, Connection, OpError};
use super::model::{RawState, diff, percent_to_volume};
use super::{Request, Shared, backend_error};
use crate::AudioError;

/// Limit for connecting, and for each query or command.
const OP_TIMEOUT: Duration = Duration::from_secs(4);

/// Delay before retrying a failed state read on a live connection.
const REFRESH_RETRY: Duration = Duration::from_secs(1);

/// Reconnect backoff bounds.
const RECONNECT_MIN: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(10);

pub(super) fn run(
    shared: Arc<Shared>,
    requests: mpsc::Receiver<Request>,
    wake: UnixStream,
    ready: mpsc::Sender<Result<(), AudioError>>,
) {
    let mut mainloop = match Mainloop::new() {
        Some(m) => m,
        None => {
            let _ = ready.send(Err(backend_error("cannot create a PulseAudio main loop")));
            return;
        }
    };
    let Some(_wake_event) = watch_wake(&mut mainloop, wake) else {
        let _ = ready.send(Err(backend_error("cannot watch the wake-up socket")));
        return;
    };
    let mut worker = Worker {
        conn: None,
        raw: RawState::default(),
        shared,
        retry_refresh_at: None,
        reconnect: Backoff::new(),
        mainloop,
    };
    if let Err(e) = worker.connect() {
        let _ = ready.send(Err(backend_error(format!(
            "cannot connect to PulseAudio: {e}"
        ))));
        return;
    }
    let _ = ready.send(Ok(()));
    worker.serve(&requests);
}

/// Registers the read end of the wake-up socket with the main loop. The
/// callback only drains it: requests are picked up after each iteration.
fn watch_wake(
    mainloop: &mut Mainloop,
    wake: UnixStream,
) -> Option<IoEvent<<Mainloop as MainloopTrait>::MI>> {
    let fd = wake.as_raw_fd();
    mainloop.new_io_event(
        fd,
        IoEventFlagSet::INPUT,
        Box::new(move |_, _, _| {
            let mut buf = [0u8; 64];
            while matches!((&wake).read(&mut buf), Ok(n) if n > 0) {}
        }),
    )
}

/// Exponential reconnect delay.
#[derive(Debug)]
struct Backoff {
    delay: Duration,
    next_attempt: Option<Instant>,
}

impl Backoff {
    fn new() -> Self {
        Self {
            delay: RECONNECT_MIN,
            next_attempt: None,
        }
    }

    /// Schedules the next attempt and doubles the delay after it.
    fn schedule(&mut self, now: Instant) {
        self.next_attempt = Some(now + self.delay);
        self.delay = (self.delay * 2).min(RECONNECT_MAX);
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

struct Worker {
    /// Declared before `mainloop` so the context is freed first.
    conn: Option<Connection>,
    /// The last successful query.
    raw: RawState,
    shared: Arc<Shared>,
    /// Set while a failed state read on a live connection awaits a retry.
    retry_refresh_at: Option<Instant>,
    reconnect: Backoff,
    mainloop: Mainloop,
}

impl Worker {
    fn serve(&mut self, requests: &mpsc::Receiver<Request>) {
        loop {
            loop {
                match requests.try_recv() {
                    Ok(Request::Command(command, reply)) => {
                        let _ = reply.send(self.handle(&command));
                    }
                    Ok(Request::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            let wake_at = self.maintain(Instant::now());
            let timeout = wake_at.map(|at| at.saturating_duration_since(Instant::now()));
            conn::iterate(&mut self.mainloop, timeout);
        }
    }

    /// Refreshes after reported changes, notices a lost connection and
    /// reconnects when the backoff allows. Returns when the loop should next
    /// wake up on its own, if ever.
    fn maintain(&mut self, now: Instant) -> Option<Instant> {
        if let Some(conn) = &self.conn {
            let retry_due = self.retry_refresh_at.is_some_and(|at| now >= at);
            if conn.is_dead() {
                tracing::warn!("lost the PulseAudio connection, reconnecting");
                self.disconnect(now);
            } else if (conn.take_dirty() || retry_due)
                && let Err(e) = self.refresh()
            {
                self.refresh_failed(&e, now);
            }
        }
        if self.conn.is_none() && self.reconnect.next_attempt.is_none_or(|at| now >= at) {
            match self.connect() {
                Ok(()) => tracing::info!("reconnected to PulseAudio"),
                Err(e) => {
                    tracing::debug!("reconnecting to PulseAudio failed: {e}");
                    self.disconnect(Instant::now());
                }
            }
        }
        match &self.conn {
            Some(_) => self.retry_refresh_at,
            None => self.reconnect.next_attempt,
        }
    }

    /// Drops the connection, keeping the cached state, and schedules a
    /// reconnect.
    fn disconnect(&mut self, now: Instant) {
        self.conn = None;
        self.retry_refresh_at = None;
        self.reconnect.schedule(now);
    }

    /// A read that timed out or was cut off means the connection is not
    /// usable; other failures are retried on the same connection.
    fn refresh_failed(&mut self, error: &OpError, now: Instant) {
        match error {
            OpError::Timeout | OpError::Cancelled => {
                tracing::warn!("reading PulseAudio state failed ({error}), reconnecting");
                self.disconnect(now);
            }
            OpError::Failed(_) => {
                tracing::warn!("reading PulseAudio state failed: {error}");
                self.retry_refresh_at = Some(now + REFRESH_RETRY);
            }
        }
    }

    fn connect(&mut self) -> Result<(), OpError> {
        self.conn = None;
        let conn = Connection::open(&mut self.mainloop, OP_TIMEOUT)?;
        conn.take_dirty();
        self.conn = Some(conn);
        self.refresh()?;
        self.reconnect.reset();
        Ok(())
    }

    /// Re-reads the server state, publishes it and emits the events that
    /// describe the change.
    fn refresh(&mut self) -> Result<(), OpError> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let raw = conn.query(&mut self.mainloop, Instant::now() + OP_TIMEOUT)?;
        self.retry_refresh_at = None;
        let state = raw.to_audio_state();
        self.raw = raw;
        let events = {
            let mut current = self.shared.lock();
            let events = diff(&current, &state);
            *current = state;
            events
        };
        for event in events {
            // An error only means nobody is subscribed.
            let _ = self.shared.events.send(event);
        }
        Ok(())
    }

    /// Applies a command, then refreshes the cache so a snapshot taken after
    /// the call returns reflects it.
    fn handle(&mut self, command: &Command) -> Result<(), AudioError> {
        let not_connected = || backend_error("not connected to PulseAudio, reconnecting");
        let conn = self.conn.as_ref().filter(|c| !c.is_dead());
        let stale = conn.ok_or_else(not_connected)?.take_dirty() || self.retry_refresh_at.is_some();
        if stale && let Err(e) = self.refresh() {
            self.refresh_failed(&e, Instant::now());
            return Err(backend_error(e));
        }
        let mut ctx = Ctx {
            conn: self.conn.as_ref().ok_or_else(not_connected)?,
            mainloop: &mut self.mainloop,
            raw: &self.raw,
            deadline: Instant::now() + OP_TIMEOUT,
        };
        let result = match command {
            Command::SetDefaultDevice { device_id } => ctx.set_default_device(device_id),
            Command::SetMaster { volume, muted } => ctx.set_master(*volume, *muted),
            Command::SetSession {
                session_id,
                volume,
                muted,
            } => ctx.set_session(session_id, *volume, *muted),
        };
        if let Err(e) = self.refresh() {
            self.refresh_failed(&e, Instant::now());
        }
        result
    }
}

/// What a command needs: the connection, the loop to wait on, and the state
/// the command is judged against.
struct Ctx<'a> {
    conn: &'a Connection,
    mainloop: &'a mut Mainloop,
    raw: &'a RawState,
    deadline: Instant,
}

impl Ctx<'_> {
    /// Makes the sink the default, then moves the client streams that were
    /// on the previous default sink to it, so streams follow the default as
    /// they do on Windows. Moving a stream to the default sink also clears any
    /// stored routing for it (PulseAudio 14+ and pipewire-pulse treat that as
    /// "follow the default"), so it keeps following later changes. Streams on
    /// other sinks were routed there deliberately and stay.
    fn set_default_device(&mut self, device_id: &str) -> Result<(), AudioError> {
        let sink = self
            .raw
            .sink_by_id(device_id)
            .ok_or_else(|| AudioError::UnknownDevice(device_id.to_owned()))?;
        let previous = self.raw.default_sink().map(|s| s.index);
        if previous == Some(sink.index) {
            return Ok(());
        }
        self.conn
            .set_default_sink(self.mainloop, self.deadline, &sink.name)
            .map_err(backend_error)?;
        let followers = self
            .raw
            .inputs
            .iter()
            .filter(|i| Some(i.sink) == previous && i.client.is_some());
        for input in followers {
            // Streams can refuse to move (or vanish); that does not undo the
            // default device change.
            if let Err(e) =
                self.conn
                    .move_sink_input(self.mainloop, self.deadline, input.index, sink.index)
            {
                tracing::debug!(
                    "moving sink-input {} to the new default failed: {e}",
                    input.index
                );
            }
        }
        Ok(())
    }

    /// Sets volume (all channels to the same level) and/or mute on the
    /// default sink, skipping whatever already has the requested value.
    fn set_master(&mut self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError> {
        let sink = self.raw.default_sink().ok_or(AudioError::NoDefaultDevice)?;
        if let Some(volume) = volume {
            let target = percent_to_volume(volume);
            if sink.volume.iter().any(|&v| v != target) {
                let cv = conn::uniform_cvolume(sink.volume.len(), target)
                    .ok_or_else(|| backend_error("the default sink has no volume channels"))?;
                self.conn
                    .set_sink_volume(self.mainloop, self.deadline, sink.index, &cv)
                    .map_err(backend_error)?;
            }
        }
        if let Some(muted) = muted
            && muted != sink.muted
        {
            self.conn
                .set_sink_mute(self.mainloop, self.deadline, sink.index, muted)
                .map_err(backend_error)?;
        }
        Ok(())
    }

    /// Sets volume and mute of a session on the default sink. A stream
    /// without volume control only has its mute set.
    fn set_session(
        &mut self,
        session_id: &str,
        volume: f32,
        muted: bool,
    ) -> Result<(), AudioError> {
        let unknown = || AudioError::UnknownSession(session_id.to_owned());
        let (_, input) = self.raw.default_session(session_id).ok_or_else(unknown)?;
        // The stream can go away between the lookup and the request.
        let map = |e: conn::OpError| {
            if e.is_no_entity() {
                unknown()
            } else {
                backend_error(e)
            }
        };
        let target = percent_to_volume(volume);
        if input.has_volume && input.volume_writable && input.volume.iter().any(|&v| v != target) {
            let cv = conn::uniform_cvolume(input.volume.len(), target)
                .ok_or_else(|| backend_error("the stream has no volume channels"))?;
            self.conn
                .set_sink_input_volume(self.mainloop, self.deadline, input.index, &cv)
                .map_err(map)?;
        }
        if muted != input.muted {
            self.conn
                .set_sink_input_mute(self.mainloop, self.deadline, input.index, muted)
                .map_err(map)?;
        }
        Ok(())
    }
}
