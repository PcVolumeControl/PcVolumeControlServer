//! The icon extension: icon ids on sessions, icon requests from clients and
//! the replies to them.

use std::fmt;
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

/// Feature name advertised in [`crate::FullState::features`] and in the mDNS
/// `features` TXT record when the server serves icons.
pub const FEATURE_ICONS: &str = "icons";

/// Longest valid [`IconId`], in characters.
pub const ICON_ID_MAX_LEN: usize = 64;

/// Most ids one `iconRequest` may carry; more is a protocol error.
pub const MAX_ICON_IDS_PER_REQUEST: usize = 32;

/// Most icon ids a connection may have outstanding: read by the server but
/// with no reply written yet. A request that would exceed it is a protocol
/// error.
pub const MAX_OUTSTANDING_ICON_IDS: usize = 64;

/// Edge length, in pixels, used when a request has no `size`.
pub const ICON_SIZE_DEFAULT: u32 = 64;
/// Requested sizes are clamped to `ICON_SIZE_MIN..=ICON_SIZE_MAX`.
pub const ICON_SIZE_MIN: u32 = 16;
pub const ICON_SIZE_MAX: u32 = 256;

/// Largest PNG the server sends; a bigger image is answered as not found.
pub const MAX_ICON_PNG_BYTES: usize = 512 * 1024;

/// The only image type the server sends.
pub const ICON_MIME_PNG: &str = "image/png";

/// `error` value of a reply for an id the server has no image for.
pub const ICON_ERROR_NOT_FOUND: &str = "notFound";

/// Opaque identifier of an icon image: 1 to [`ICON_ID_MAX_LEN`] characters of
/// `[a-z0-9]`.
///
/// Ids are derived from the image content, so an id always denotes the same
/// image and clients may cache by id indefinitely.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IconId(String);

impl IconId {
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidIconId> {
        let id = id.into();
        let valid = (1..=ICON_ID_MAX_LEN).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidIconId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IconId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for IconId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for IconId {
    type Err = InvalidIconId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for IconId {
    type Error = InvalidIconId;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<IconId> for String {
    fn from(id: IconId) -> Self {
        id.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidIconId;

impl fmt::Display for InvalidIconId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "icon ids are 1 to {ICON_ID_MAX_LEN} characters of [a-z0-9]"
        )
    }
}

impl std::error::Error for InvalidIconId {}

/// The `iconRequest` member of a client message.
///
/// `ids` are kept as sent: an id that is not a valid [`IconId`] is answered
/// with a not-found reply rather than rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IconRequest {
    pub ids: Vec<String>,
    /// Desired edge length in pixels; see [`IconRequest::size_px`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
}

impl IconRequest {
    /// The requested edge length: [`ICON_SIZE_DEFAULT`] when absent, otherwise
    /// rounded and clamped to `ICON_SIZE_MIN..=ICON_SIZE_MAX`.
    pub fn size_px(&self) -> u32 {
        let Some(size) = self.size else {
            return ICON_SIZE_DEFAULT;
        };
        size.round()
            .clamp(f64::from(ICON_SIZE_MIN), f64::from(ICON_SIZE_MAX)) as u32
    }

    /// The ids in request order with later duplicates removed.
    pub fn unique_ids(&self) -> Vec<&str> {
        let mut unique: Vec<&str> = Vec::with_capacity(self.ids.len());
        for id in &self.ids {
            if !unique.contains(&id.as_str()) {
                unique.push(id);
            }
        }
        unique
    }
}

/// A server line answering one requested id. Sent only to the client that
/// asked, never broadcast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IconMessage {
    pub protocol_version: u32,
    pub icon: IconReply,
}

/// The `icon` member of an [`IconMessage`]. `id` echoes the requested id
/// verbatim, which need not be a valid [`IconId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IconReply {
    Image {
        id: String,
        /// Actual edge length in pixels.
        size: u32,
        mime: String,
        /// Standard base64 with padding.
        data: String,
    },
    Error {
        id: String,
        /// [`ICON_ERROR_NOT_FOUND`]; clients treat any value as "no image".
        error: String,
    },
}

impl IconReply {
    /// An image reply, or a not-found reply if `png` is larger than
    /// [`MAX_ICON_PNG_BYTES`].
    pub fn image(id: impl Into<String>, size: u32, png: &[u8]) -> Self {
        if png.len() > MAX_ICON_PNG_BYTES {
            return Self::not_found(id);
        }
        Self::Image {
            id: id.into(),
            size,
            mime: ICON_MIME_PNG.to_owned(),
            data: BASE64.encode(png),
        }
    }

    pub fn not_found(id: impl Into<String>) -> Self {
        Self::Error {
            id: id.into(),
            error: ICON_ERROR_NOT_FOUND.to_owned(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Image { id, .. } | Self::Error { id, .. } => id,
        }
    }
}

/// Serialises an icon reply as one UTF-8 JSON line, including the trailing
/// `\n`.
pub fn encode_icon_reply(reply: IconReply) -> String {
    let message = IconMessage {
        protocol_version: PROTOCOL_VERSION,
        icon: reply,
    };
    let mut line = serde_json::to_string(&message).expect("IconMessage always serialises");
    line.push('\n');
    line
}
