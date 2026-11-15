use std::fmt;

use crate::{ClientUpdate, FullState, PROTOCOL_VERSION};

/// Longest client line, in bytes excluding the terminator, that a server
/// should accept. Real client messages are a few hundred bytes.
pub const MAX_LINE_LEN: usize = 64 * 1024;

/// Why a client line was rejected. Either variant means the server closes the
/// client's connection.
#[derive(Debug)]
pub enum ProtocolError {
    Malformed(serde_json::Error),
    VersionMismatch { received: Option<i64> },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "malformed message: {e}"),
            Self::VersionMismatch { received: Some(v) } => {
                write!(f, "protocol version {v}, expected {PROTOCOL_VERSION}")
            }
            Self::VersionMismatch { received: None } => {
                write!(f, "missing protocolVersion, expected {PROTOCOL_VERSION}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Malformed(e) => Some(e),
            Self::VersionMismatch { .. } => None,
        }
    }
}

/// Serialises a full state as one UTF-8 JSON line, including the trailing `\n`.
pub fn encode_full_state(state: &FullState) -> String {
    let mut line =
        serde_json::to_string(state).expect("FullState contains only JSON-representable data");
    line.push('\n');
    line
}

/// Parses one client line (with or without its line terminator).
///
/// Returns `Ok(None)` for a blank line, which clients may send as a keepalive.
pub fn parse_client_line(line: &str) -> Result<Option<ClientUpdate>, ProtocolError> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.trim().is_empty() {
        return Ok(None);
    }
    // Going through `Value` rejects top-level arrays, which serde would
    // otherwise accept as a positional encoding of the struct.
    let value: serde_json::Value = serde_json::from_str(line).map_err(ProtocolError::Malformed)?;
    if !value.is_object() {
        return Err(ProtocolError::Malformed(serde::de::Error::custom(
            "expected a JSON object",
        )));
    }
    let update: ClientUpdate = serde_json::from_value(value).map_err(ProtocolError::Malformed)?;
    if update.protocol_version != Some(i64::from(PROTOCOL_VERSION)) {
        return Err(ProtocolError::VersionMismatch {
            received: update.protocol_version,
        });
    }
    Ok(Some(update))
}
