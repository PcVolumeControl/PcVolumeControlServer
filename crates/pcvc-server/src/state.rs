use pcvc_audio::AudioState;
use pcvc_protocol::{
    APPLICATION_VERSION, DeviceState, FEATURE_ICONS, FullState, PROTOCOL_VERSION, SessionState,
    clamp_volume,
};

use crate::IconProvider;

/// [`to_full_state`] plus the icon extension when there is a provider: the
/// `icons` feature and each session's icon id, if the provider knows it.
pub(crate) fn to_full_state_with_icons(
    state: &AudioState,
    icons: Option<&dyn IconProvider>,
) -> Option<FullState> {
    let mut full = to_full_state(state)?;
    if let (Some(icons), Some(device)) = (icons, &state.default_device) {
        full.features = vec![FEATURE_ICONS.to_owned()];
        for (wire, session) in full
            .default_device
            .sessions
            .iter_mut()
            .zip(&device.sessions)
        {
            wire.icon_id = icons.icon_id(session);
        }
    }
    Some(full)
}

/// Converts a backend snapshot to the plain v7 wire full state. `None` when
/// there is no default device, since the protocol has no way to express that.
pub fn to_full_state(state: &AudioState) -> Option<FullState> {
    let device = state.default_device.as_ref()?;
    Some(FullState {
        protocol_version: PROTOCOL_VERSION,
        application_version: APPLICATION_VERSION.to_owned(),
        features: Vec::new(),
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
                    icon_id: None,
                })
                .collect(),
        },
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
}
