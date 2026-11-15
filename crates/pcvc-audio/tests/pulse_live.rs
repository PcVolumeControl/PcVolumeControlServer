//! Tests against the user's running PulseAudio or PipeWire server. Ignored by
//! default; run with
//!
//! ```text
//! cargo test -p pcvc-audio --test pulse_live -- --ignored --test-threads=1
//! ```
//!
//! They only change things the test creates: a silent `pw-play` stream (so
//! `pw-play` must be installed) and a `module-null-sink`. The null sink is
//! the default for well under two seconds, during which streams following
//! the default play into it (silence). The original default sink is restored
//! and the module unloaded by guards that also run when a test fails, and
//! each test checks that the default sink, its volume and its mute ended up as
//! they started.

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::operation::{Operation, State as OpState};
use libpulse_binding::time::MicroSeconds;
use pcvc_audio::{AudioBackend, AudioError, AudioEvent, AudioState, Device, PulseBackend};
use tokio::sync::broadcast;

const WAIT: Duration = Duration::from_secs(3);
const STREAM_NAME: &str = "pcvc-test-stream";

/// The tests share the default sink, so they run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A minimal synchronous control connection, independent of the backend
/// under test, used to set up and restore the server.
struct Ctl {
    context: Context,
    mainloop: Mainloop,
}

impl Ctl {
    fn connect() -> Self {
        let mut mainloop = Mainloop::new().expect("main loop");
        let mut context = Context::new(&mainloop, "pcvc-test-control").expect("context");
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

    fn load_null_sink(&mut self, sink_name: &str) -> u32 {
        let index = Rc::new(Cell::new(u32::MAX));
        let slot = Rc::clone(&index);
        let args = format!(
            "sink_name={sink_name} \
             sink_properties='device.description=PcvcTestSink state.restore-props=false'"
        );
        let op = self
            .context
            .introspect()
            .load_module("module-null-sink", &args, move |i| slot.set(i));
        assert!(self.wait(op), "loading module-null-sink timed out");
        assert_ne!(index.get(), u32::MAX, "loading module-null-sink failed");
        index.get()
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

/// A null sink that is unloaded, and the original default sink restored, on
/// drop.
struct NullSink {
    ctl: Ctl,
    module: u32,
    name: String,
    original_default: String,
}

impl NullSink {
    fn load(mut ctl: Ctl) -> Self {
        let original_default = ctl.default_sink();
        // A fixed name, because WirePlumber remembers every sink that was ever
        // the default; repeated runs then reuse one entry.
        let name = "pcvc_test_null".to_owned();
        let module = ctl.load_null_sink(&name);
        Self {
            ctl,
            module,
            name,
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

/// A `pw-play` process playing silence to the default sink, with WirePlumber
/// told not to remember its volume or routing. Killed on drop.
struct SilentStream(Child);

impl SilentStream {
    fn start() -> Self {
        let path = silent_wav();
        let child = Command::new("pw-play")
            .arg("-P")
            .arg(format!(
                "{{ application.name = \"{STREAM_NAME}\" \
                 state.restore-props = false state.restore-target = false }}"
            ))
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning pw-play");
        Self(child)
    }
}

impl Drop for SilentStream {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 60 s of 16-bit stereo silence at 48 kHz.
fn silent_wav() -> PathBuf {
    const RATE: u32 = 48_000;
    const SECONDS: u32 = 60;
    let data_len = RATE * SECONDS * 4;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pcvc-silence.wav");
    std::fs::write(&path, wav).expect("writing the silent WAV");
    path
}

/// Polls the backend until `check` accepts its state.
fn wait_for<T>(backend: &PulseBackend, what: &str, check: impl Fn(&AudioState) -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        let state = backend.snapshot().unwrap();
        if let Some(found) = check(&state) {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn default_device(backend: &PulseBackend) -> Device {
    backend
        .snapshot()
        .unwrap()
        .default_device
        .expect("default device")
}

fn drain(rx: &mut broadcast::Receiver<AudioEvent>) -> Vec<AudioEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
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
#[ignore = "needs a running PulseAudio or PipeWire server and pw-play"]
fn session_control_on_own_stream() {
    let _serial = serial();
    let mut ctl = Ctl::connect();
    let original_default = ctl.default_sink();
    let backend = PulseBackend::new().unwrap();
    let before = default_device(&backend);
    let mut events = backend.subscribe();

    let stream = SilentStream::start();
    let session = wait_for(&backend, "the test stream", |s| {
        s.default_device
            .as_ref()?
            .sessions
            .iter()
            .find(|s| s.name == STREAM_NAME)
            .cloned()
    });
    assert!(session.id.starts_with(&format!("{}|", before.id)));
    assert!(drain(&mut events).contains(&AudioEvent::SessionAdded {
        id: session.id.clone()
    }));

    let lower = (session.volume / 2.0).floor();
    backend.set_session(&session.id, lower, false).unwrap();
    wait_for(&backend, "the lowered volume", |s| {
        let found = s
            .default_device
            .as_ref()?
            .sessions
            .iter()
            .find(|x| x.id == session.id)?;
        ((found.volume - lower).abs() < 0.01 && !found.muted).then_some(())
    });
    backend.set_session(&session.id, lower, true).unwrap();
    wait_for(&backend, "the mute", |s| {
        let found = s
            .default_device
            .as_ref()?
            .sessions
            .iter()
            .find(|x| x.id == session.id)?;
        found.muted.then_some(())
    });
    assert!(drain(&mut events).contains(&AudioEvent::SessionChanged {
        id: session.id.clone()
    }));

    // Requests for sessions that are not on the default device.
    for bad in [
        "nonsense".to_owned(),
        format!("{}|4294967294", before.id),
        format!(
            "{}-other|{}",
            before.id,
            session.id.rsplit('|').next().unwrap()
        ),
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
        (!s.default_device
            .as_ref()?
            .sessions
            .iter()
            .any(|x| x.id == session.id))
        .then_some(())
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
#[ignore = "needs a running PulseAudio or PipeWire server and pw-play"]
fn default_device_and_master_on_null_sink() {
    let _serial = serial();
    let backend = PulseBackend::new().unwrap();
    let before = default_device(&backend);
    let stream = SilentStream::start();
    let own = wait_for(&backend, "the test stream", |s| {
        s.default_device
            .as_ref()?
            .sessions
            .iter()
            .find(|s| s.name == STREAM_NAME)
            .cloned()
    });
    let own_index = own.id.rsplit('|').next().unwrap().to_owned();

    let null = NullSink::load(Ctl::connect());
    assert_eq!(null.original_default, before.id);
    wait_for(&backend, "the null sink", |s| {
        s.devices.iter().any(|d| d.id == null.name).then_some(())
    });
    assert!(matches!(
        backend.set_default_device("pcvc-no-such-sink"),
        Err(AudioError::UnknownDevice(_))
    ));
    let mut events = backend.subscribe();

    // The null sink is the default from here until the restore below.
    let switched_at = Instant::now();
    backend.set_default_device(&null.name).unwrap();
    let on_null = wait_for(&backend, "the null sink to become the default", |s| {
        let d = s.default_device.as_ref()?;
        (d.id == null.name).then(|| d.clone())
    });
    assert_eq!(on_null.name, "PcvcTestSink");
    let followed = format!("{}|{own_index}", null.name);
    wait_for(&backend, "the test stream to follow the default", |s| {
        s.default_device
            .as_ref()?
            .sessions
            .iter()
            .any(|x| x.id == followed)
            .then_some(())
    });

    let lower = (on_null.volume / 2.0).floor();
    backend.set_master(Some(lower), Some(true)).unwrap();
    wait_for(&backend, "master volume and mute", |s| {
        let d = s.default_device.as_ref()?;
        (d.id == null.name && (d.volume - lower).abs() < 0.01 && d.muted).then_some(())
    });
    backend.set_master(None, Some(false)).unwrap();
    wait_for(&backend, "master unmute", |s| {
        let d = s.default_device.as_ref()?;
        (d.id == null.name && !d.muted && (d.volume - lower).abs() < 0.01).then_some(())
    });

    backend.set_default_device(&before.id).unwrap();
    wait_for(&backend, "the original default", |s| {
        (s.default_device.as_ref()?.id == before.id).then_some(())
    });
    let null_default_for = switched_at.elapsed();
    assert!(
        null_default_for < Duration::from_secs(2),
        "null sink was the default for {null_default_for:?}"
    );
    wait_for(&backend, "the test stream to follow back", |s| {
        s.default_device
            .as_ref()?
            .sessions
            .iter()
            .any(|x| x.id == own.id)
            .then_some(())
    });
    let seen = drain(&mut events);
    assert!(seen.contains(&AudioEvent::DefaultDeviceChanged), "{seen:?}");
    assert!(seen.contains(&AudioEvent::MasterChanged), "{seen:?}");

    let null_name = null.name.clone();
    drop(null);
    drop(stream);
    wait_for(&backend, "the null sink to go away", |s| {
        (!s.devices.iter().any(|d| d.id == null_name)).then_some(())
    });
    let after = default_device(&backend);
    assert_eq!(Ctl::connect().default_sink(), before.id);
    assert_eq!(
        (&after.id, after.volume, after.muted),
        (&before.id, before.volume, before.muted)
    );
    eprintln!("null sink was the default for {null_default_for:?}");
}
