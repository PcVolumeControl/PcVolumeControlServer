use std::panic::{AssertUnwindSafe, catch_unwind};

use pcvc_audio::{AudioState, Session};
use pcvc_protocol::{
    APPLICATION_VERSION, DeviceState, FEATURE_ICONS, FullState, IconId, PROTOCOL_VERSION,
    SessionState, clamp_volume,
};

use crate::IconProvider;

/// Protocol extensions a server with or without an icon provider advertises,
/// in the full state and over mDNS.
pub(crate) fn features(icons: bool) -> Vec<&'static str> {
    if icons {
        vec![FEATURE_ICONS]
    } else {
        Vec::new()
    }
}

/// Converts a backend snapshot to the plain v7 wire full state. `None` when
/// there is no default device, since the protocol has no way to express that.
pub fn to_full_state(state: &AudioState) -> Option<FullState> {
    to_full_state_with_icons(state, None)
}

/// [`to_full_state`] plus, with a provider, the icon extension: the `icons`
/// feature and each session's icon id, if the provider knows it.
pub(crate) fn to_full_state_with_icons(
    state: &AudioState,
    icons: Option<&dyn IconProvider>,
) -> Option<FullState> {
    let device = state.default_device.as_ref()?;
    Some(FullState {
        protocol_version: PROTOCOL_VERSION,
        application_version: APPLICATION_VERSION.to_owned(),
        features: features(icons.is_some())
            .into_iter()
            .map(str::to_owned)
            .collect(),
        device_ids: state
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone()))
            .collect(),
        default_device: DeviceState {
            device_id: device.id.clone(),
            name: device.name.clone(),
            master_volume: clamp_volume(device.volume),
            master_muted: device.muted,
            sessions: device
                .sessions
                .iter()
                .map(|s| SessionState {
                    name: s.name.clone(),
                    id: s.id.clone(),
                    volume: clamp_volume(s.volume),
                    muted: s.muted,
                    icon_id: icons.and_then(|icons| icon_id(icons, s)),
                })
                .collect(),
        },
    })
}

/// A panicking provider must not stop states from being published, nor be
/// reported as an audio backend failure.
fn icon_id(icons: &dyn IconProvider, session: &Session) -> Option<IconId> {
    catch_unwind(AssertUnwindSafe(|| icons.icon_id(session))).unwrap_or_else(|_| {
        tracing::error!(session = %session.name, "icon provider panicked in icon_id");
        None
    })
}

#[cfg(test)]
mod tests {
    use pcvc_audio::{Device, DeviceInfo, Session};

    use super::*;

    #[test]
    fn no_default_device_has_no_full_state() {
        assert_eq!(to_full_state(&AudioState::default()), None);
    }

    #[test]
    fn converts_and_sanitises_volumes() {
        let state = AudioState {
            devices: vec![DeviceInfo {
                id: "a".into(),
                name: "A".into(),
            }],
            default_device: Some(
                Device::new("a", "A", f32::NAN).with_session(Session::new("a|s", "S", 120.0)),
            ),
        };
        let full = to_full_state(&state).unwrap();
        assert_eq!(full.protocol_version, PROTOCOL_VERSION);
        assert_eq!(full.device_ids["a"], "A");
        assert_eq!(full.default_device.master_volume, 0.0);
        assert_eq!(full.default_device.sessions[0].volume, 100.0);
    }

    /// Knows an id for session "S" and panics for "P".
    struct Provider(tokio::sync::broadcast::Sender<()>);

    impl IconProvider for Provider {
        fn icon_id(&self, session: &Session) -> Option<IconId> {
            match session.name.as_str() {
                "S" => Some(IconId::new("abc").unwrap()),
                "P" => panic!("provider bug"),
                _ => None,
            }
        }

        fn icon_png(&self, _id: &IconId, _size: u32) -> Option<crate::IconImage> {
            None
        }

        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
            self.0.subscribe()
        }
    }

    #[test]
    fn provider_adds_features_and_ids_and_its_panics_are_contained() {
        let state = AudioState {
            devices: Vec::new(),
            default_device: Some(
                Device::new("a", "A", 50.0)
                    .with_session(Session::new("a|s", "S", 1.0))
                    .with_session(Session::new("a|p", "P", 1.0))
                    .with_session(Session::new("a|o", "O", 1.0)),
            ),
        };
        let provider = Provider(tokio::sync::broadcast::Sender::new(1));
        let full = to_full_state_with_icons(&state, Some(&provider)).unwrap();
        assert_eq!(full.features, [FEATURE_ICONS]);
        let ids: Vec<_> = full
            .default_device
            .sessions
            .iter()
            .map(|s| s.icon_id.as_ref().map(IconId::as_str))
            .collect();
        assert_eq!(ids, [Some("abc"), None, None]);

        let plain = to_full_state(&state).unwrap();
        assert!(plain.features.is_empty());
        assert!(
            plain
                .default_device
                .sessions
                .iter()
                .all(|s| s.icon_id.is_none())
        );
    }
}
