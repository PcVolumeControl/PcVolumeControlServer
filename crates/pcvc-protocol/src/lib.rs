//! PcVolumeControl wire protocol v7.
//!
//! Newline-delimited JSON over TCP. The server sends a [`FullState`] on connect
//! and after every audio change; clients send partial [`ClientUpdate`]s. See
//! `docs/PROTOCOL.md` at the repository root for the full specification.

mod framing;
mod model;
mod update;

pub use framing::{MAX_LINE_LEN, ProtocolError, encode_full_state, parse_client_line};
pub use model::{ClientUpdate, DeviceState, DeviceUpdate, FullState, SessionState, SessionUpdate};
pub use update::{Command, clamp_volume, plan_commands};

/// Protocol version spoken by this implementation. Clients sending any other
/// value are disconnected.
pub const PROTOCOL_VERSION: u32 = 7;

/// Application version reported in every full state.
pub const APPLICATION_VERSION: &str = "v10";

/// Volumes on the wire are percentages in this inclusive range.
pub const VOLUME_MIN: f32 = 0.0;
pub const VOLUME_MAX: f32 = 100.0;
