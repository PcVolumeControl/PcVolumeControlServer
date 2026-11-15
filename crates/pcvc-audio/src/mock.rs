use std::sync::{Mutex, MutexGuard, PoisonError};

use pcvc_protocol::clamp_volume;
use tokio::sync::broadcast;

use crate::{AudioBackend, AudioError, AudioEvent, AudioState, Device, DeviceInfo, Session};

const EVENT_CAPACITY: usize = 64;

/// A device as the mock holds it: every device keeps its own sessions, and the
/// default device's sessions are the ones reported.
#[derive(Debug, Clone, PartialEq)]
pub struct MockDevice {
    pub id: String,
    pub name: String,
    pub volume: f32,
    pub muted: bool,
    pub sessions: Vec<Session>,
}

impl MockDevice {
    pub fn new(id: impl Into<String>, name: impl Into<String>, volume: f32) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            volume: clamp_volume(volume),
            muted: false,
            sessions: Vec::new(),
        }
    }

    /// Adds a session whose id is derived from this device's id and `app`.
    pub fn with_session(mut self, app: &str, name: impl Into<String>, volume: f32) -> Self {
        self.sessions.push(Session {
            id: MockBackend::session_id(&self.id, app),
            name: name.into(),
            volume: clamp_volume(volume),
            muted: false,
        });
        self
    }
}

#[derive(Debug)]
struct MockState {
    devices: Vec<MockDevice>,
    default_id: Option<String>,
}

impl MockState {
    fn default_device_mut(&mut self) -> Result<&mut MockDevice, AudioError> {
        let id = self
            .default_id
            .as_deref()
            .ok_or(AudioError::NoDefaultDevice)?;
        self.devices
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or(AudioError::NoDefaultDevice)
    }
}

/// In-memory [`AudioBackend`] with a scripted device and session list.
///
/// Commands mutate its state and emit events like a real backend would. The
/// scripting methods (`add_device`, `remove_session`, ...) simulate changes
/// made outside the protocol, such as an application starting playback.
#[derive(Debug)]
pub struct MockBackend {
    state: Mutex<MockState>,
    events: broadcast::Sender<AudioEvent>,
}

impl MockBackend {
    /// Creates a mock with the given devices. Fails if `default_device_id` is
    /// not one of them.
    pub fn new(
        devices: Vec<MockDevice>,
        default_device_id: Option<&str>,
    ) -> Result<Self, AudioError> {
        if let Some(id) = default_device_id
            && !devices.iter().any(|d| d.id == id)
        {
            return Err(AudioError::UnknownDevice(id.to_owned()));
        }
        Ok(Self {
            state: Mutex::new(MockState {
                devices,
                default_id: default_device_id.map(str::to_owned),
            }),
            events: broadcast::channel(EVENT_CAPACITY).0,
        })
    }

    /// Three devices with a handful of sessions, including a non-ASCII name.
    pub fn demo() -> Self {
        const SPEAKERS: &str = "0f4090a9-dee2-4563-ba29-0ad6b93d9e22";
        const HEADPHONES: &str = "c5a32106-264d-40b2-a2e0-74eda397454c";
        const HEADSET: &str = "c98e4030-926b-4e62-8c83-1c529574cc51";
        let devices = vec![
            MockDevice::new(SPEAKERS, "Speakers (Mock Audio)", 80.0)
                .with_session("firefox", "Firefox", 100.0)
                .with_session("spotify", "Spotify", 65.0)
                .with_session("discord", "Discord", 40.0)
                .with_session("musique", "Musique \u{e9}t\u{e9}", 55.0),
            MockDevice::new(HEADPHONES, "Headphones (Mock Audio)", 50.0)
                .with_session("steam", "Steam", 83.0),
            MockDevice::new(HEADSET, "Headset (Mock USB Audio)", 70.0)
                .with_session("zoom", "Zoom", 90.0),
        ];
        Self::new(devices, Some(SPEAKERS)).expect("demo default device exists")
    }

    /// The session id the mock uses for `app` on `device_id`. It embeds the
    /// device id, as the protocol requires.
    pub fn session_id(device_id: &str, app: &str) -> String {
        format!("{device_id}|mock/{app}")
    }

    /// Adds a device. If there is no default device it becomes the default.
    pub fn add_device(&self, device: MockDevice) {
        let became_default = {
            let mut state = self.lock();
            let became_default = state.default_id.is_none();
            if became_default {
                state.default_id = Some(device.id.clone());
            }
            state.devices.retain(|d| d.id != device.id);
            state.devices.push(device);
            became_default
        };
        self.emit(AudioEvent::DevicesChanged);
        if became_default {
            self.emit(AudioEvent::DefaultDeviceChanged);
        }
    }

    /// Removes a device. Removing the default device makes the first remaining
    /// device the default, or leaves no default.
    pub fn remove_device(&self, device_id: &str) -> Result<(), AudioError> {
        let default_changed = {
            let mut state = self.lock();
            let before = state.devices.len();
            state.devices.retain(|d| d.id != device_id);
            if state.devices.len() == before {
                return Err(AudioError::UnknownDevice(device_id.to_owned()));
            }
            let default_changed = state.default_id.as_deref() == Some(device_id);
            if default_changed {
                state.default_id = state.devices.first().map(|d| d.id.clone());
            }
            default_changed
        };
        self.emit(AudioEvent::DevicesChanged);
        if default_changed {
            self.emit(AudioEvent::DefaultDeviceChanged);
        }
        Ok(())
    }

    /// Adds a session to a device, replacing any session with the same id.
    pub fn add_session(&self, device_id: &str, mut session: Session) -> Result<(), AudioError> {
        session.volume = clamp_volume(session.volume);
        let id = session.id.clone();
        {
            let mut state = self.lock();
            let device = state
                .devices
                .iter_mut()
                .find(|d| d.id == device_id)
                .ok_or_else(|| AudioError::UnknownDevice(device_id.to_owned()))?;
            device.sessions.retain(|s| s.id != session.id);
            device.sessions.push(session);
        }
        self.emit(AudioEvent::SessionAdded { id });
        Ok(())
    }

    /// Removes a session from whichever device holds it.
    pub fn remove_session(&self, session_id: &str) -> Result<(), AudioError> {
        let removed = self.lock().devices.iter_mut().any(|d| {
            let before = d.sessions.len();
            d.sessions.retain(|s| s.id != session_id);
            d.sessions.len() != before
        });
        if !removed {
            return Err(AudioError::UnknownSession(session_id.to_owned()));
        }
        self.emit(AudioEvent::SessionRemoved {
            id: session_id.to_owned(),
        });
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: AudioEvent) {
        // An error only means nobody is subscribed.
        let _ = self.events.send(event);
    }
}

impl AudioBackend for MockBackend {
    fn snapshot(&self) -> Result<AudioState, AudioError> {
        let state = self.lock();
        let devices = state
            .devices
            .iter()
            .map(|d| DeviceInfo {
                id: d.id.clone(),
                name: d.name.clone(),
            })
            .collect();
        let default_device = state
            .default_id
            .as_deref()
            .and_then(|id| state.devices.iter().find(|d| d.id == id))
            .map(|d| Device {
                id: d.id.clone(),
                name: d.name.clone(),
                volume: d.volume,
                muted: d.muted,
                sessions: d.sessions.clone(),
            });
        Ok(AudioState {
            devices,
            default_device,
        })
    }

    fn set_default_device(&self, device_id: &str) -> Result<(), AudioError> {
        {
            let mut state = self.lock();
            if !state.devices.iter().any(|d| d.id == device_id) {
                return Err(AudioError::UnknownDevice(device_id.to_owned()));
            }
            state.default_id = Some(device_id.to_owned());
        }
        self.emit(AudioEvent::DefaultDeviceChanged);
        Ok(())
    }

    fn set_master(&self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError> {
        {
            let mut state = self.lock();
            let device = state.default_device_mut()?;
            if let Some(muted) = muted {
                device.muted = muted;
            }
            if let Some(volume) = volume {
                device.volume = clamp_volume(volume);
            }
        }
        self.emit(AudioEvent::MasterChanged);
        Ok(())
    }

    fn set_session(&self, session_id: &str, volume: f32, muted: bool) -> Result<(), AudioError> {
        {
            let mut state = self.lock();
            let session = state
                .default_device_mut()?
                .sessions
                .iter_mut()
                .find(|s| s.id == session_id)
                .ok_or_else(|| AudioError::UnknownSession(session_id.to_owned()))?;
            session.volume = clamp_volume(volume);
            session.muted = muted;
        }
        self.emit(AudioEvent::SessionChanged {
            id: session_id.to_owned(),
        });
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;

    fn default_device(mock: &MockBackend) -> Device {
        mock.snapshot().unwrap().default_device.unwrap()
    }

    fn drain(rx: &mut broadcast::Receiver<AudioEvent>) -> Vec<AudioEvent> {
        let mut events = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => events.push(e),
                Err(TryRecvError::Empty) => return events,
                Err(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn demo_snapshot() {
        let mock = MockBackend::demo();
        let state = mock.snapshot().unwrap();
        assert_eq!(state.devices.len(), 3);
        let device = state.default_device.unwrap();
        assert_eq!(device.name, "Speakers (Mock Audio)");
        assert_eq!(device.sessions.len(), 4);
        for session in &device.sessions {
            assert!(session.id.contains(&device.id));
        }
    }

    #[test]
    fn new_rejects_unknown_default() {
        assert!(matches!(
            MockBackend::new(vec![], Some("x")),
            Err(AudioError::UnknownDevice(_))
        ));
        let empty = MockBackend::new(vec![], None).unwrap();
        assert_eq!(empty.snapshot().unwrap(), AudioState::default());
    }

    #[test]
    fn set_master_applies_present_fields_and_emits() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        mock.set_master(Some(150.0), None).unwrap();
        let d = default_device(&mock);
        assert_eq!((d.volume, d.muted), (100.0, false));
        mock.set_master(None, Some(true)).unwrap();
        let d = default_device(&mock);
        assert_eq!((d.volume, d.muted), (100.0, true));
        assert_eq!(drain(&mut rx), vec![AudioEvent::MasterChanged; 2]);
    }

    #[test]
    fn set_session_sets_both_and_emits() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let id = default_device(&mock).sessions[1].id.clone();
        mock.set_session(&id, -1.0, true).unwrap();
        let s = default_device(&mock)
            .sessions
            .into_iter()
            .find(|s| s.id == id)
            .unwrap();
        assert_eq!((s.volume, s.muted), (0.0, true));
        assert_eq!(drain(&mut rx), vec![AudioEvent::SessionChanged { id }]);
    }

    #[test]
    fn set_session_only_reaches_default_device() {
        let mock = MockBackend::demo();
        let steam = MockBackend::session_id("c5a32106-264d-40b2-a2e0-74eda397454c", "steam");
        assert!(matches!(
            mock.set_session(&steam, 1.0, false),
            Err(AudioError::UnknownSession(_))
        ));
    }

    #[test]
    fn switching_default_device_switches_sessions() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        mock.set_default_device("c98e4030-926b-4e62-8c83-1c529574cc51")
            .unwrap();
        let d = default_device(&mock);
        assert_eq!(d.name, "Headset (Mock USB Audio)");
        assert_eq!(d.sessions.len(), 1);
        assert_eq!(d.sessions[0].name, "Zoom");
        assert_eq!(drain(&mut rx), vec![AudioEvent::DefaultDeviceChanged]);
        assert!(matches!(
            mock.set_default_device("nope"),
            Err(AudioError::UnknownDevice(_))
        ));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn scripted_session_changes_emit() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let device_id = default_device(&mock).id;
        let id = MockBackend::session_id(&device_id, "vlc");
        mock.add_session(
            &device_id,
            Session {
                id: id.clone(),
                name: "VLC".into(),
                volume: 30.0,
                muted: false,
            },
        )
        .unwrap();
        assert_eq!(default_device(&mock).sessions.len(), 5);
        mock.remove_session(&id).unwrap();
        assert_eq!(default_device(&mock).sessions.len(), 4);
        assert!(matches!(
            mock.remove_session(&id),
            Err(AudioError::UnknownSession(_))
        ));
        assert_eq!(
            drain(&mut rx),
            vec![
                AudioEvent::SessionAdded { id: id.clone() },
                AudioEvent::SessionRemoved { id }
            ]
        );
    }

    #[test]
    fn removing_default_device_picks_another() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let id = default_device(&mock).id;
        mock.remove_device(&id).unwrap();
        let state = mock.snapshot().unwrap();
        assert_eq!(state.devices.len(), 2);
        assert_eq!(
            state.default_device.unwrap().name,
            "Headphones (Mock Audio)"
        );
        assert_eq!(
            drain(&mut rx),
            vec![AudioEvent::DevicesChanged, AudioEvent::DefaultDeviceChanged]
        );
    }

    #[test]
    fn last_device_removed_then_added() {
        let mock = MockBackend::new(vec![MockDevice::new("a", "A", 10.0)], Some("a")).unwrap();
        mock.remove_device("a").unwrap();
        assert!(mock.snapshot().unwrap().default_device.is_none());
        assert!(matches!(
            mock.set_master(Some(1.0), None),
            Err(AudioError::NoDefaultDevice)
        ));
        mock.add_device(MockDevice::new("b", "B", 20.0));
        assert_eq!(default_device(&mock).id, "b");
    }
}
