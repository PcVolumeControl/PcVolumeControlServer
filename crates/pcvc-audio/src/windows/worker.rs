//! The COM thread: owns every Core Audio object, keeps the cached
//! [`AudioState`] current and executes commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ::windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use ::windows::Win32::Media::Audio::{
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    ISimpleAudioVolume,
};
use ::windows::core::Interface;
use tokio::sync::broadcast;

use super::com::{self, Apartment};
use super::logic::{self, Change, Dirty};
use super::policy_config;
use crate::{AudioError, AudioEvent, AudioState, Device, DeviceInfo, Session};

const EVENT_CAPACITY: usize = 64;

/// Delay before re-reading after a failed refresh.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Most messages handled before the cache is refreshed, so a flood of
/// notifications cannot starve commands.
const MAX_BATCH: usize = 256;

/// State shared between the COM thread and [`super::WindowsBackend`].
pub(super) struct Shared {
    state: Mutex<AudioState>,
    pub events: broadcast::Sender<AudioEvent>,
    alive: AtomicBool,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            state: Mutex::default(),
            events: broadcast::channel(EVENT_CAPACITY).0,
            alive: AtomicBool::new(true),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, AudioState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// False once the COM thread has exited.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

pub(super) type Reply = SyncSender<Result<(), AudioError>>;

pub(super) enum Msg {
    /// Posted by Core Audio callbacks.
    #[allow(dead_code)]
    Changed(Change),
    Command(Request, Reply),
    Shutdown,
}

pub(super) enum Request {
    DefaultDevice {
        device_id: String,
    },
    Master {
        volume: Option<f32>,
        muted: Option<bool>,
    },
    Session {
        session_id: String,
        volume: f32,
        muted: bool,
    },
}

/// Body of the COM thread. Reports the outcome of start-up on `init`, then
/// serves `rx` until [`Msg::Shutdown`].
pub(super) fn run(
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    init: SyncSender<Result<(), AudioError>>,
) {
    let _alive = AliveGuard(Arc::clone(&shared));
    let apartment = match Apartment::init() {
        Ok(apartment) => apartment,
        Err(e) => {
            let _ = init.send(Err(com_error("CoInitializeEx", e)));
            return;
        }
    };
    let worker = Worker::new(shared, tx);
    let mut worker = match worker {
        Ok(worker) => worker,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    let _ = init.send(Ok(()));
    worker.serve(&rx);
    // Every COM object must be released before the apartment is left.
    drop(worker);
    drop(apartment);
}

/// Marks the backend dead when the COM thread exits, including by panic.
struct AliveGuard(Arc<Shared>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Release);
    }
}

pub(super) fn com_error(context: &str, e: ::windows::core::Error) -> AudioError {
    AudioError::Backend(format!("{context}: {e}").into())
}

/// An active render endpoint.
struct Endpoint {
    endpoint_id: String,
    info: DeviceInfo,
}

struct Worker {
    shared: Arc<Shared>,
    #[allow(dead_code)]
    tx: Sender<Msg>,
    enumerator: IMMDeviceEnumerator,
    devices: Vec<Endpoint>,
    default: Option<DefaultDevice>,
    dirty: Dirty,
    /// Rebuild the default device's objects even if its id is unchanged,
    /// after one of them failed.
    reopen_default: bool,
    retry_at: Option<Instant>,
}

impl Worker {
    fn new(shared: Arc<Shared>, tx: Sender<Msg>) -> Result<Self, AudioError> {
        let enumerator =
            com::create_enumerator().map_err(|e| com_error("creating IMMDeviceEnumerator", e))?;
        let mut worker = Self {
            shared,
            tx,
            enumerator,
            devices: Vec::new(),
            default: None,
            dirty: Dirty::default(),
            reopen_default: false,
            retry_at: None,
        };
        worker.refresh(Dirty::all())?;
        worker.publish();
        Ok(worker)
    }

    fn serve(&mut self, rx: &Receiver<Msg>) {
        let mut replies = Vec::new();
        loop {
            let first = match self.retry_at {
                None => match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return,
                },
                Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(msg) => Some(msg),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            let mut shutdown = false;
            let batch = first
                .into_iter()
                .chain(std::iter::from_fn(|| rx.try_recv().ok()))
                .take(MAX_BATCH);
            for msg in batch {
                match msg {
                    Msg::Changed(change) => self.dirty.mark(change),
                    Msg::Command(request, reply) => {
                        let result = self.execute(request);
                        replies.push((reply, result));
                    }
                    Msg::Shutdown => {
                        shutdown = true;
                        break;
                    }
                }
            }
            self.flush();
            // After the flush, so a caller sees its change in the snapshot.
            for (reply, result) in replies.drain(..) {
                let _ = reply.send(result);
            }
            if shutdown {
                return;
            }
        }
    }

    /// Runs a command against fresh state, and marks what it changes so the
    /// next flush re-reads it even if Core Audio's notification is late.
    fn execute(&mut self, request: Request) -> Result<(), AudioError> {
        self.flush();
        match request {
            Request::DefaultDevice { device_id } => {
                let endpoint_id = match self.endpoint_id_of(&device_id) {
                    Some(id) => id,
                    None => {
                        // The device may have appeared since the last refresh.
                        self.devices = self.enumerate_devices()?;
                        self.endpoint_id_of(&device_id)
                            .ok_or(AudioError::UnknownDevice(device_id))?
                    }
                };
                self.dirty.mark(Change::Default);
                policy_config::set_default_endpoint(&endpoint_id)
                    .map_err(|e| com_error("IPolicyConfig::SetDefaultEndpoint", e))
            }
            Request::Master { volume, muted } => {
                let default = self.default.as_ref().ok_or(AudioError::NoDefaultDevice)?;
                self.dirty.mark(Change::Master);
                com::set_master(&default.volume, volume, muted)
                    .map_err(|e| com_error("setting master volume", e))
            }
            Request::Session {
                session_id,
                volume,
                muted,
            } => {
                let default = self.default.as_ref().ok_or(AudioError::NoDefaultDevice)?;
                let session = default
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .ok_or(AudioError::UnknownSession(session_id))?;
                self.dirty.mark(Change::SessionValues);
                com::set_session_volume(&session.simple, volume, muted)
                    .map_err(|e| com_error("setting session volume", e))
            }
        }
    }

    fn endpoint_id_of(&self, device_id: &str) -> Option<String> {
        self.devices
            .iter()
            .find(|e| e.info.id == device_id)
            .map(|e| e.endpoint_id.clone())
    }

    /// Re-reads whatever is dirty and publishes the result.
    fn flush(&mut self) {
        if self.dirty.is_empty() && self.retry_at.is_none() {
            return;
        }
        let dirty = std::mem::take(&mut self.dirty);
        match self.refresh(dirty) {
            Ok(()) => self.retry_at = None,
            Err(e) => {
                tracing::warn!("refreshing audio state failed, retrying: {e}");
                self.dirty.merge(dirty);
                self.dirty.mark(Change::Default);
                self.reopen_default = true;
                self.retry_at = Some(Instant::now() + RETRY_INTERVAL);
            }
        }
        self.publish();
    }

    fn refresh(&mut self, dirty: Dirty) -> Result<(), AudioError> {
        if dirty.devices {
            self.devices = self.enumerate_devices()?;
        }
        let reopened = if dirty.devices || dirty.default || self.reopen_default {
            self.update_default()?
        } else {
            false
        };
        if let Some(default) = self.default.as_mut().filter(|_| !reopened) {
            if dirty.master {
                default.refresh_master()?;
            }
            if dirty.session_list {
                default.refresh_session_list()?;
            } else if dirty.session_values {
                default.refresh_session_values();
            }
        }
        Ok(())
    }

    fn enumerate_devices(&self) -> Result<Vec<Endpoint>, AudioError> {
        let devices = com::active_render_endpoints(&self.enumerator)
            .map_err(|e| com_error("enumerating render endpoints", e))?;
        let mut endpoints = Vec::with_capacity(devices.len());
        for device in devices {
            match read_endpoint(&device) {
                Ok(endpoint) => endpoints.push(endpoint),
                // Typically an endpoint removed during enumeration.
                Err(e) => tracing::debug!("skipping endpoint: {e}"),
            }
        }
        Ok(endpoints)
    }

    /// Follows the default endpoint, reopening its objects when it changed.
    /// Returns whether it was reopened, which also re-reads everything on it.
    fn update_default(&mut self) -> Result<bool, AudioError> {
        let device = com::default_render_endpoint(&self.enumerator)
            .map_err(|e| com_error("GetDefaultAudioEndpoint", e))?;
        let Some(device) = device else {
            self.default = None;
            self.reopen_default = false;
            return Ok(false);
        };
        let endpoint_id =
            com::endpoint_id(&device).map_err(|e| com_error("reading endpoint id", e))?;
        let unchanged = self
            .default
            .as_ref()
            .is_some_and(|d| d.endpoint_id == endpoint_id);
        if unchanged && !self.reopen_default {
            return Ok(false);
        }
        // Release the old endpoint's objects before opening the new one.
        self.default = None;
        self.default = Some(DefaultDevice::open(device, endpoint_id)?);
        self.reopen_default = false;
        Ok(true)
    }

    /// Replaces the shared state and emits the events describing the change.
    fn publish(&self) {
        let state = self.build_state();
        let events = {
            let mut current = self.shared.state();
            let events = logic::diff_states(&current, &state);
            *current = state;
            events
        };
        for event in events {
            // An error only means nobody is subscribed.
            let _ = self.shared.events.send(event);
        }
    }

    fn build_state(&self) -> AudioState {
        let devices = self.devices.iter().map(|e| e.info.clone()).collect();
        let default_device = self.default.as_ref().map(|d| {
            let name = self
                .devices
                .iter()
                .find(|e| e.endpoint_id == d.endpoint_id)
                .map_or_else(|| d.name.clone(), |e| e.info.name.clone());
            Device {
                id: d.device_id.clone(),
                name,
                volume: d.master.0,
                muted: d.master.1,
                sessions: d.sessions.iter().map(SessionEntry::to_session).collect(),
            }
        });
        AudioState {
            devices,
            default_device,
        }
    }
}

fn read_endpoint(device: &IMMDevice) -> Result<Endpoint, AudioError> {
    let endpoint_id = com::endpoint_id(device).map_err(|e| com_error("reading endpoint id", e))?;
    let id = logic::device_id_from_endpoint_id(&endpoint_id);
    let friendly = com::friendly_name(device).map_err(|e| com_error("reading endpoint name", e))?;
    let name = logic::device_name(friendly.as_deref(), &id);
    Ok(Endpoint {
        endpoint_id,
        info: DeviceInfo { id, name },
    })
}

/// The default endpoint and the objects that watch and control it.
struct DefaultDevice {
    endpoint_id: String,
    device_id: String,
    /// Used until the device list has caught up with a new default.
    name: String,
    /// Percent and mute.
    master: (f32, bool),
    sessions: Vec<SessionEntry>,
    volume: IAudioEndpointVolume,
    manager: IAudioSessionManager2,
    _device: IMMDevice,
}

impl DefaultDevice {
    fn open(device: IMMDevice, endpoint_id: String) -> Result<Self, AudioError> {
        let Endpoint { info, .. } = read_endpoint(&device)?;
        let volume: IAudioEndpointVolume =
            com::activate(&device).map_err(|e| com_error("activating IAudioEndpointVolume", e))?;
        let manager: IAudioSessionManager2 =
            com::activate(&device).map_err(|e| com_error("activating IAudioSessionManager2", e))?;
        let master = com::master(&volume).map_err(|e| com_error("reading master volume", e))?;
        let mut default = Self {
            endpoint_id,
            device_id: info.id,
            name: info.name,
            master,
            sessions: Vec::new(),
            volume,
            manager,
            _device: device,
        };
        default.refresh_session_list()?;
        Ok(default)
    }

    fn refresh_master(&mut self) -> Result<(), AudioError> {
        self.master =
            com::master(&self.volume).map_err(|e| com_error("reading master volume", e))?;
        Ok(())
    }

    /// Re-enumerates sessions, keeping known ones in place and opening new
    /// ones, then re-reads every session's values.
    fn refresh_session_list(&mut self) -> Result<(), AudioError> {
        let controls = com::session_controls(&self.manager)
            .map_err(|e| com_error("enumerating audio sessions", e))?;
        let mut listed = Vec::new();
        for control in controls {
            match com::is_listed(&control).and_then(|listed| {
                listed
                    .then(|| com::session_instance_id(&control))
                    .transpose()
            }) {
                Ok(Some(id)) => listed.push((id, control)),
                Ok(None) => {}
                Err(e) => tracing::debug!("skipping audio session: {e}"),
            }
        }
        let ids: Vec<String> = listed.iter().map(|(id, _)| id.clone()).collect();
        let (kept, added) = logic::reconcile(std::mem::take(&mut self.sessions), |s| &s.id, &ids);
        self.sessions = kept;
        self.refresh_session_values();
        for id in added {
            let Some((_, control)) = listed.iter().find(|(listed_id, _)| *listed_id == id) else {
                continue;
            };
            if !id.contains(&self.device_id) {
                tracing::warn!(
                    session = %id,
                    device = %self.device_id,
                    "session id does not contain its device id"
                );
            }
            match SessionEntry::open(id, control.clone()) {
                Ok(entry) => self.sessions.push(entry),
                Err(e) => tracing::debug!("skipping audio session: {e}"),
            }
        }
        Ok(())
    }

    /// Re-reads every session's name, volume and mute, dropping sessions that
    /// can no longer be read.
    fn refresh_session_values(&mut self) {
        self.sessions.retain_mut(|s| match s.refresh() {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(session = %s.id, "dropping unreadable audio session: {e}");
                false
            }
        });
    }
}

/// A listed session on the default endpoint.
struct SessionEntry {
    id: String,
    control: IAudioSessionControl2,
    simple: ISimpleAudioVolume,
    pid: u32,
    /// `GetDisplayName` as last read, and what it resolved to.
    raw_display: Option<String>,
    display: Option<String>,
    /// Looked up the first time the session has no display name.
    process: Option<Option<String>>,
    volume: f32,
    muted: bool,
}

impl SessionEntry {
    fn open(id: String, control: IAudioSessionControl2) -> Result<Self, AudioError> {
        let simple: ISimpleAudioVolume = control
            .cast()
            .map_err(|e| com_error("querying ISimpleAudioVolume", e))?;
        // Multi-process sessions have no single pid; they fall back to their
        // display name or "Unknown".
        let pid = com::process_id(&control).unwrap_or(0);
        let mut entry = Self {
            id,
            control,
            simple,
            pid,
            raw_display: None,
            display: None,
            process: None,
            volume: 0.0,
            muted: false,
        };
        entry.refresh()?;
        Ok(entry)
    }

    fn refresh(&mut self) -> Result<(), AudioError> {
        let raw = com::display_name(&self.control)
            .map_err(|e| com_error("reading session display name", e))?;
        if self.raw_display.as_deref() != Some(raw.as_str()) {
            self.display = logic::resolve_display_name(&raw, com::load_indirect_string);
            self.raw_display = Some(raw);
        }
        if self.display.is_none() && self.process.is_none() {
            let pid = self.pid;
            self.process = Some((pid != 0).then(|| com::process_image_stem(pid)).flatten());
        }
        (self.volume, self.muted) = com::session_volume(&self.simple)
            .map_err(|e| com_error("reading session volume", e))?;
        Ok(())
    }

    fn to_session(&self) -> Session {
        Session {
            id: self.id.clone(),
            name: logic::session_name(
                self.display.as_deref(),
                self.process.as_ref().and_then(Option::as_deref),
            ),
            volume: self.volume,
            muted: self.muted,
        }
    }
}
