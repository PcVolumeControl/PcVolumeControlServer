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
/// state. The kinds are mutually exclusive and checked in order:
///
/// 1. `defaultDevice.deviceId` differs from the current default: switch the
///    default device (only if the id is a known active device) and nothing else.
/// 2. `masterVolume` or `masterMuted` present: apply those and nothing else.
/// 3. Otherwise, for each live session on the default device, apply volume and
///    mute from the first `sessions[]` entry with a matching `id`.
///
/// An update without `defaultDevice` or without `defaultDevice.deviceId`
/// produces nothing. The protocol version is not checked here.
pub fn plan_commands(current: &FullState, update: &ClientUpdate) -> Vec<Command> {
    let Some(device) = &update.default_device else {
        return Vec::new();
    };
    let Some(device_id) = device.device_id.as_deref() else {
        return Vec::new();
    };
    let current_device = &current.default_device;

    if device_id != current_device.device_id {
        if current.device_ids.contains_key(device_id) {
            return vec![Command::SetDefaultDevice {
                device_id: device_id.to_owned(),
            }];
        }
        return Vec::new();
    }

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
