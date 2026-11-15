use pcvc_audio::AudioState;
use pcvc_protocol::{
    APPLICATION_VERSION, DeviceState, FullState, PROTOCOL_VERSION, SessionState, clamp_volume,
};

/// Converts a backend snapshot to the wire full state. `None` when there is no
/// default device, since the protocol has no way to express that.
pub fn to_full_state(state: &AudioState) -> Option<FullState> {
    let device = state.default_device.as_ref()?;
    Some(FullState {
        protocol_version: PROTOCOL_VERSION,
        application_version: APPLICATION_VERSION.to_owned(),
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
                })
                .collect(),
        },
    })
}

#[cfg(test)]
mod tests {
    use pcvc_audio::{AppHint, Device, DeviceInfo, Session};

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

    #[test]
    fn app_hints_stay_local() {
        let state = |app: AppHint| AudioState {
            devices: vec![],
            default_device: Some(
                Device::new("a", "A", 50.0)
                    .with_session(Session::new("a|s", "S", 1.0).with_app(app)),
            ),
        };
        let plain = to_full_state(&state(AppHint::default())).unwrap();
        let hinted = to_full_state(&state(AppHint {
            icon_name: Some("firefox".into()),
            process_binary: Some("firefox".into()),
            process_id: Some(42),
            app_id: Some("org.mozilla.firefox".into()),
        }))
        .unwrap();
        assert_eq!(plain, hinted);
        assert_eq!(
            pcvc_protocol::encode_full_state(&plain),
            pcvc_protocol::encode_full_state(&hinted)
        );
    }
}
