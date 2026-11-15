//! The COM thread: owns every Core Audio object, keeps the cached
//! [`AudioState`] current and executes commands.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ::windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback,
};
use ::windows::Win32::Media::Audio::{
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents, IAudioSessionManager2,
    IAudioSessionNotification, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    ISimpleAudioVolume,
};
use ::windows::core::{GUID, Interface};
use tokio::sync::broadcast;

use super::callbacks::{self, MtaSession, Poster, Registration};
use super::com::{self, Apartment, OpenedProcess};
use super::logic::{self, Change, Dirty, SessionAction, SessionSource};
use super::policy_config;
use crate::{AppHint, AudioError, AudioEvent, AudioState, Device, DeviceInfo, Session};

const EVENT_CAPACITY: usize = 64;

/// Most messages handled before the cache is refreshed and replies are sent,
/// which bounds how long a command waits behind others.
const MAX_BATCH: usize = 64;

/// How often sessions are checked for an exited process.
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_secs(2);

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
    Changed(Change),
    /// A session reported by `IAudioSessionNotification`, which the session
    /// enumerator may not list yet.
    SessionCreated {
        endpoint_id: String,
        session: MtaSession,
    },
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
    // Queued messages can hold COM objects, so the receiver is dropped before
    // the apartment on every path.
    let rx = rx;
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
    drop(rx);
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
    poster: Poster,
    _endpoint_notifications: Registration<IMMDeviceEnumerator, IMMNotificationClient>,
    enumerator: IMMDeviceEnumerator,
    devices: Vec<Endpoint>,
    default: Option<DefaultDevice>,
    dirty: Dirty,
    /// Rebuild the default device's objects even if its id is unchanged,
    /// after one of them failed.
    reopen_default: bool,
    /// Consecutive failed refreshes.
    failures: u32,
    retry_at: Option<Instant>,
    last_process_check: Instant,
}

impl Worker {
    fn new(shared: Arc<Shared>, tx: Sender<Msg>) -> Result<Self, AudioError> {
        let enumerator =
            com::create_enumerator().map_err(|e| com_error("creating IMMDeviceEnumerator", e))?;
        let context = GUID::new().map_err(|e| com_error("creating an event context", e))?;
        let poster = Poster::new(tx, context);
        // Registered before the first read so that no change is missed.
        let endpoint_notifications =
            callbacks::register_endpoint_notifications(&enumerator, poster.clone())
                .map_err(|e| com_error("registering IMMNotificationClient", e))?;
        let mut worker = Self {
            shared,
            poster,
            _endpoint_notifications: endpoint_notifications,
            enumerator,
            devices: Vec::new(),
            default: None,
            dirty: Dirty::default(),
            reopen_default: false,
            failures: 0,
            retry_at: None,
            last_process_check: Instant::now(),
        };
        worker.refresh(Dirty::all())?;
        worker.publish();
        Ok(worker)
    }

    /// When to next look for sessions whose process has exited, which Core
    /// Audio does not report: an app that stops playing and exits leaves an
    /// inactive session behind.
    fn process_check_at(&self) -> Option<Instant> {
        let watching = self
            .default
            .as_ref()
            .is_some_and(|d| d.sessions.iter().any(SessionEntry::can_detect_exit));
        watching.then(|| self.last_process_check + PROCESS_CHECK_INTERVAL)
    }

    fn serve(&mut self, rx: &Receiver<Msg>) {
        let mut replies = Vec::new();
        loop {
            let deadline = self
                .retry_at
                .into_iter()
                .chain(self.process_check_at())
                .min();
            let first = match deadline {
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
                    Msg::SessionCreated {
                        endpoint_id,
                        session,
                    } => {
                        // A session of a previous default device is dropped.
                        if let Some(default) = self
                            .default
                            .as_mut()
                            .filter(|d| d.endpoint_id == endpoint_id)
                        {
                            default.created.push(session.into_inner());
                            self.dirty.mark(Change::SessionList);
                        }
                    }
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
            if self
                .process_check_at()
                .is_some_and(|at| at <= Instant::now())
            {
                self.last_process_check = Instant::now();
                if self
                    .default
                    .as_mut()
                    .is_some_and(DefaultDevice::drop_exited)
                {
                    self.publish();
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

    /// Runs a command against the cached objects, re-reading once if the
    /// target is not found, and marks what it changes so that the flush after
    /// the batch re-reads it even if Core Audio's notification is late.
    fn execute(&mut self, request: Request) -> Result<(), AudioError> {
        match request {
            Request::DefaultDevice { device_id } => {
                let endpoint_id = self
                    .find_or_refresh(Change::Devices, |w| w.endpoint_id_of(&device_id))
                    .ok_or(AudioError::UnknownDevice(device_id))?;
                self.dirty.mark(Change::Default);
                policy_config::set_default_endpoint(&endpoint_id)
                    .map_err(|e| com_error("IPolicyConfig::SetDefaultEndpoint", e))
            }
            Request::Master { volume, muted } => {
                let control = self
                    .find_or_refresh(Change::Default, |w| {
                        w.default.as_ref().map(|d| d.volume.clone())
                    })
                    .ok_or(AudioError::NoDefaultDevice)?;
                self.dirty.mark(Change::Master);
                com::set_master(&control, volume, muted, self.poster.context())
                    .map_err(|e| com_error("setting master volume", e))
            }
            Request::Session {
                session_id,
                volume,
                muted,
            } => {
                let control = self.find_or_refresh(Change::SessionList, |w| {
                    let default = w.default.as_ref()?;
                    let session = default.sessions.iter().find(|s| s.id == session_id)?;
                    Some(session.simple.clone())
                });
                let control = match (control, &self.default) {
                    (Some(control), _) => control,
                    (None, Some(_)) => return Err(AudioError::UnknownSession(session_id)),
                    (None, None) => return Err(AudioError::NoDefaultDevice),
                };
                self.dirty.mark(Change::SessionValues);
                com::set_session_volume(&control, volume, muted, self.poster.context())
                    .map_err(|e| com_error("setting session volume", e))
            }
        }
    }

    /// `find` on the cached state or, if that finds nothing, on the state
    /// after re-reading `change`.
    fn find_or_refresh<T>(
        &mut self,
        change: Change,
        find: impl Fn(&Self) -> Option<T>,
    ) -> Option<T> {
        if let Some(found) = find(self) {
            return Some(found);
        }
        self.dirty.mark(change);
        self.flush();
        find(self)
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
            Ok(()) => {
                if self.failures > 0 {
                    tracing::info!("audio state readable again");
                }
                self.failures = 0;
                self.retry_at = None;
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                let delay = logic::retry_delay(self.failures);
                if self.failures == 1 {
                    tracing::warn!("refreshing audio state failed, retrying: {e}");
                } else {
                    tracing::debug!(
                        failures = self.failures,
                        ?delay,
                        "refresh failed again: {e}"
                    );
                }
                self.dirty.merge(dirty);
                self.dirty.mark(Change::Default);
                self.reopen_default = true;
                self.retry_at = Some(Instant::now() + delay);
            }
        }
        self.publish();
    }

    fn refresh(&mut self, dirty: Dirty) -> Result<(), AudioError> {
        if dirty.devices {
            self.devices = self.enumerate_devices()?;
        }
        let mut reopened = if dirty.devices || dirty.default || self.reopen_default {
            self.update_default()?
        } else {
            false
        };
        // A device change can invalidate the default endpoint's objects while
        // its id stays the same (disabled and re-enabled, driver restart).
        // Reading through them tells.
        if dirty.devices && !reopened {
            let invalid = self
                .default
                .as_mut()
                .is_some_and(|d| d.refresh_master().is_err());
            if invalid {
                self.reopen_default = true;
                reopened = self.update_default()?;
            }
        }
        let poster = &self.poster;
        if let Some(default) = self.default.as_mut().filter(|_| !reopened) {
            if dirty.master {
                default.refresh_master()?;
            }
            if dirty.session_list {
                default.refresh_session_list(poster)?;
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
        let same = self
            .default
            .as_ref()
            .filter(|d| d.endpoint_id == endpoint_id);
        if same.is_some() && !self.reopen_default {
            return Ok(false);
        }
        // Reopening the same endpoint keeps the session order and the
        // sessions reported as created but not yet looked at.
        let (order, created) = same
            .map(|d| {
                let order: Vec<String> = d.sessions.iter().map(|s| s.id.clone()).collect();
                (order, d.created.clone())
            })
            .unwrap_or_default();
        // On failure the old default stays cached until a retry succeeds,
        // rather than briefly publishing no default device.
        self.default = Some(DefaultDevice::open(
            device,
            endpoint_id,
            &self.poster,
            &order,
            created,
        )?);
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
    Ok(endpoint_info(device, endpoint_id))
}

/// An endpoint's protocol id and name. An unreadable friendly name falls back
/// to the id rather than hiding the endpoint.
fn endpoint_info(device: &IMMDevice, endpoint_id: String) -> Endpoint {
    let id = logic::device_id_from_endpoint_id(&endpoint_id);
    let friendly = com::friendly_name(device).unwrap_or_else(|e| {
        tracing::debug!(endpoint = %endpoint_id, "reading endpoint name failed: {e}");
        None
    });
    let name = logic::device_name(friendly.as_deref(), &id);
    Endpoint {
        endpoint_id,
        info: DeviceInfo { id, name },
    }
}

/// Keeps `result`'s value, or logs that the callback could not be registered.
/// A device or session without its callbacks is still listed and controllable,
/// it just updates only when something else triggers a refresh.
fn optional<T>(what: &str, result: ::windows::core::Result<T>) -> Option<T> {
    result
        .inspect_err(|e| tracing::warn!("registering {what} failed: {e}"))
        .ok()
}

/// The default endpoint and the objects that watch and control it.
struct DefaultDevice {
    endpoint_id: String,
    device_id: String,
    /// Used until the device list has caught up with a new default.
    name: String,
    /// Percent and mute.
    master: (f32, bool),
    /// Listed sessions in the order the phones show them. A session leaves
    /// the list when it expires, is disconnected, its process exits or it
    /// can no longer be read, not when the session enumerator stops
    /// returning it.
    sessions: Vec<SessionEntry>,
    /// Sessions from `OnSessionCreated` not yet looked at.
    created: Vec<IAudioSessionControl>,
    /// Ids of sessions dropped because their process exited. The enumerator
    /// keeps listing them as inactive.
    exited: HashSet<String>,
    _volume_notifications: Option<Registration<IAudioEndpointVolume, IAudioEndpointVolumeCallback>>,
    _session_notifications: Option<Registration<IAudioSessionManager2, IAudioSessionNotification>>,
    volume: IAudioEndpointVolume,
    manager: IAudioSessionManager2,
    _device: IMMDevice,
}

impl DefaultDevice {
    /// Opens the endpoint's controls, registers its callbacks and reads it.
    /// Sessions whose ids are in `order` come first, in that order; `created`
    /// are sessions reported as created that the enumerator may not list.
    fn open(
        device: IMMDevice,
        endpoint_id: String,
        poster: &Poster,
        order: &[String],
        created: Vec<IAudioSessionControl>,
    ) -> Result<Self, AudioError> {
        let Endpoint { endpoint_id, info } = endpoint_info(&device, endpoint_id);
        let volume: IAudioEndpointVolume =
            com::activate(&device).map_err(|e| com_error("activating IAudioEndpointVolume", e))?;
        let manager: IAudioSessionManager2 =
            com::activate(&device).map_err(|e| com_error("activating IAudioSessionManager2", e))?;
        let volume_notifications = optional(
            "IAudioEndpointVolumeCallback",
            callbacks::register_volume_notifications(&volume, poster.clone()),
        );
        // Core Audio only delivers session-created notifications to a manager
        // whose session enumerator has been requested.
        com::request_session_enumerator(&manager)
            .map_err(|e| com_error("enumerating audio sessions", e))?;
        let session_notifications = optional(
            "IAudioSessionNotification",
            callbacks::register_session_created(&manager, endpoint_id.clone(), poster.clone()),
        );
        let master = com::master(&volume).map_err(|e| com_error("reading master volume", e))?;
        let mut default = Self {
            endpoint_id,
            device_id: info.id,
            name: info.name,
            master,
            sessions: Vec::new(),
            created,
            exited: HashSet::new(),
            _volume_notifications: volume_notifications,
            _session_notifications: session_notifications,
            volume,
            manager,
            _device: device,
        };
        default.refresh_session_list(poster)?;
        logic::sort_like(&mut default.sessions, |s| &s.id, order);
        Ok(default)
    }

    fn refresh_master(&mut self) -> Result<(), AudioError> {
        self.master =
            com::master(&self.volume).map_err(|e| com_error("reading master volume", e))?;
        Ok(())
    }

    /// Drops sessions whose process has exited. Returns whether any was.
    fn drop_exited(&mut self) -> bool {
        let before = self.sessions.len();
        let exited = &mut self.exited;
        self.sessions.retain(|s| {
            let gone = s.has_exited();
            if gone {
                exited.insert(s.id.clone());
            }
            !gone
        });
        self.sessions.len() != before
    }

    /// Drops sessions that are gone and re-reads the others, then takes in
    /// sessions reported as created and sessions the enumerator lists that
    /// are not known yet.
    fn refresh_session_list(&mut self, poster: &Poster) -> Result<(), AudioError> {
        let enumerated = com::session_controls(&self.manager)
            .map_err(|e| com_error("enumerating audio sessions", e))?;
        self.refresh_session_values();

        for created in std::mem::take(&mut self.created) {
            let found = created
                .cast::<IAudioSessionControl2>()
                .and_then(|control| Ok((com::session_instance_id(&control)?, control)));
            match found {
                Ok((id, control)) => {
                    let same_control = self
                        .sessions
                        .iter()
                        .any(|s| s.id == id && com::same_object(&s.control, &control));
                    let source = SessionSource::Created { same_control };
                    self.take_session(id, control, source, poster);
                }
                Err(e) => tracing::debug!("skipping created audio session: {e}"),
            }
        }

        for control in enumerated {
            let id = match com::session_instance_id(&control) {
                Ok(id) => id,
                Err(e) => {
                    tracing::debug!("skipping audio session: {e}");
                    continue;
                }
            };
            let exited_before = self.exited.contains(&id);
            // Only asked when it matters, to spare a cross-process call.
            let active = exited_before && com::is_active(&control).unwrap_or(false);
            let source = SessionSource::Enumerated {
                exited_before,
                active,
            };
            self.take_session(id, control, source, poster);
        }
        Ok(())
    }

    /// Adds, replaces or skips a session found during a session-list refresh.
    fn take_session(
        &mut self,
        id: String,
        control: IAudioSessionControl2,
        source: SessionSource,
        poster: &Poster,
    ) {
        let known = self.sessions.iter().position(|s| s.id == id);
        let action = logic::session_action(known.is_some(), source);
        if action == SessionAction::Skip {
            return;
        }
        let entry = match SessionEntry::open(id, control, poster) {
            Ok(Some(entry)) => entry,
            Ok(None) => return,
            Err(e) => {
                tracing::debug!("skipping audio session: {e}");
                return;
            }
        };
        if !entry.id.contains(&self.device_id) {
            tracing::warn!(
                session = %entry.id,
                device = %self.device_id,
                "session id does not contain its device id"
            );
        }
        self.exited.remove(&entry.id);
        match known {
            Some(i) => self.sessions[i] = entry,
            None => self.sessions.push(entry),
        }
    }

    /// Re-reads every session's name, volume and mute, dropping sessions that
    /// are gone or can no longer be read.
    fn refresh_session_values(&mut self) {
        self.drop_exited();
        self.sessions.retain_mut(|s| match s.refresh() {
            Ok(listed) => listed,
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
    /// The owning process, if the session has exactly one and it could be
    /// opened.
    process: Option<com::Process>,
    pid: Option<u32>,
    /// The owning process's image path, read once when the session opens.
    image_path: Option<Arc<str>>,
    /// Set by the session's callbacks when it expires or is disconnected.
    gone: Arc<AtomicBool>,
    /// `GetDisplayName` as last read, and what it resolved to.
    raw_display: Option<String>,
    display: Option<String>,
    /// The image file stem, the name used when there is no display name.
    process_name: Option<String>,
    volume: f32,
    muted: bool,
    _events: Option<Registration<IAudioSessionControl, IAudioSessionEvents>>,
}

impl SessionEntry {
    /// Registers for the session's events, then reads it. `None` if it is not
    /// listed: the system-sounds session, expired, or its process has exited.
    fn open(
        id: String,
        control: IAudioSessionControl2,
        poster: &Poster,
    ) -> Result<Option<Self>, AudioError> {
        if !com::is_listed(&control).map_err(|e| com_error("reading session state", e))? {
            return Ok(None);
        }
        let simple: ISimpleAudioVolume = control
            .cast()
            .map_err(|e| com_error("querying ISimpleAudioVolume", e))?;
        let gone = Arc::new(AtomicBool::new(false));
        let events = optional(
            "IAudioSessionEvents",
            callbacks::register_session_events(&control, Arc::clone(&gone), poster.clone()),
        );
        // Multi-process sessions report no single pid.
        let pid = com::process_id(&control).ok().filter(|&pid| pid != 0);
        let process = match pid.map(com::Process::open) {
            Some(OpenedProcess::Open(process)) => Some(process),
            // An exited app's session lingers as inactive.
            Some(OpenedProcess::NoSuchProcess) => return Ok(None),
            Some(OpenedProcess::Denied) | None => None,
        };
        let image_path = process.as_ref().and_then(com::Process::image_path);
        let mut entry = Self {
            id,
            control,
            simple,
            process,
            pid,
            process_name: image_path.as_deref().and_then(logic::process_stem),
            image_path: image_path.map(Arc::from),
            gone,
            raw_display: None,
            display: None,
            volume: 0.0,
            muted: false,
            _events: events,
        };
        Ok(entry.refresh()?.then_some(entry))
    }

    fn has_exited(&self) -> bool {
        self.process.as_ref().is_some_and(com::Process::has_exited)
    }

    fn can_detect_exit(&self) -> bool {
        self.process.as_ref().is_some_and(com::Process::can_wait)
    }

    /// Re-reads the session. Returns false if it is no longer listed.
    fn refresh(&mut self) -> Result<bool, AudioError> {
        if self.gone.load(Ordering::Acquire) {
            return Ok(false);
        }
        if !com::is_listed(&self.control).map_err(|e| com_error("reading session state", e))? {
            return Ok(false);
        }
        let raw = com::display_name(&self.control)
            .map_err(|e| com_error("reading session display name", e))?;
        if self.raw_display.as_deref() != Some(raw.as_str()) {
            self.display = logic::resolve_display_name(&raw, com::load_indirect_string);
            self.raw_display = Some(raw);
        }
        (self.volume, self.muted) = com::session_volume(&self.simple)
            .map_err(|e| com_error("reading session volume", e))?;
        Ok(true)
    }

    fn to_session(&self) -> Session {
        Session {
            id: self.id.clone(),
            name: logic::session_name(self.display.as_deref(), self.process_name.as_deref()),
            volume: self.volume,
            muted: self.muted,
            // Core Audio has nothing per session beyond the display name,
            // which is already the name.
            detail: None,
            app: AppHint {
                process_binary: self.image_path.clone(),
                process_id: self.pid,
                ..AppHint::default()
            },
        }
    }
}
