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
use super::model::{
    RawState, StreamChange, diff, percent_to_volume, volume_matches, volume_to_percent,
};
use super::{Request, Shared, backend_error};
use crate::AudioError;

/// Limit for connecting and for each state read outside a command.
const OP_TIMEOUT: Duration = Duration::from_secs(4);

/// Delay before retrying a failed state read on a live connection.
const REFRESH_RETRY: Duration = Duration::from_secs(1);

/// How long reported changes gather before the state is read. A browser tab
/// that changes its title retitles each of its streams separately, a few
/// milliseconds apart; read in between, the tab's session would split in
/// two and merge again.
const SETTLE: Duration = Duration::from_millis(30);

/// How often the first state read is attempted before startup gives up.
const STARTUP_READ_ATTEMPTS: u32 = 3;

/// Reconnect backoff bounds.
const RECONNECT_MIN: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(10);

pub(super) fn run(
    shared: Arc<Shared>,
    details: bool,
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
        details,
        schedule: ReadSchedule::new(SETTLE),
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

/// When the worker reads and publishes the state: `settle` after the first
/// change since the last publish, or [`REFRESH_RETRY`] after a failed read.
/// Neither moves for later changes, so changes that keep coming (a slider
/// dragged in another mixer, commands from a client) are still published
/// regularly.
#[derive(Debug)]
struct ReadSchedule {
    settle: Duration,
    due: Option<Instant>,
    /// Set while a failed read on a live connection awaits its retry.
    retry_at: Option<Instant>,
}

impl ReadSchedule {
    fn new(settle: Duration) -> Self {
        Self {
            settle,
            due: None,
            retry_at: None,
        }
    }

    /// Notes that the server reported changes by `now`.
    fn changed(&mut self, now: Instant) {
        if self.retry_at.is_none() {
            self.due.get_or_insert(now + self.settle);
        }
    }

    /// Notes that the state was read at `now` for a command but not
    /// published.
    fn read_unpublished(&mut self, now: Instant) {
        self.retry_at = None;
        self.changed(now);
    }

    /// When the state is next read and published, if it is.
    fn next(&self) -> Option<Instant> {
        self.retry_at.or(self.due)
    }

    fn is_due(&self, now: Instant) -> bool {
        self.next().is_some_and(|at| now >= at)
    }

    /// Nothing is pending: after a publish, or without a connection.
    fn clear(&mut self) {
        self.due = None;
        self.retry_at = None;
    }

    /// A read on a live connection failed at `now`.
    fn failed(&mut self, now: Instant) {
        self.due = None;
        self.retry_at = Some(now + REFRESH_RETRY);
    }
}

struct Worker {
    /// Declared before `mainloop` so the context is freed first.
    conn: Option<Connection>,
    /// The last state read, patched with the effect of commands applied since.
    raw: RawState,
    shared: Arc<Shared>,
    /// Whether sessions get a [`Session::detail`](crate::Session::detail).
    details: bool,
    schedule: ReadSchedule,
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

    /// Refreshes once reported changes have settled, notices a lost
    /// connection and reconnects when the backoff allows. Returns when the
    /// loop should next wake up on its own, if ever.
    fn maintain(&mut self, now: Instant) -> Option<Instant> {
        if let Some(conn) = &self.conn {
            if conn.is_dead() {
                tracing::warn!("lost the PulseAudio connection, reconnecting");
                self.disconnect(now);
            } else {
                if conn.is_dirty() {
                    self.schedule.changed(now);
                }
                if self.schedule.is_due(now)
                    && let Err(e) = self.refresh(now + OP_TIMEOUT)
                {
                    self.refresh_failed(&e, now);
                }
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
            Some(_) => self.schedule.next(),
            None => self.reconnect.next_attempt,
        }
    }

    /// Drops the connection, keeping the cached state, and schedules a
    /// reconnect.
    fn disconnect(&mut self, now: Instant) {
        self.conn = None;
        self.schedule.clear();
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
                self.schedule.failed(now);
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

    /// Re-reads the server state into the cache. Changes reported while the
    /// read is in flight leave the connection dirty, so they are read again.
    fn read(&mut self, deadline: Instant) -> Result<(), OpError> {
        let conn = self.conn.as_ref().ok_or(OpError::Cancelled)?;
        conn.take_dirty();
        self.raw = conn.query(&mut self.mainloop, deadline, &self.raw.clients)?;
        Ok(())
    }

    /// Re-reads the server state, publishes it and emits the events that
    /// describe the change.
    fn refresh(&mut self, deadline: Instant) -> Result<(), OpError> {
        self.read(deadline)?;
        self.schedule.clear();
        let state = self.raw.to_audio_state(self.details);
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
    /// command is judged against the current state; that state is published
    /// only when the changes have settled (see [`ReadSchedule`]), like any
    /// other. The cache is not re-read afterwards: the commands patch it
    /// with what they set, and the change events the server sends in
    /// response trigger the authoritative read (and the
    /// [`AudioEvent`](crate::AudioEvent)s).
    fn handle(&mut self, command: &Command, deadline: Instant) -> Result<(), AudioError> {
        if Instant::now() >= deadline {
            return Err(backend_error("PulseAudio request expired before it ran"));
        }
        let not_connected = || backend_error("not connected to PulseAudio, reconnecting");
        let conn = self.conn.as_ref().filter(|c| !c.is_dead());
        let stale = conn.ok_or_else(not_connected)?.is_dirty() || self.schedule.retry_at.is_some();
        if stale {
            if let Err(e) = self.read(deadline) {
                self.refresh_failed(&e, Instant::now());
                return Err(backend_error(e));
            }
            self.schedule.read_unpublished(Instant::now());
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

    /// Sets volume and mute of every stream of a session on the default sink
    /// (see [`StreamGroup::changes`](super::model::StreamGroup::changes) for
    /// the streams left alone and when the request is refused).
    ///
    /// The server may reject a stream; the others are still set and the
    /// first rejection is returned. A stream that went away meanwhile is
    /// skipped, unless every stream did, which reports the session as
    /// unknown. A timeout or a lost connection stops at once.
    fn set_session(
        &mut self,
        session_id: &str,
        volume: f32,
        muted: bool,
    ) -> Result<(), AudioError> {
        let unknown = || AudioError::UnknownSession(session_id.to_owned());
        let changes = self
            .raw
            .default_group(session_id)
            .ok_or_else(unknown)?
            .changes(volume, muted)
            .ok_or_else(|| backend_error("the volume of this session cannot be changed"))?;
        let mut gone = 0;
        let mut rejected = None;
        for change in &changes {
            match self.set_stream(change) {
                Ok(()) => {}
                Err(e) if e.is_no_entity() => gone += 1,
                Err(e @ OpError::Failed(_)) => {
                    rejected.get_or_insert(e);
                }
                Err(e) => return Err(backend_error(e)),
            }
        }
        if gone > 0 && gone == changes.len() {
            return Err(unknown());
        }
        rejected.map_or(Ok(()), |e| Err(backend_error(e)))
    }

    /// Applies one stream's part of [`Self::set_session`] and records it in
    /// the cache.
    fn set_stream(&mut self, change: &StreamChange) -> Result<(), OpError> {
        let index = change.index;
        if let Some((target, cv)) = &change.volume {
            self.conn
                .set_sink_input_volume(self.mainloop, self.deadline, index, cv)?;
            if let Some(input) = self.raw.input_mut(index) {
                input.volume.fill(*target);
            }
        }
        if let Some(muted) = change.muted {
            self.conn
                .set_sink_input_mute(self.mainloop, self.deadline, index, muted)?;
            if let Some(input) = self.raw.input_mut(index) {
                input.muted = muted;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;
    use crate::AudioEvent;
    use crate::pulse::model::{AppProps, RawSink, RawSinkInput};

    /// Speakers with two streams of one Firefox tab, titled `first` and
    /// `second`.
    fn tab(first: &str, second: &str) -> RawState {
        let stream = |index: u32, title: &str| RawSinkInput {
            index,
            sink: 1,
            client: Some(9),
            volume: vec![65536; 2],
            has_volume: true,
            volume_writable: true,
            app: AppProps {
                name: Some("Firefox".into()),
                process_id: Some(500),
                ..AppProps::default()
            },
            media_name: Some(title.to_owned()),
            ..RawSinkInput::default()
        };
        RawState {
            default_sink: Some(CString::new("speakers").unwrap()),
            sinks: vec![RawSink {
                index: 1,
                name: CString::new("speakers").unwrap(),
                description: None,
                volume: vec![65536; 2],
                muted: false,
            }],
            inputs: vec![stream(10, first), stream(11, second)],
            clients: Default::default(),
        }
    }

    enum Step {
        /// The server state changes to this, and the server reports it.
        Server(RawState),
        /// A command arrives, reading the state if changes were reported.
        Command,
    }

    /// Runs `steps` (at a time in milliseconds) through a [`ReadSchedule`]
    /// the way [`Worker::serve`], [`Worker::handle`] and
    /// [`Worker::maintain`] use it, with a fake clock: the loop wakes at each
    /// step and when the schedule says, and a publish shows the server state
    /// of that moment. Returns when each publish happened and its events.
    fn play(
        base: &RawState,
        steps: &[(u64, Step)],
        settle: Duration,
    ) -> Vec<(u64, Vec<AudioEvent>)> {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut schedule = ReadSchedule::new(settle);
        let mut published = base.to_audio_state(true);
        let mut server = base;
        let mut dirty = false;
        let mut publishes = Vec::new();
        let mut steps = steps.iter().peekable();
        loop {
            let next_step = steps.peek().map(|(ms, _)| at(*ms));
            let Some(now) = [next_step, schedule.next()].into_iter().flatten().min() else {
                return publishes;
            };
            while let Some((_, step)) = steps.next_if(|(ms, _)| at(*ms) <= now) {
                match step {
                    Step::Server(raw) => {
                        server = raw;
                        dirty = true;
                    }
                    Step::Command if dirty => {
                        dirty = false;
                        schedule.read_unpublished(now);
                    }
                    Step::Command => {}
                }
            }
            if dirty {
                schedule.changed(now);
            }
            if schedule.is_due(now) {
                dirty = false;
                schedule.clear();
                let state = server.to_audio_state(true);
                let ms = now.duration_since(start).as_millis() as u64;
                publishes.push((ms, diff(&published, &state)));
                published = state;
            }
        }
    }

    fn retitled() -> Vec<AudioEvent> {
        vec![AudioEvent::SessionDetailChanged {
            id: "speakers|10".into(),
        }]
    }

    #[test]
    fn a_staggered_title_change_is_one_update() {
        let base = tab("Old", "Old");
        let steps = [
            (0, Step::Server(tab("New", "Old"))),
            (3, Step::Server(tab("New", "New"))),
        ];
        assert_eq!(play(&base, &steps, SETTLE), [(30, retitled())]);

        // Read at once, the group splits and merges again.
        assert_eq!(
            play(&base, &steps, Duration::ZERO),
            [
                (
                    0,
                    vec![
                        AudioEvent::SessionDetailChanged {
                            id: "speakers|10".into()
                        },
                        AudioEvent::SessionAdded {
                            id: "speakers|11".into()
                        },
                    ]
                ),
                (
                    3,
                    vec![AudioEvent::SessionRemoved {
                        id: "speakers|11".into()
                    }]
                ),
            ]
        );

        // A command in between reads the half-changed state without
        // publishing it.
        let steps = [
            (0, Step::Server(tab("New", "Old"))),
            (1, Step::Command),
            (3, Step::Server(tab("New", "New"))),
        ];
        assert_eq!(play(&base, &steps, SETTLE), [(30, retitled())]);
    }

    #[test]
    fn changes_that_keep_coming_are_published_regularly() {
        let base = tab("Old", "Old");
        let mut steps: Vec<(u64, Step)> = (0..10).map(|i| (i * 7, Step::Command)).collect();
        steps.insert(0, (0, Step::Server(tab("New", "New"))));
        steps.push((40, Step::Server(tab("Newer", "Newer"))));
        steps.push((41, Step::Command));
        steps.push((50, Step::Server(tab("Newest", "Newest"))));
        steps.sort_by_key(|(ms, _)| *ms);
        let times: Vec<u64> = play(&base, &steps, SETTLE)
            .into_iter()
            .map(|(ms, events)| {
                assert_eq!(events, retitled());
                ms
            })
            .collect();
        assert_eq!(times, [30, 70]);
    }

    #[test]
    fn a_failed_read_is_retried_after_its_delay() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut schedule = ReadSchedule::new(SETTLE);
        assert_eq!(schedule.next(), None);
        schedule.changed(at(0));
        schedule.changed(at(20));
        assert_eq!(schedule.next(), Some(at(30)));
        assert!(!schedule.is_due(at(29)) && schedule.is_due(at(30)));

        schedule.failed(at(30));
        let retry = at(30) + REFRESH_RETRY;
        assert_eq!(schedule.next(), Some(retry));
        schedule.changed(at(40));
        assert_eq!(schedule.next(), Some(retry), "changes wait for the retry");

        // A read for a command that succeeds ends the wait.
        schedule.read_unpublished(at(50));
        assert_eq!(schedule.next(), Some(at(80)));
        schedule.clear();
        assert_eq!(schedule.next(), None);
    }
}
