use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Number;

use crate::{IconId, IconRequest};

/// Full audio state sent by the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullState {
    pub protocol_version: u32,
    pub application_version: String,
    /// Extensions the server supports, such as [`crate::FEATURE_ICONS`].
    /// Omitted from the wire when empty, which keeps the line identical to
    /// plain v7.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_as_default"
    )]
    pub features: Vec<String>,
    /// Active playback devices, id to friendly name.
    pub device_ids: BTreeMap<String, String>,
    pub default_device: DeviceState,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceState {
    pub device_id: String,
    pub name: String,
    pub master_volume: f32,
    pub master_muted: bool,
    pub sessions: Vec<SessionState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    pub name: String,
    pub id: String,
    pub volume: f32,
    pub muted: bool,
    /// Omitted until the server knows the session's icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_id: Option<IconId>,
}

/// A client message: a partial [`FullState`].
///
/// `protocolVersion` is optional here so that a missing version is reported as
/// a version mismatch rather than as malformed JSON; both drop the client.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_ids: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_device: Option<DeviceUpdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_request: Option<IconRequest>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_volume: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionUpdate>>,
}

/// A session entry in a client update. `id`, `volume` and `muted` are all
/// required; an entry missing any of them makes the message malformed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub id: String,
    pub volume: f32,
    pub muted: bool,
}
