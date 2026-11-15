//! Shared by the integration tests.

use pcvc_audio::Session;
use pcvc_server::{IconId, IconImage, IconProvider};
use tokio::sync::broadcast;

/// A provider that knows no icons.
pub struct NoIcons(pub broadcast::Sender<()>);

impl IconProvider for NoIcons {
    fn icon_id(&self, _session: &Session) -> Option<IconId> {
        None
    }

    fn icon_png(&self, _id: &IconId, _size: u32) -> Option<IconImage> {
        None
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.0.subscribe()
    }
}
