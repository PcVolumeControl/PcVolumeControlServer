//! PcVolumeControl wire protocol v7, with the optional "v8" extensions.
//!
//! Newline-delimited JSON over TCP. The server sends a [`FullState`] on connect
//! and after every audio change; clients send partial [`ClientUpdate`]s. The
//! extensions are additive and keep `protocolVersion` at 7: a server that
//! serves icons lists [`FEATURE_ICONS`] in [`FullState::features`], tags
//! sessions with an [`IconId`], and answers a client's [`IconRequest`] with
//! [`IconMessage`] lines sent to that client only. See `docs/PROTOCOL.md` at
//! the repository root for the full specification.

mod framing;
mod icon;
mod model;
mod update;

pub use framing::{MAX_LINE_LEN, ProtocolError, encode_full_state, parse_client_line};
pub use icon::{
    FEATURE_ICONS, ICON_ERROR_NOT_FOUND, ICON_ID_MAX_LEN, ICON_MIME_PNG, ICON_SIZE_DEFAULT,
    ICON_SIZE_MAX, ICON_SIZE_MIN, IconId, IconMessage, IconReply, IconRequest, InvalidIconId,
    MAX_ICON_IDS_PER_REQUEST, MAX_ICON_PNG_BYTES, encode_icon_reply,
};
pub use model::{ClientUpdate, DeviceState, DeviceUpdate, FullState, SessionState, SessionUpdate};
pub use update::{Command, clamp_volume, plan_commands};

/// Protocol version on the wire, in both directions, with or without the
/// extensions. Clients sending any other value are disconnected.
pub const PROTOCOL_VERSION: u32 = 7;

/// Application version reported in every full state.
pub const APPLICATION_VERSION: &str = "v10";

/// Volumes on the wire are percentages in this inclusive range.
pub const VOLUME_MIN: f32 = 0.0;
pub const VOLUME_MAX: f32 = 100.0;
