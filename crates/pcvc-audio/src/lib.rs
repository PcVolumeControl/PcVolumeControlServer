//! Audio backend abstraction for the PcVolumeControl host.
//!
//! An [`AudioBackend`] exposes the default playback device, its sessions and
//! the list of active playback devices, accepts the three kinds of change the
//! protocol supports, and notifies listeners through [`AudioEvent`]s.
//! [`MockBackend`] is always available; platform backends (Windows Core Audio,
//! PulseAudio) live in `cfg`-gated modules of this crate.

mod mock;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(any(target_os = "windows", test))]
mod windows;

#[cfg(target_os = "windows")]
pub use self::windows::WindowsBackend;
pub use mock::MockBackend;
use std::sync::Arc;

use pcvc_protocol::Command;
#[cfg(target_os = "linux")]
pub use pulse::PulseBackend;
use tokio::sync::broadcast;

/// Snapshot of the audio system as the protocol presents it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioState {
    /// Active playback devices.
    pub devices: Vec<DeviceInfo>,
    /// The default playback device, if there is one.
    pub default_device: Option<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// A playback device with its master channel and sessions.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Percent, 0.0 to 100.0.
    pub volume: f32,
    pub muted: bool,
    pub sessions: Vec<Session>,
}

impl Device {
    /// A device with no sessions, unmuted.
    pub fn new(id: impl Into<String>, name: impl Into<String>, volume: f32) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            volume,
            muted: false,
            sessions: Vec::new(),
        }
    }

    pub fn with_session(mut self, session: Session) -> Self {
        self.sessions.push(session);
        self
    }
}

/// A per-application audio stream on a device.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// Opaque to clients.
    pub id: String,
    pub name: String,
    /// Percent, 0.0 to 100.0.
    pub volume: f32,
    pub muted: bool,
    /// Tells this session apart from others of the same application, such as
    /// a browser tab's title. The server sends it as the protocol's
    /// `detail`, after [`pcvc_protocol::session_detail`].
    pub detail: Option<String>,
    /// Who is playing, for presenting the session locally (icons). Not part
    /// of the protocol.
    pub app: AppHint,
}

impl Session {
    /// An unmuted session with no detail and no application hints.
    pub fn new(id: impl Into<String>, name: impl Into<String>, volume: f32) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            volume,
            muted: false,
            detail: None,
            app: AppHint::default(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The event for a change from `old` to `self`, if there is one.
    pub(crate) fn change_from(&self, old: &Session) -> Option<AudioEvent> {
        if self == old {
            return None;
        }
        let id = self.id.clone();
        let without_detail = |s: &Session| Session {
            detail: None,
            ..s.clone()
        };
        Some(if without_detail(self) == without_detail(old) {
            AudioEvent::SessionDetailChanged { id }
        } else {
            AudioEvent::SessionChanged { id }
        })
    }

    pub fn with_app(mut self, app: AppHint) -> Self {
        self.app = app;
        self
    }
}

/// What the platform says about the application behind a session. Every
/// field is optional; values are shared, so cloning is cheap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AppHint {
    /// An icon the application declared: a freedesktop icon name or an
    /// absolute file path (Linux `application.icon_name`).
    pub icon_name: Option<Arc<str>>,
    /// The executable. Linux: `application.process.binary`, usually a bare
    /// file name. Windows: the full image path.
    pub process_binary: Option<Arc<str>>,
    pub process_id: Option<u32>,
    /// A desktop application id (Linux `application.id`, or the Flatpak app
    /// id of a sandboxed client).
    pub app_id: Option<Arc<str>>,
}

impl AppHint {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A change notification. Events are hints: consumers re-read the state with
/// [`AudioBackend::snapshot`] rather than patching a cached copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    DevicesChanged,
    DefaultDeviceChanged,
    MasterChanged,
    SessionAdded {
        id: String,
    },
    SessionRemoved {
        id: String,
    },
    SessionChanged {
        id: String,
    },
    /// Only the session's [`Session::detail`] changed. Consumers that do not
    /// show details can ignore it.
    SessionDetailChanged {
        id: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("unknown device: {0}")]
    UnknownDevice(String),
    #[error("unknown session: {0}")]
    UnknownSession(String),
    #[error("no default playback device")]
    NoDefaultDevice,
    #[error("audio backend error: {0}")]
    Backend(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// The audio system behind the server.
///
/// Methods are synchronous and may block briefly; async callers should run
/// them on a blocking-capable thread. Implementations must:
///
/// - list only active playback devices and exclude system-sound sessions;
/// - report and accept volumes as percentages, clamping inputs to 0..=100;
/// - emit at least one [`AudioEvent`] after every change to the state, whether
///   it came from these methods or from elsewhere in the system. Calls that
///   change nothing need not emit.
pub trait AudioBackend: Send + Sync {
    fn snapshot(&self) -> Result<AudioState, AudioError>;

    /// Makes `device_id` the default playback (and, where the platform has the
    /// concept, communications) device.
    fn set_default_device(&self, device_id: &str) -> Result<(), AudioError>;

    /// Sets master volume and/or mute on the default device.
    fn set_master(&self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError>;

    /// Sets volume and mute of a session on the default device.
    fn set_session(&self, session_id: &str, volume: f32, muted: bool) -> Result<(), AudioError>;

    /// A new receiver for change events. A lagged receiver should treat the
    /// lag as "something changed".
    fn subscribe(&self) -> broadcast::Receiver<AudioEvent>;

    /// Runs a protocol command through the matching setter.
    fn apply(&self, command: &Command) -> Result<(), AudioError> {
        match command {
            Command::SetDefaultDevice { device_id } => self.set_default_device(device_id),
            Command::SetMaster { volume, muted } => self.set_master(*volume, *muted),
            Command::SetSession {
                session_id,
                volume,
                muted,
            } => self.set_session(session_id, *volume, *muted),
        }
    }
}
