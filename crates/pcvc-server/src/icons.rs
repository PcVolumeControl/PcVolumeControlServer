use std::sync::Arc;

use pcvc_audio::Session;
use pcvc_protocol::IconId;
use tokio::sync::broadcast;

/// A PNG rendering of an icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconImage {
    /// Actual edge length in pixels, which may differ from the size asked for.
    pub size: u32,
    pub png: Arc<[u8]>,
}

/// Where the server gets app icons from. The server only relays: it tags
/// sessions with [`IconProvider::icon_id`] and answers client requests with
/// [`IconProvider::icon_png`].
///
/// The intended shape is a resolver with a cache: `icon_id` answers from the
/// cache and, on a miss, starts resolving the session's icon in the
/// background and returns `None`. When the background work finds an icon, the
/// provider fires [`IconProvider::subscribe`]'s channel; the server then
/// rebuilds the state, calls `icon_id` again and broadcasts the session with
/// its new id.
///
/// Ids must be content-derived (see [`IconId`]): the same id must always
/// denote the same image, because clients cache by id indefinitely.
pub trait IconProvider: Send + Sync + 'static {
    /// The icon id for `session`, if already known. Called for every session
    /// on every state rebuild, so it must not block.
    fn icon_id(&self, session: &Session) -> Option<IconId>;

    /// PNG bytes for `id` with an edge of about `size` pixels (already clamped
    /// to the protocol's range). `None` if the id is unknown. May block; the
    /// server calls it on a blocking thread, one call at a time per client and
    /// a few at a time overall.
    fn icon_png(&self, id: &IconId, size: u32) -> Option<IconImage>;

    /// Fires when an icon id became available for some session. Lagging is
    /// treated as a signal; closing the channel stops the server listening.
    fn subscribe(&self) -> broadcast::Receiver<()>;
}
