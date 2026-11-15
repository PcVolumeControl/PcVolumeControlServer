use crate::{ClientUpdate, FullState, VOLUME_MAX, VOLUME_MIN};

/// A backend operation derived from a client update.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SetDefaultDevice {
        device_id: String,
    },
    SetMaster {
        volume: Option<f32>,
        muted: Option<bool>,
    },
    SetSession {
        session_id: String,
        volume: f32,
        muted: bool,
    },
}

/// Clamps a volume to `0.0..=100.0`. NaN maps to 0.
pub fn clamp_volume(volume: f32) -> f32 {
    if volume.is_nan() {
        VOLUME_MIN
    } else {
        volume.clamp(VOLUME_MIN, VOLUME_MAX)
    }
}

/// Decides which backend commands a client update produces, given the current
/// state (`None` when there is no default device). The kinds are mutually
/// exclusive and checked in order:
///
/// 1. `defaultDevice.deviceId` differs from the current default: switch the
///    default device and nothing else. The id is not checked against
///    `deviceIds`; the backend rejects unknown ids, so the failure is visible.
/// 2. `masterVolume` or `masterMuted` present: apply those and nothing else.
/// 3. Otherwise, for each live session on the default device, apply volume and
///    mute from the first `sessions[]` entry with a matching `id`.
///
/// An update without `defaultDevice` or without `defaultDevice.deviceId`
/// produces nothing. Without a current default device, any `deviceId` counts as
/// a switch. The protocol version is not checked here.
pub fn plan_commands(current: Option<&FullState>, update: &ClientUpdate) -> Vec<Command> {
    let Some(device) = &update.default_device else {
        return Vec::new();
    };
    let Some(device_id) = device.device_id.as_deref() else {
        return Vec::new();
    };
    let current_device = match current {
        Some(current) if current.default_device.device_id == device_id => &current.default_device,
        _ => {
            return vec![Command::SetDefaultDevice {
                device_id: device_id.to_owned(),
            }];
        }
    };

    if device.master_volume.is_some() || device.master_muted.is_some() {
        return vec![Command::SetMaster {
            volume: device.master_volume.map(clamp_volume),
            muted: device.master_muted,
        }];
    }

    let Some(requested) = &device.sessions else {
        return Vec::new();
    };
    current_device
        .sessions
        .iter()
        .filter_map(|live| {
            requested
                .iter()
                .find(|s| s.id == live.id)
                .map(|s| Command::SetSession {
                    session_id: s.id.clone(),
                    volume: clamp_volume(s.volume),
                    muted: s.muted,
                })
        })
        .collect()
}
