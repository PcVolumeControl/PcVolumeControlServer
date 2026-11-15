//! Audio backend abstraction for the PcVolumeControl host.
//!
//! An [`AudioBackend`] exposes the default playback device, its sessions and
//! the list of active playback devices, accepts the three kinds of change the
//! protocol supports, and notifies listeners through [`AudioEvent`]s.
//! [`MockBackend`] is always available; platform backends (Windows Core Audio,
//! PulseAudio) will live in `cfg`-gated modules of this crate.

mod mock;

pub use mock::{MockBackend, MockDevice};
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

/// A per-application audio stream on a device.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// Opaque, but must contain the owning device's id as a substring (the iOS
    /// client derives the device id from it).
    pub id: String,
    pub name: String,
    /// Percent, 0.0 to 100.0.
    pub volume: f32,
    pub muted: bool,
}

/// A change notification. Events are hints: consumers re-read the state with
/// [`AudioBackend::snapshot`] rather than patching a cached copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    DevicesChanged,
    DefaultDeviceChanged,
    MasterChanged,
    SessionAdded { id: String },
    SessionRemoved { id: String },
    SessionChanged { id: String },
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
///   it came from these methods or from elsewhere in the system.
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
}
