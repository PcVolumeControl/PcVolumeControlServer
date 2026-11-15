use std::sync::{Mutex, MutexGuard, PoisonError};

use pcvc_protocol::clamp_volume;
use tokio::sync::broadcast;

use crate::{
    AppHint, AudioBackend, AudioError, AudioEvent, AudioState, Device, DeviceInfo, Session,
};

const EVENT_CAPACITY: usize = 64;

#[derive(Debug)]
struct MockState {
    /// Every device keeps its own sessions; the default device's are reported.
    devices: Vec<Device>,
    default_id: Option<String>,
}

impl MockState {
    fn default_device_mut(&mut self) -> Result<&mut Device, AudioError> {
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
/// Commands mutate its state and emit events like a real backend would; a call
/// that changes nothing emits nothing. The scripting methods (`add_device`,
/// `remove_session`, ...) simulate changes made outside the protocol, such as
/// an application starting playback.
#[derive(Debug)]
pub struct MockBackend {
    state: Mutex<MockState>,
    events: broadcast::Sender<AudioEvent>,
}

impl MockBackend {
    /// Creates a mock with the given devices. Volumes are clamped. Fails if
    /// `default_device_id` is not one of them or a session id does not contain
    /// its device id.
    pub fn new(
        mut devices: Vec<Device>,
        default_device_id: Option<&str>,
    ) -> Result<Self, AudioError> {
        for device in &mut devices {
            normalize(device)?;
        }
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
        let session = |device: &str, app: &str, name: &str, volume: f32| {
            Session::new(Self::session_id(device, app), name, volume).with_app(AppHint {
                process_binary: Some(app.into()),
                ..AppHint::default()
            })
        };
        let devices = vec![
            Device::new(SPEAKERS, "Speakers (Mock Audio)", 80.0)
                .with_session(session(SPEAKERS, "firefox", "Firefox", 100.0))
                .with_session(session(SPEAKERS, "spotify", "Spotify", 65.0))
                .with_session(session(SPEAKERS, "discord", "Discord", 40.0))
                .with_session(session(SPEAKERS, "musique", "Musique \u{e9}t\u{e9}", 55.0)),
            Device::new(HEADPHONES, "Headphones (Mock Audio)", 50.0)
                .with_session(session(HEADPHONES, "steam", "Steam", 83.0)),
            Device::new(HEADSET, "Headset (Mock USB Audio)", 70.0)
                .with_session(session(HEADSET, "zoom", "Zoom", 90.0)),
        ];
        Self::new(devices, Some(SPEAKERS)).expect("demo data is valid")
    }

    /// The session id the mock uses for `app` on `device_id`. It embeds the
    /// device id, as the protocol requires.
    pub fn session_id(device_id: &str, app: &str) -> String {
        format!("{device_id}|mock/{app}")
    }

    /// Adds a device, or replaces the device with the same id in place. If
    /// there is no default device it becomes the default.
    pub fn add_device(&self, mut device: Device) -> Result<(), AudioError> {
        normalize(&mut device)?;
        let default_changed = {
            let mut state = self.lock();
            if state.default_id.is_none() {
                state.default_id = Some(device.id.clone());
            }
            let is_default = state.default_id.as_deref() == Some(&device.id);
            match state.devices.iter_mut().find(|d| d.id == device.id) {
                Some(existing) => *existing = device,
                None => state.devices.push(device),
            }
            is_default
        };
        self.emit(AudioEvent::DevicesChanged);
        if default_changed {
            self.emit(AudioEvent::DefaultDeviceChanged);
        }
        Ok(())
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

    /// Adds a session to a device, or replaces the session with the same id.
    pub fn add_session(&self, device_id: &str, mut session: Session) -> Result<(), AudioError> {
        normalize_session(device_id, &mut session)?;
        let event = {
            let mut state = self.lock();
            let device = state
                .devices
                .iter_mut()
                .find(|d| d.id == device_id)
                .ok_or_else(|| AudioError::UnknownDevice(device_id.to_owned()))?;
            match device.sessions.iter_mut().find(|s| s.id == session.id) {
                Some(existing) => {
                    let event = session.change_from(existing);
                    *existing = session;
                    event
                }
                None => {
                    let id = session.id.clone();
                    device.sessions.push(session);
                    Some(AudioEvent::SessionAdded { id })
                }
            }
        };
        if let Some(event) = event {
            self.emit(event);
        }
        Ok(())
    }

    /// Sets the detail of a session on any device, leaving the rest of it as
    /// it is.
    pub fn set_session_detail(
        &self,
        session_id: &str,
        detail: Option<&str>,
    ) -> Result<(), AudioError> {
        let changed = {
            let mut state = self.lock();
            let session = state
                .devices
                .iter_mut()
                .flat_map(|d| d.sessions.iter_mut())
                .find(|s| s.id == session_id)
                .ok_or_else(|| AudioError::UnknownSession(session_id.to_owned()))?;
            let detail = detail.map(str::to_owned);
            let changed = session.detail != detail;
            session.detail = detail;
            changed
        };
        if changed {
            self.emit(AudioEvent::SessionDetailChanged {
                id: session_id.to_owned(),
            });
        }
        Ok(())
    }

    /// Removes a session from every device that holds it.
    pub fn remove_session(&self, session_id: &str) -> Result<(), AudioError> {
        let mut removed = false;
        for device in &mut self.lock().devices {
            let before = device.sessions.len();
            device.sessions.retain(|s| s.id != session_id);
            removed |= device.sessions.len() != before;
        }
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

fn normalize(device: &mut Device) -> Result<(), AudioError> {
    device.volume = clamp_volume(device.volume);
    for session in &mut device.sessions {
        normalize_session(&device.id, session)?;
    }
    Ok(())
}

fn normalize_session(device_id: &str, session: &mut Session) -> Result<(), AudioError> {
    if !session.id.contains(device_id) {
        return Err(AudioError::InvalidSessionId {
            session_id: session.id.clone(),
            device_id: device_id.to_owned(),
        });
    }
    session.volume = clamp_volume(session.volume);
    Ok(())
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
            .cloned();
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
            if state.default_id.as_deref() == Some(device_id) {
                return Ok(());
            }
            state.default_id = Some(device_id.to_owned());
        }
        self.emit(AudioEvent::DefaultDeviceChanged);
        Ok(())
    }

    fn set_master(&self, volume: Option<f32>, muted: Option<bool>) -> Result<(), AudioError> {
        let changed = {
            let mut state = self.lock();
            let device = state.default_device_mut()?;
            let before = (device.volume, device.muted);
            if let Some(muted) = muted {
                device.muted = muted;
            }
            if let Some(volume) = volume {
                device.volume = clamp_volume(volume);
            }
            before != (device.volume, device.muted)
        };
        if changed {
            self.emit(AudioEvent::MasterChanged);
        }
        Ok(())
    }

    fn set_session(&self, session_id: &str, volume: f32, muted: bool) -> Result<(), AudioError> {
        let changed = {
            let mut state = self.lock();
            let session = state
                .default_device_mut()?
                .sessions
                .iter_mut()
                .find(|s| s.id == session_id)
                .ok_or_else(|| AudioError::UnknownSession(session_id.to_owned()))?;
            let after = (clamp_volume(volume), muted);
            let changed = (session.volume, session.muted) != after;
            (session.volume, session.muted) = after;
            changed
        };
        if changed {
            self.emit(AudioEvent::SessionChanged {
                id: session_id.to_owned(),
            });
        }
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use pcvc_protocol::Command;
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;

    const HEADPHONES: &str = "c5a32106-264d-40b2-a2e0-74eda397454c";
    const HEADSET: &str = "c98e4030-926b-4e62-8c83-1c529574cc51";

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
    fn new_validates_and_clamps() {
        assert!(matches!(
            MockBackend::new(vec![], Some("x")),
            Err(AudioError::UnknownDevice(_))
        ));
        assert!(matches!(
            MockBackend::new(
                vec![Device::new("a", "A", 1.0).with_session(Session::new("s", "S", 1.0))],
                None
            ),
            Err(AudioError::InvalidSessionId { .. })
        ));
        let empty = MockBackend::new(vec![], None).unwrap();
        assert_eq!(empty.snapshot().unwrap(), AudioState::default());

        let mock = MockBackend::new(
            vec![Device::new("a", "A", f32::NAN).with_session(Session::new("a|s", "S", 400.0))],
            Some("a"),
        )
        .unwrap();
        let d = default_device(&mock);
        assert_eq!((d.volume, d.sessions[0].volume), (0.0, 100.0));
    }

    #[test]
    fn set_master_applies_present_fields_and_emits_on_change() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        mock.set_master(Some(150.0), None).unwrap();
        let d = default_device(&mock);
        assert_eq!((d.volume, d.muted), (100.0, false));
        mock.set_master(None, Some(true)).unwrap();
        let d = default_device(&mock);
        assert_eq!((d.volume, d.muted), (100.0, true));
        mock.set_master(None, None).unwrap();
        mock.set_master(Some(100.0), Some(true)).unwrap();
        assert_eq!(drain(&mut rx), vec![AudioEvent::MasterChanged; 2]);
    }

    #[test]
    fn set_session_sets_both_and_emits_on_change() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let id = default_device(&mock).sessions[1].id.clone();
        mock.set_session(&id, -1.0, true).unwrap();
        mock.set_session(&id, 0.0, true).unwrap();
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
        let steam = MockBackend::session_id(HEADPHONES, "steam");
        assert!(matches!(
            mock.set_session(&steam, 1.0, false),
            Err(AudioError::UnknownSession(_))
        ));
    }

    #[test]
    fn switching_default_device_switches_sessions() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        mock.set_default_device(HEADSET).unwrap();
        mock.set_default_device(HEADSET).unwrap();
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
    fn apply_dispatches_commands() {
        let mock = MockBackend::demo();
        mock.apply(&Command::SetMaster {
            volume: Some(12.0),
            muted: Some(true),
        })
        .unwrap();
        let d = default_device(&mock);
        assert_eq!((d.volume, d.muted), (12.0, true));
        mock.apply(&Command::SetDefaultDevice {
            device_id: HEADSET.into(),
        })
        .unwrap();
        let zoom = MockBackend::session_id(HEADSET, "zoom");
        mock.apply(&Command::SetSession {
            session_id: zoom,
            volume: 5.0,
            muted: true,
        })
        .unwrap();
        let d = default_device(&mock);
        assert_eq!(d.id, HEADSET);
        assert_eq!((d.sessions[0].volume, d.sessions[0].muted), (5.0, true));
    }

    #[test]
    fn scripted_session_changes_emit() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let device_id = default_device(&mock).id;
        let id = MockBackend::session_id(&device_id, "vlc");
        mock.add_session(&device_id, Session::new(&id, "VLC", 30.0))
            .unwrap();
        mock.add_session(&device_id, Session::new(&id, "VLC", 31.0))
            .unwrap();
        assert_eq!(default_device(&mock).sessions.len(), 5);
        mock.remove_session(&id).unwrap();
        assert_eq!(default_device(&mock).sessions.len(), 4);
        assert!(matches!(
            mock.remove_session(&id),
            Err(AudioError::UnknownSession(_))
        ));
        assert!(matches!(
            mock.add_session(&device_id, Session::new("vlc", "VLC", 1.0)),
            Err(AudioError::InvalidSessionId { .. })
        ));
        assert_eq!(
            drain(&mut rx),
            vec![
                AudioEvent::SessionAdded { id: id.clone() },
                AudioEvent::SessionChanged { id: id.clone() },
                AudioEvent::SessionRemoved { id }
            ]
        );
    }

    #[test]
    fn detail_changes_have_their_own_event() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let device_id = default_device(&mock).id;
        let id = MockBackend::session_id(&device_id, "tab");
        let tab = Session::new(&id, "Firefox", 30.0).with_detail("One");
        mock.add_session(&device_id, tab.clone()).unwrap();
        mock.add_session(&device_id, tab.clone()).unwrap();
        mock.add_session(&device_id, tab.clone().with_detail("Two"))
            .unwrap();
        mock.set_session(&id, 40.0, true).unwrap();
        mock.set_session_detail(&id, Some("Three")).unwrap();
        mock.set_session_detail(&id, Some("Three")).unwrap();
        let session = default_device(&mock).sessions.pop().unwrap();
        assert_eq!(
            (session.volume, session.muted, session.detail.as_deref()),
            (40.0, true, Some("Three")),
            "the rest of the session is kept"
        );
        assert!(matches!(
            mock.set_session_detail("nope", None),
            Err(AudioError::UnknownSession(_))
        ));
        assert_eq!(
            drain(&mut rx),
            vec![
                AudioEvent::SessionAdded { id: id.clone() },
                AudioEvent::SessionDetailChanged { id: id.clone() },
                AudioEvent::SessionChanged { id: id.clone() },
                AudioEvent::SessionDetailChanged { id },
            ]
        );
    }

    #[test]
    fn add_device_replaces_in_place() {
        let mock = MockBackend::demo();
        let mut rx = mock.subscribe();
        let speakers = default_device(&mock).id;
        mock.add_device(Device::new(&speakers, "Renamed", 150.0))
            .unwrap();
        let state = mock.snapshot().unwrap();
        assert_eq!(state.devices[0].name, "Renamed");
        assert_eq!(state.default_device.unwrap().volume, 100.0);
        assert_eq!(
            drain(&mut rx),
            vec![AudioEvent::DevicesChanged, AudioEvent::DefaultDeviceChanged]
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
        let mock = MockBackend::new(vec![Device::new("a", "A", 10.0)], Some("a")).unwrap();
        mock.remove_device("a").unwrap();
        assert!(mock.snapshot().unwrap().default_device.is_none());
        assert!(matches!(
            mock.set_master(Some(1.0), None),
            Err(AudioError::NoDefaultDevice)
        ));
        mock.add_device(Device::new("b", "B", 20.0)).unwrap();
        assert_eq!(default_device(&mock).id, "b");
    }
}
