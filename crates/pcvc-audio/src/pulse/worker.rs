//! The thread that owns the PulseAudio main loop and connection.

use std::cell::Cell;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use libpulse_binding::mainloop::api::Mainloop as MainloopTrait;
use libpulse_binding::mainloop::events::io::{FlagSet as IoEventFlagSet, IoEvent};
use libpulse_binding::mainloop::standard::Mainloop;
use pcvc_protocol::Command;

use super::conn::{self, Connection, OpError};
use super::model::{RawState, diff, percent_to_volume, volume_matches, volume_to_percent};
use super::{Request, Shared, backend_error};
use crate::AudioError;

/// Limit for connecting and for each state read outside a command.
const OP_TIMEOUT: Duration = Duration::from_secs(4);

/// Delay before retrying a failed state read on a live connection.
const REFRESH_RETRY: Duration = Duration::from_secs(1);

/// How often the first state read is attempted before startup gives up.
const STARTUP_READ_ATTEMPTS: u32 = 3;

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
    let woken = Rc::new(Cell::new(false));
    let Some(_wake_event) = watch_wake(&mut mainloop, wake, Rc::clone(&woken)) else {
        let _ = ready.send(Err(backend_error("cannot watch the wake-up socket")));
        return;
    };
    let mut worker = Worker {
        conn: None,
        raw: RawState::default(),
        shared,
        retry_refresh_at: None,
        reconnect: Backoff::new(),
        woken,
        mainloop,
    };
    if let Err(e) = worker.start() {
        let _ = ready.send(Err(backend_error(format!(
            "cannot connect to PulseAudio: {e}"
        ))));
        return;
    }
    let _ = ready.send(Ok(()));
    worker.serve(&requests);
}

/// Registers the read end of the wake-up socket with the main loop. The
/// callback drains it and sets `woken`; requests are picked up by
/// [`Worker::serve`].
fn watch_wake(
    mainloop: &mut Mainloop,
    wake: UnixStream,
    woken: Rc<Cell<bool>>,
) -> Option<IoEvent<<Mainloop as MainloopTrait>::MI>> {
    let fd = wake.as_raw_fd();
    mainloop.new_io_event(
        fd,
        IoEventFlagSet::INPUT,
        Box::new(move |_, _, _| {
            let mut buf = [0u8; 64];
            while matches!((&wake).read(&mut buf), Ok(n) if n > 0) {}
            woken.set(true);
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
    /// The last state read, patched with the effect of commands applied since.
    raw: RawState,
    shared: Arc<Shared>,
    /// Set while a failed state read on a live connection awaits a retry.
    retry_refresh_at: Option<Instant>,
    reconnect: Backoff,
    /// Set when the wake-up socket was drained. The main loop also runs
    /// inside state reads and commands, so a wake-up can be consumed there
    /// while its request is still queued.
    woken: Rc<Cell<bool>>,
    mainloop: Mainloop,
}

impl Worker {
    /// Connects and reads the initial state. The connection itself is not
    /// retried, but a failed first read on a working connection is, since
    /// the server may still be starting up.
    fn start(&mut self) -> Result<(), OpError> {
        let mut attempts = 1;
        loop {
            match self.connect() {
                Ok(()) => return Ok(()),
                Err(OpError::Failed(_))
                    if self.conn.is_some() && attempts < STARTUP_READ_ATTEMPTS =>
                {
                    attempts += 1;
                    let until = Instant::now() + REFRESH_RETRY;
                    while Instant::now() < until {
                        conn::iterate(&mut self.mainloop, Some(until - Instant::now()));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn serve(&mut self, requests: &mpsc::Receiver<Request>) {
        loop {
            self.woken.set(false);
            loop {
                match requests.try_recv() {
                    Ok(Request::Command {
                        command,
                        deadline,
                        reply,
                    }) => {
                        let _ = reply.send(self.handle(&command, deadline));
                    }
                    Ok(Request::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            let wake_at = self.maintain(Instant::now());
            if self.woken.get() {
                continue;
            }
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
            } else if (conn.is_dirty() || retry_due)
                && let Err(e) = self.refresh(now + OP_TIMEOUT)
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

    /// Opens a connection and reads the state. On a read failure the new
    /// connection is kept in `self.conn`.
    fn connect(&mut self) -> Result<(), OpError> {
        self.conn = None;
        self.conn = Some(Connection::open(&mut self.mainloop, OP_TIMEOUT)?);
        self.refresh(Instant::now() + OP_TIMEOUT)?;
        self.reconnect.reset();
        Ok(())
    }

    /// Re-reads the server state, publishes it and emits the events that
    /// describe the change. Changes reported while the read is in flight
    /// leave the connection dirty, so they are read again.
    fn refresh(&mut self, deadline: Instant) -> Result<(), OpError> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.take_dirty();
        let raw = conn.query(&mut self.mainloop, deadline, &self.raw.clients)?;
        self.retry_refresh_at = None;
        let state = raw.to_audio_state();
        self.raw = raw;
        // Events are sent before the lock is released, so whoever sees the
        // new state in a snapshot can also already receive its events.
        let mut current = self.shared.lock();
        for event in diff(&current, &state) {
            // An error only means nobody is subscribed.
            let _ = self.shared.events.send(event);
        }
        *current = state;
        Ok(())
    }

    /// Applies a command unless its caller has already given up.
    ///
    /// The cache is re-read first if the server reported changes, so the
    /// command is judged against the current state. The cache is not re-read
    /// afterwards: the commands patch it with what they set, and the change
    /// events the server sends in response trigger the authoritative read
    /// (and the [`AudioEvent`](crate::AudioEvent)s).
    fn handle(&mut self, command: &Command, deadline: Instant) -> Result<(), AudioError> {
        if Instant::now() >= deadline {
            return Err(backend_error("PulseAudio request expired before it ran"));
        }
        let not_connected = || backend_error("not connected to PulseAudio, reconnecting");
        let conn = self.conn.as_ref().filter(|c| !c.is_dead());
        let stale = conn.ok_or_else(not_connected)?.is_dirty() || self.retry_refresh_at.is_some();
        if stale && let Err(e) = self.refresh(deadline) {
            self.refresh_failed(&e, Instant::now());
            return Err(backend_error(e));
        }
        let mut ctx = Ctx {
            conn: self.conn.as_ref().ok_or_else(not_connected)?,
            mainloop: &mut self.mainloop,
            raw: &mut self.raw,
            deadline,
        };
        match command {
            Command::SetDefaultDevice { device_id } => ctx.set_default_device(device_id),
            Command::SetMaster { volume, muted } => ctx.set_master(*volume, *muted),
            Command::SetSession {
                session_id,
                volume,
                muted,
            } => ctx.set_session(session_id, *volume, *muted),
        }
    }
}

/// What a command needs: the connection, the loop to wait on, and the state
/// the command is judged against and records its effect in.
struct Ctx<'a> {
    conn: &'a Connection,
    mainloop: &'a mut Mainloop,
    raw: &'a mut RawState,
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
        let (target, name) = (sink.index, sink.name.clone());
        let previous = self.raw.default_sink().map(|s| s.index);
        if previous == Some(target) {
            return Ok(());
        }
        self.conn
            .set_default_sink(self.mainloop, self.deadline, &name)
            .map_err(backend_error)?;
        self.raw.default_sink = Some(name);
        for input in &mut self.raw.inputs {
            if Some(input.sink) != previous || input.client.is_none() {
                continue;
            }
            // Streams can refuse to move (or vanish); that does not undo the
            // default device change.
            match self
                .conn
                .move_sink_input(self.mainloop, self.deadline, input.index, target)
            {
                Ok(()) => input.sink = target,
                Err(e) => tracing::debug!(
                    "moving sink-input {} to the new default failed: {e}",
                    input.index
                ),
            }
        }
        Ok(())
    }

    /// Sets volume and/or mute on the default sink. A volume that matches the
    /// current one (see [`volume_matches`]) is left alone, so an echoed value
    /// does not flatten the channel balance or cut a boost above 100 %.
    fn set_master(&mut self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError> {
        let sink = self.raw.default_sink().ok_or(AudioError::NoDefaultDevice)?;
        let index = sink.index;
        let new_volume = match volume {
            Some(v) if !volume_matches(v, volume_to_percent(&sink.volume)) => {
                let target = percent_to_volume(v);
                let cv = conn::uniform_volume(sink.volume.len(), target)
                    .ok_or_else(|| backend_error("the default sink has no volume channels"))?;
                Some((target, cv))
            }
            _ => None,
        };
        let new_mute = muted.filter(|&m| m != sink.muted);
        if let Some((target, cv)) = new_volume {
            self.conn
                .set_sink_volume(self.mainloop, self.deadline, index, &cv)
                .map_err(backend_error)?;
            if let Some(sink) = self.raw.sink_mut(index) {
                sink.volume.fill(target);
            }
        }
        if let Some(muted) = new_mute {
            self.conn
                .set_sink_mute(self.mainloop, self.deadline, index, muted)
                .map_err(backend_error)?;
            if let Some(sink) = self.raw.sink_mut(index) {
                sink.muted = muted;
            }
        }
        Ok(())
    }

    /// Sets volume and mute of a session on the default sink. A matching
    /// volume is left alone as in [`Self::set_master`]; a volume change on a
    /// stream whose volume cannot be set is refused without changing
    /// anything.
    fn set_session(
        &mut self,
        session_id: &str,
        volume: f32,
        muted: bool,
    ) -> Result<(), AudioError> {
        let unknown = || AudioError::UnknownSession(session_id.to_owned());
        let (_, input) = self.raw.default_session(session_id).ok_or_else(unknown)?;
        let index = input.index;
        let new_volume = if volume_matches(volume, input.percent()) {
            None
        } else {
            let target = percent_to_volume(volume);
            let cv = conn::uniform_volume(input.volume.len(), target)
                .filter(|_| input.has_volume && input.volume_writable)
                .ok_or_else(|| backend_error("the volume of this stream cannot be changed"))?;
            Some((target, cv))
        };
        let new_mute = (muted != input.muted).then_some(muted);
        // The stream can go away between the lookup and the request.
        let map = |e: OpError| {
            if e.is_no_entity() {
                unknown()
            } else {
                backend_error(e)
            }
        };
        if let Some((target, cv)) = new_volume {
            self.conn
                .set_sink_input_volume(self.mainloop, self.deadline, index, &cv)
                .map_err(map)?;
            if let Some(input) = self.raw.input_mut(index) {
                input.volume.fill(target);
            }
        }
        if let Some(muted) = new_mute {
            self.conn
                .set_sink_input_mute(self.mainloop, self.deadline, index, muted)
                .map_err(map)?;
            if let Some(input) = self.raw.input_mut(index) {
                input.muted = muted;
            }
        }
        Ok(())
    }
}
