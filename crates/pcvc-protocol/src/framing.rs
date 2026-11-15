use std::fmt;

use serde_json::{Number, Value};

use crate::{ClientUpdate, FullState, MAX_ICON_IDS_PER_REQUEST, PROTOCOL_VERSION};

/// Longest client line, in bytes excluding the terminator, that is accepted.
/// Real client messages are a few hundred bytes.
pub const MAX_LINE_LEN: usize = 64 * 1024;

/// Why a client line was rejected. Every variant means the server closes the
/// client's connection.
#[derive(Debug)]
pub enum ProtocolError {
    Malformed(serde_json::Error),
    LineTooLong { len: usize },
    VersionMismatch { received: Option<Number> },
    TooManyIconIds { count: usize },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "malformed message: {e}"),
            Self::LineTooLong { len } => {
                write!(
                    f,
                    "line of {len} bytes exceeds the {MAX_LINE_LEN} byte limit"
                )
            }
            Self::VersionMismatch { received: Some(v) } => {
                write!(f, "protocol version {v}, expected {PROTOCOL_VERSION}")
            }
            Self::VersionMismatch { received: None } => {
                write!(f, "missing protocolVersion, expected {PROTOCOL_VERSION}")
            }
            Self::TooManyIconIds { count } => write!(
                f,
                "iconRequest has {count} ids, at most {MAX_ICON_IDS_PER_REQUEST} are allowed"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(e) => Some(e),
            Self::LineTooLong { .. }
            | Self::VersionMismatch { .. }
            | Self::TooManyIconIds { .. } => None,
        }
    }
}

/// Serialises a full state as one UTF-8 JSON line, including the trailing `\n`.
///
/// Volumes must be finite (see [`crate::clamp_volume`]); serde_json writes
/// non-finite floats as `null`, which the clients cannot decode.
pub fn encode_full_state(state: &FullState) -> String {
    let mut line = serde_json::to_string(state).expect("FullState has only string map keys");
    line.push('\n');
    line
}

/// Parses one client line (with or without its line terminator).
///
/// Returns `Ok(None)` for a blank line, which clients may send as a keepalive.
pub fn parse_client_line(line: &str) -> Result<Option<ClientUpdate>, ProtocolError> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.len() > MAX_LINE_LEN {
        return Err(ProtocolError::LineTooLong { len: line.len() });
    }
    if line.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(line).map_err(ProtocolError::Malformed)?;
    check_shape(&value).map_err(|msg| ProtocolError::Malformed(serde::de::Error::custom(msg)))?;
    let update: ClientUpdate = serde_json::from_value(value).map_err(ProtocolError::Malformed)?;
    if !update
        .protocol_version
        .as_ref()
        .is_some_and(is_supported_version)
    {
        return Err(ProtocolError::VersionMismatch {
            received: update.protocol_version,
        });
    }
    if let Some(request) = &update.icon_request
        && request.ids.len() > MAX_ICON_IDS_PER_REQUEST
    {
        return Err(ProtocolError::TooManyIconIds {
            count: request.ids.len(),
        });
    }
    Ok(Some(update))
}

/// Integral floats such as `7.0` are accepted, as the C# server did.
fn is_supported_version(version: &Number) -> bool {
    version.as_f64() == Some(f64::from(PROTOCOL_VERSION))
}

/// Serde's derived `Deserialize` also accepts a JSON array as a positional
/// encoding of a struct. Clients only ever send objects, so anything else in an
/// object position is malformed.
fn check_shape(value: &Value) -> Result<(), &'static str> {
    let Some(root) = value.as_object() else {
        return Err("expected a JSON object");
    };
    if root
        .get("iconRequest")
        .is_some_and(|v| !v.is_null() && !v.is_object())
    {
        return Err("iconRequest must be an object");
    }
    let Some(device) = root.get("defaultDevice").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(device) = device.as_object() else {
        return Err("defaultDevice must be an object");
    };
    let Some(Value::Array(sessions)) = device.get("sessions") else {
        return Ok(());
    };
    if sessions.iter().all(Value::is_object) {
        Ok(())
    } else {
        Err("sessions entries must be objects")
    }
}
