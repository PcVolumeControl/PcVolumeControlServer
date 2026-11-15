//! Tests against the user's running PulseAudio or PipeWire server. Ignored by
//! default; run with
//!
//! ```text
//! cargo test -p pcvc-audio --test pulse_live -- --ignored --test-threads=1
//! ```
//!
//! Volume and mute are only changed on things the tests create: silent
//! playback streams opened by the test process and a `module-null-sink`.
//!
//! Making the null sink the default (for at most [`NULL_DEFAULT_BUDGET`])
//! also moves every stream on the real default sink to it and back, so they
//! play into the null sink meanwhile; moving a stream to the default sink
//! makes it follow the default from then on, which resets a stream that was
//! pinned to the real default sink. A watchdog thread restores the original
//! default sink once the budget is spent; a guard restores it and unloads the
//! module when the test ends or fails; and a run killed before its guard ran
//! is cleaned up by the next one. Each test checks that the default sink,
//! its volume and its mute ended up as they started.

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::operation::{Operation, State as OpState};
use libpulse_binding::proplist::Proplist;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{FlagSet as StreamFlagSet, SeekMode, State as StreamState, Stream};
use libpulse_binding::time::MicroSeconds;
use libpulse_binding::volume::{ChannelVolumes, Volume};
use pcvc_audio::{AudioBackend, AudioError, AudioEvent, AudioState, Device, PulseBackend, Session};
use tokio::sync::broadcast::{self, error::TryRecvError};

const WAIT: Duration = Duration::from_secs(3);

/// Longest time the null sink may stay the default. Every wait while it is
/// the default ends by then, so a slow server fails the test (and the guard
/// restores the default) instead of extending the window.
const NULL_DEFAULT_BUDGET: Duration = Duration::from_millis(1500);

const STREAM_NAME: &str = "pcvc-test-stream";

/// The test streams' initial volume, just under 80 %.
const STREAM_VOLUME: u32 = Volume::NORMAL.0 / 5 * 4;

/// A fixed name, because WirePlumber remembers every sink that was ever the
/// default; repeated runs then reuse one entry.
const NULL_SINK: &str = "pcvc_test_null";
const NULL_SINK_DESCRIPTION: &str = "PcvcTestSink";

/// The tests share the default sink, so they run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A minimal synchronous connection, independent of the backend under test,
/// used to set up and restore the server and to play the test stream.
struct Ctl {
    context: Context,
    mainloop: Mainloop,
}

impl Ctl {
    fn connect() -> Self {
        Self::connect_as("pcvc-test-control")
    }

    /// Connects with `name` as the client's `application.name`.
    fn connect_as(name: &str) -> Self {
        let mut mainloop = Mainloop::new().expect("main loop");
        let mut context = Context::new(&mainloop, name).expect("context");
        context
            .connect(None, FlagSet::NOAUTOSPAWN, None)
            .expect("connect");
        let deadline = Instant::now() + WAIT;
        while context.get_state() != State::Ready {
            assert!(context.get_state().is_good(), "control connection failed");
            assert!(Instant::now() < deadline, "control connection timed out");
            iterate(&mut mainloop);
        }
        Self { context, mainloop }
    }

    /// Waits for an operation without panicking, since the restore guard
    /// calls this from `Drop`. Returns false on a timeout.
    fn wait<T: ?Sized>(&mut self, mut op: Operation<T>) -> bool {
        let deadline = Instant::now() + WAIT;
        while op.get_state() == OpState::Running {
            if Instant::now() >= deadline {
                op.cancel();
                return false;
            }
            iterate(&mut self.mainloop);
        }
        true
    }

    fn try_default_sink(&mut self) -> Option<String> {
        let name = Rc::new(RefCell::new(None));
        let slot = Rc::clone(&name);
        let op = self.context.introspect().get_server_info(move |info| {
            *slot.borrow_mut() = info.default_sink_name.as_ref().map(|n| n.to_string());
        });
        self.wait(op);
        name.take()
    }

    fn default_sink(&mut self) -> String {
        self.try_default_sink().expect("server has a default sink")
    }

    fn set_default_sink(&mut self, name: &str) -> bool {
        let ok = Rc::new(Cell::new(false));
        let slot = Rc::clone(&ok);
        let op = self.context.set_default_sink(name, move |s| slot.set(s));
        self.wait(op) && ok.get()
    }

    /// Modules loaded by an earlier run that died before its guard ran.
    fn stale_null_sinks(&mut self) -> Vec<u32> {
        let found = Rc::new(RefCell::new(Vec::new()));
        let slot = Rc::clone(&found);
        let marker = format!("sink_name={NULL_SINK} ");
        let op = self.context.introspect().get_module_info_list(move |item| {
            if let ListResult::Item(m) = item
                && m.name.as_deref() == Some("module-null-sink")
                && m.argument.as_deref().is_some_and(|a| a.contains(&marker))
            {
                slot.borrow_mut().push(m.index);
            }
        });
        assert!(self.wait(op), "listing modules timed out");
        found.take()
    }

    fn load_null_sink(&mut self) -> u32 {
        let index = Rc::new(Cell::new(u32::MAX));
        let slot = Rc::clone(&index);
        let args = format!(
            "sink_name={NULL_SINK} \
             sink_properties='device.description={NULL_SINK_DESCRIPTION} state.restore-props=false'"
        );
        let op = self
            .context
            .introspect()
            .load_module("module-null-sink", &args, move |i| slot.set(i));
        assert!(self.wait(op), "loading module-null-sink timed out");
        assert_ne!(index.get(), u32::MAX, "loading module-null-sink failed");
        index.get()
    }

    /// The volume (average, in percent) and mute of one of the test's own
    /// streams, or `None` once it is gone. Only the test's streams are read
    /// this way: the safe binding converts their sample spec and channel map
    /// into enums, which is sound for the values the test chose.
    fn own_stream(&mut self, index: u32) -> Option<(f32, bool)> {
        let found = Rc::new(Cell::new(None));
        let slot = Rc::clone(&found);
        let op = self
            .context
            .introspect()
            .get_sink_input_info(index, move |item| {
                if let ListResult::Item(info) = item {
                    slot.set(Some((volume_to_percent(info.volume.avg().0), info.mute)));
                }
            });
        assert!(self.wait(op), "reading sink-input {index} timed out");
        found.get()
    }

    /// Polls [`Self::own_stream`] until `check` accepts its volume and mute.
    fn wait_for_own_stream(&mut self, index: u32, check: impl Fn(f32, bool) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let state = self.own_stream(index);
            if state.is_some_and(|(volume, muted)| check(volume, muted)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "sink-input {index} stayed at {state:?}"
            );
            iterate(&mut self.mainloop);
        }
    }

    fn unload_module(&mut self, index: u32) -> bool {
        let ok = Rc::new(Cell::new(false));
        let slot = Rc::clone(&ok);
        let op = self
            .context
            .introspect()
            .unload_module(index, move |s| slot.set(s));
        self.wait(op) && ok.get()
    }
}

fn volume_to_percent(raw: u32) -> f32 {
    raw as f32 * 100.0 / Volume::NORMAL.0 as f32
}

fn iterate(mainloop: &mut Mainloop) {
    let result = mainloop
        .prepare(Some(MicroSeconds(20_000)))
        .and_then(|()| mainloop.poll())
        .and_then(|_| mainloop.dispatch());
    if let Err(e) = result {
        eprintln!("main loop iteration failed: {e}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Unloads null sinks left by a run that was killed before its guard ran.
/// If one of them was the default, the server falls back to another sink
/// when it goes away; that sink is then selected explicitly, so the server
/// no longer remembers the test sink as the configured default (WirePlumber
/// would otherwise make the next test sink the default as soon as it loads).
fn remove_stale_null_sinks() {
    let mut ctl = Ctl::connect();
    let was_default = ctl.try_default_sink().as_deref() == Some(NULL_SINK);
    for stale in ctl.stale_null_sinks() {
        assert!(ctl.unload_module(stale), "unloading stale module {stale}");
    }
    if was_default {
        let deadline = Instant::now() + WAIT;
        let fallback = loop {
            match ctl.try_default_sink() {
                Some(name) if name != NULL_SINK => break name,
                _ => {
                    assert!(Instant::now() < deadline, "no default sink after cleanup");
                    iterate(&mut ctl.mainloop);
                }
            }
        };
        assert!(ctl.set_default_sink(&fallback), "selecting {fallback}");
        eprintln!("an interrupted run left the test sink as the default; selected {fallback}");
    }
}

/// A null sink that is unloaded, and the original default sink restored, on
/// drop.
struct NullSink {
    ctl: Ctl,
    module: u32,
    original_default: String,
}

impl NullSink {
    fn load(mut ctl: Ctl) -> Self {
        let original_default = ctl.default_sink();
        assert_ne!(
            original_default, NULL_SINK,
            "run remove_stale_null_sinks first"
        );
        let module = ctl.load_null_sink();
        Self {
            ctl,
            module,
            original_default,
        }
    }
}

impl Drop for NullSink {
    fn drop(&mut self) {
        if self.ctl.try_default_sink().as_ref() != Some(&self.original_default) {
            let restored = self.ctl.set_default_sink(&self.original_default);
            eprintln!("restored the default sink: {restored}");
        }
        if !self.ctl.unload_module(self.module) {
            eprintln!("unloading null sink module {} failed", self.module);
        }
    }
}

/// Restores `original` as the default sink from another thread once
/// `budget` has passed, unless dropped first. This bounds how long the test
/// sink stays the default even while the test thread is blocked in a call.
struct Watchdog {
    cancel: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn arm(original: String, budget: Duration) -> Self {
        let (cancel, cancelled) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            if cancelled.recv_timeout(budget) != Err(mpsc::RecvTimeoutError::Timeout) {
                return;
            }
            let mut ctl = Ctl::connect();
            if ctl.try_default_sink().as_deref() == Some(NULL_SINK) {
                let restored = ctl.set_default_sink(&original);
                eprintln!("watchdog restored the default sink: {restored}");
            }
        });
        Self {
            cancel: Some(cancel),
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.cancel.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A playback stream of silence from this process, on its own connection
/// and thread, with WirePlumber told not to remember its volume or routing.
/// It ends on drop, or with the process.
struct SilentStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The sink-input index.
    index: u32,
}

impl SilentStream {
    fn start() -> Self {
        Self::start_as("silence")
    }

    /// A stream with `media_name` as its `media.name`.
    fn start_as(media_name: &'static str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let flag = Arc::clone(&stop);
        // Built before waiting, so its drop also stops a stream that
        // becomes ready too late.
        let mut stream = Self {
            stop,
            thread: Some(std::thread::spawn(move || {
                play_silence(&flag, &ready_tx, media_name);
            })),
            index: u32::MAX,
        };
        stream.index = ready_rx
            .recv_timeout(WAIT)
            .expect("the silent stream did not start");
        stream
    }
}

impl Drop for SilentStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn play_silence(stop: &AtomicBool, ready: &mpsc::Sender<u32>, media_name: &str) {
    let mut ctl = Ctl::connect_as(STREAM_NAME);
    let mut stream_props = Proplist::new().expect("proplist");
    for key in ["state.restore-props", "state.restore-target"] {
        stream_props.set_str(key, "false").expect("property");
    }
    let spec = Spec {
        format: Format::S16le,
        channels: 2,
        rate: 48_000,
    };
    let frame = spec.frame_size();
    let mut stream =
        Stream::new_with_proplist(&mut ctl.context, media_name, &spec, None, &mut stream_props)
            .expect("stream");
    // Without an initial volume, PipeWire reports 0 until the volume is
    // first set.
    let mut volume = ChannelVolumes::default();
    volume.set(spec.channels, Volume(STREAM_VOLUME));
    stream
        .connect_playback(None, None, StreamFlagSet::NOFLAGS, Some(&volume), None)
        .expect("connect playback");
    let deadline = Instant::now() + WAIT;
    while stream.get_state() != StreamState::Ready {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        assert!(stream.get_state().is_good(), "the stream failed");
        assert!(Instant::now() < deadline, "the stream timed out");
        iterate(&mut ctl.mainloop);
    }
    let _ = ready.send(stream.get_index().expect("a ready stream has an index"));
    let zeros = vec![0u8; 64 * 1024];
    while !stop.load(Ordering::Relaxed) {
        if let Some(n) = stream.writable_size() {
            // Whole frames only, as `write` requires.
            let len = n.min(zeros.len()) / frame * frame;
            if len > 0 {
                let _ = stream.write(&zeros[..len], None, 0, SeekMode::Relative);
            }
        }
        iterate(&mut ctl.mainloop);
    }
    let _ = stream.disconnect();
}

/// Polls the backend until `check` accepts its state, failing at `deadline`.
fn wait_until<T>(
    backend: &PulseBackend,
    what: &str,
    deadline: Instant,
    check: impl Fn(&AudioState) -> Option<T>,
) -> T {
    loop {
        let state = backend.snapshot().unwrap();
        if let Some(found) = check(&state) {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for<T>(backend: &PulseBackend, what: &str, check: impl Fn(&AudioState) -> Option<T>) -> T {
    wait_until(backend, what, Instant::now() + WAIT, check)
}

fn default_device(backend: &PulseBackend) -> Device {
    backend
        .snapshot()
        .unwrap()
        .default_device
        .expect("default device")
}

fn own_session(state: &AudioState) -> Option<Session> {
    state
        .default_device
        .as_ref()?
        .sessions
        .iter()
        .find(|s| s.name == STREAM_NAME)
        .cloned()
}

fn session_by_id(state: &AudioState, id: &str) -> Option<Session> {
    state
        .default_device
        .as_ref()?
        .sessions
        .iter()
        .find(|s| s.id == id)
        .cloned()
}

/// Everything received so far. Fails if the receiver lagged, since the
/// events the tests look for may then be the ones that were dropped.
fn drain(rx: &mut broadcast::Receiver<AudioEvent>) -> Vec<AudioEvent> {
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Lagged(n)) => panic!("the event receiver lagged by {n}"),
            Err(TryRecvError::Empty | TryRecvError::Closed) => return events,
        }
    }
}

#[test]
#[ignore = "needs a running PulseAudio or PipeWire server"]
fn snapshot_lists_sinks() {
    let _serial = serial();
    let backend = PulseBackend::new().unwrap();
    let state = backend.snapshot().unwrap();
    assert!(!state.devices.is_empty());
    let device = state.default_device.as_ref().expect("default device");
    assert!(state.devices.iter().any(|d| d.id == device.id));
    assert!((0.0..=100.0).contains(&device.volume));
    for s in &device.sessions {
        assert!(s.id.starts_with(&format!("{}|", device.id)), "{}", s.id);
        assert!((0.0..=100.0).contains(&s.volume));
        assert!(!s.name.is_empty());
    }
    eprintln!("{state:#?}");
}

#[test]
#[ignore = "needs a running PulseAudio or PipeWire server"]
fn session_control_on_own_stream() {
    let _serial = serial();
    let mut ctl = Ctl::connect();
    let original_default = ctl.default_sink();
    let backend = PulseBackend::new().unwrap();
    let before = default_device(&backend);
    let mut events = backend.subscribe();

    let stream = SilentStream::start();
    let session = wait_for(&backend, "the test stream", own_session);
    assert!(session.id.starts_with(&format!("{}|", before.id)));
    assert!(drain(&mut events).contains(&AudioEvent::SessionAdded {
        id: session.id.clone()
    }));

    let find = |s: &AudioState| session_by_id(s, &session.id);
    let lower = (session.volume / 2.0).floor();
    backend.set_session(&session.id, lower, false).unwrap();
    wait_for(&backend, "the lowered volume", |s| {
        find(s).filter(|x| (x.volume - lower).abs() < 0.01 && !x.muted)
    });
    backend.set_session(&session.id, lower, true).unwrap();
    wait_for(&backend, "the mute", |s| find(s).filter(|x| x.muted));
    assert!(drain(&mut events).contains(&AudioEvent::SessionChanged {
        id: session.id.clone()
    }));

    // Requests for sessions that are not on the default device.
    let index = session.id.rsplit('|').next().unwrap();
    for bad in [
        "nonsense".to_owned(),
        format!("{}|4294967294", before.id),
        format!("{}-other|{index}", before.id),
    ] {
        assert!(
            matches!(
                backend.set_session(&bad, 1.0, true),
                Err(AudioError::UnknownSession(_))
            ),
            "{bad}"
        );
    }

    backend
        .set_session(&session.id, session.volume, session.muted)
        .unwrap();
    drop(stream);
    wait_for(&backend, "the stream to go away", |s| {
        find(s).is_none().then_some(())
    });
    assert!(drain(&mut events).contains(&AudioEvent::SessionRemoved {
        id: session.id.clone()
    }));
    assert!(matches!(
        backend.set_session(&session.id, 1.0, true),
        Err(AudioError::UnknownSession(_))
    ));

    let after = default_device(&backend);
    assert_eq!(ctl.default_sink(), original_default);
    assert_eq!(
        (&after.id, after.volume, after.muted),
        (&before.id, before.volume, before.muted)
    );
}

#[test]
#[ignore = "needs a running PulseAudio or PipeWire server"]
fn default_device_and_master_on_null_sink() {
    let _serial = serial();
    // Before the backend starts, so its first state read sees no leftovers.
    remove_stale_null_sinks();
    let backend = PulseBackend::new().unwrap();
    let before = default_device(&backend);
    let stream = SilentStream::start();
    let own = wait_for(&backend, "the test stream", own_session);
    let own_index = own.id.rsplit('|').next().unwrap().to_owned();

    let mut null = NullSink::load(Ctl::connect());
    assert_eq!(null.original_default, before.id);
    // The leftover cleanup recognises a module loaded this way.
    assert_eq!(null.ctl.stale_null_sinks(), vec![null.module]);
    wait_for(&backend, "the null sink", |s| {
        s.devices.iter().any(|d| d.id == NULL_SINK).then_some(())
    });
    assert!(matches!(
        backend.set_default_device("pcvc-no-such-sink"),
        Err(AudioError::UnknownDevice(_))
    ));
    let mut events = backend.subscribe();

    // The null sink is the default from here until the restore below.
    let switched_at = Instant::now();
    let window_ends = switched_at + NULL_DEFAULT_BUDGET;
    let watchdog = Watchdog::arm(before.id.clone(), NULL_DEFAULT_BUDGET);
    backend.set_default_device(NULL_SINK).unwrap();
    let on_null = wait_until(&backend, "the null sink default", window_ends, |s| {
        let d = s.default_device.as_ref()?;
        (d.id == NULL_SINK).then(|| d.clone())
    });
    assert_eq!(on_null.name, NULL_SINK_DESCRIPTION);
    let seen = drain(&mut events);
    assert!(seen.contains(&AudioEvent::DefaultDeviceChanged), "{seen:?}");
    let followed = format!("{NULL_SINK}|{own_index}");
    wait_until(&backend, "the stream to follow", window_ends, |s| {
        session_by_id(s, &followed)
    });

    let lower = (on_null.volume / 2.0).floor();
    backend.set_master(Some(lower), Some(true)).unwrap();
    wait_until(&backend, "master volume and mute", window_ends, |s| {
        let d = s.default_device.as_ref()?;
        (d.id == NULL_SINK && (d.volume - lower).abs() < 0.01 && d.muted).then_some(())
    });
    backend.set_master(None, Some(false)).unwrap();
    wait_until(&backend, "master unmute", window_ends, |s| {
        let d = s.default_device.as_ref()?;
        (d.id == NULL_SINK && !d.muted && (d.volume - lower).abs() < 0.01).then_some(())
    });
    let seen = drain(&mut events);
    assert!(seen.contains(&AudioEvent::MasterChanged), "{seen:?}");

    backend.set_default_device(&before.id).unwrap();
    wait_until(&backend, "the original default", window_ends, |s| {
        (s.default_device.as_ref()?.id == before.id).then_some(())
    });
    let null_default_for = switched_at.elapsed();
    drop(watchdog);
    assert!(
        null_default_for <= NULL_DEFAULT_BUDGET,
        "the null sink was the default for {null_default_for:?}"
    );
    let seen = drain(&mut events);
    assert!(seen.contains(&AudioEvent::DefaultDeviceChanged), "{seen:?}");
    wait_for(&backend, "the test stream to follow back", |s| {
        session_by_id(s, &own.id)
    });

    drop(null);
    drop(stream);
    wait_for(&backend, "the null sink to go away", |s| {
        (!s.devices.iter().any(|d| d.id == NULL_SINK)).then_some(())
    });
    let after = default_device(&backend);
    assert_eq!(Ctl::connect().default_sink(), before.id);
    assert_eq!(
        (&after.id, after.volume, after.muted),
        (&before.id, before.volume, before.muted)
    );
    eprintln!("null sink was the default for {null_default_for:?}");
}

/// Two streams of this process with the same `media.name` are one session:
/// it is set as a whole, and it outlives its representative.
#[test]
#[ignore = "needs a running PulseAudio or PipeWire server"]
fn streams_that_look_the_same_are_one_session() {
    const MEDIA_NAME: &str = "pcvc-test-group";
    let _serial = serial();
    let mut ctl = Ctl::connect();
    let original_default = ctl.default_sink();
    let backend = PulseBackend::with_details(true).unwrap();
    let before = default_device(&backend);
    let mut events = backend.subscribe();

    let first = SilentStream::start_as(MEDIA_NAME);
    let second = SilentStream::start_as(MEDIA_NAME);
    let (rep, other) = (first.index.min(second.index), first.index.max(second.index));
    let rep_id = format!("{}|{rep}", before.id);
    let other_id = format!("{}|{other}", before.id);
    let ours = |s: &AudioState| -> Vec<Session> {
        s.default_device
            .iter()
            .flat_map(|d| &d.sessions)
            .filter(|x| x.name == STREAM_NAME)
            .cloned()
            .collect()
    };
    // The backend knows both streams once the other one's id addresses the
    // group; an echo of the shown state changes nothing.
    let session = wait_for(&backend, "one session for both streams", |s| {
        let ours = ours(s);
        let [session] = ours.as_slice() else {
            return None;
        };
        (session.id == rep_id
            && session.detail.as_deref() == Some(MEDIA_NAME)
            && backend
                .set_session(&other_id, session.volume, session.muted)
                .is_ok())
        .then(|| session.clone())
    });

    let initial = volume_to_percent(STREAM_VOLUME);
    assert!((session.volume - initial).abs() < 0.01, "{session:?}");
    for index in [rep, other] {
        let (volume, muted) = ctl.own_stream(index).expect("the test stream");
        assert!(
            (volume - initial).abs() < 0.01 && !muted,
            "{index}: {volume} {muted}"
        );
    }

    // Set through the id of the stream that is not the representative.
    let lower = (session.volume / 2.0).floor();
    backend.set_session(&other_id, lower, true).unwrap();
    for index in [rep, other] {
        ctl.wait_for_own_stream(index, |volume, muted| {
            (volume - lower).abs() < 0.01 && muted
        });
    }
    wait_for(&backend, "the lowered, muted session", |s| {
        session_by_id(s, &rep_id).filter(|x| (x.volume - lower).abs() < 0.01 && x.muted)
    });
    backend.set_session(&rep_id, lower, false).unwrap();
    for index in [rep, other] {
        ctl.wait_for_own_stream(index, |volume, muted| {
            (volume - lower).abs() < 0.01 && !muted
        });
    }
    wait_for(&backend, "the unmuted session", |s| {
        session_by_id(s, &rep_id).filter(|x| !x.muted)
    });
    drain(&mut events);

    // The representative goes away; the session continues under the other
    // stream's id, as one row throughout.
    let (oldest, newest) = if first.index == rep {
        (first, second)
    } else {
        (second, first)
    };
    drop(oldest);
    let continued = wait_for(&backend, "the session under the other id", |s| {
        let ours = ours(s);
        assert!(ours.len() <= 1, "the group split: {ours:?}");
        ours.into_iter().find(|session| session.id == other_id)
    });
    assert!((continued.volume - lower).abs() < 0.01, "{continued:?}");
    assert_eq!(
        (continued.muted, continued.detail.as_deref()),
        (false, Some(MEDIA_NAME))
    );
    let seen = drain(&mut events);
    assert!(
        seen.contains(&AudioEvent::SessionRemoved { id: rep_id.clone() })
            && seen.contains(&AudioEvent::SessionAdded {
                id: other_id.clone()
            }),
        "{seen:?}"
    );
    assert!(matches!(
        backend.set_session(&rep_id, 1.0, true),
        Err(AudioError::UnknownSession(_))
    ));

    drop(newest);
    wait_for(&backend, "the streams to go away", |s| {
        ours(s).is_empty().then_some(())
    });
    let after = default_device(&backend);
    assert_eq!(ctl.default_sink(), original_default);
    assert_eq!(
        (&after.id, after.volume, after.muted),
        (&before.id, before.volume, before.muted)
    );
}

/// Commands sent back to back, while the backend is busy reading the state
/// changes the previous ones caused, are each answered promptly.
#[test]
#[ignore = "needs a running PulseAudio or PipeWire server"]
fn rapid_commands_are_answered_promptly() {
    let _serial = serial();
    let backend = PulseBackend::new().unwrap();
    let stream = SilentStream::start();
    let session = wait_for(&backend, "the test stream", own_session);
    let mut slowest = Duration::ZERO;
    for i in 0..200 {
        let started = Instant::now();
        backend
            .set_session(&session.id, session.volume, i % 2 == 0)
            .unwrap();
        slowest = slowest.max(started.elapsed());
        if i % 10 == 0 {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    drop(stream);
    eprintln!("slowest command took {slowest:?}");
    assert!(slowest < Duration::from_millis(500), "{slowest:?}");
}
