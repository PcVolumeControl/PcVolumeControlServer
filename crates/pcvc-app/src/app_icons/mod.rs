//! Application icons for the Mixer's session rows.
//!
//! A [`Resolver`] turns what a session says about its application
//! ([`pcvc_audio::AppHint`]) into a square RGBA image; on Linux it searches
//! desktop entries and icon themes. Resolution blocks on the file system,
//! so [`IconWorker`] runs it on a background thread and posts each result
//! to the inbox. [`IconCache`] remembers results, including misses.

mod cache;
pub mod fallback;
#[cfg(target_os = "linux")]
mod linux;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod raster;

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, mpsc};

use iced::widget::image;
use pcvc_audio::Session;

pub use cache::{IconCache, Slot};

use crate::icons::Rgba;
use crate::inbox::{Inbound, Inbox};

/// Everything a lookup depends on, so equal keys always resolve alike.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IconKey {
    pub name: Arc<str>,
    pub icon_name: Option<Arc<str>>,
    pub app_id: Option<Arc<str>>,
    pub process_binary: Option<Arc<str>>,
    /// The process, where the platform's lookup uses it (Steam games on
    /// Linux); `None` elsewhere so sessions of one program share a key.
    pub process_id: Option<u32>,
    /// Edge length in physical pixels.
    pub size: u32,
}

impl IconKey {
    pub fn new(session: &Session, size: u32) -> Self {
        Self {
            name: Arc::from(session.name.as_str()),
            icon_name: session.app.icon_name.clone(),
            app_id: session.app.app_id.clone(),
            process_binary: session.app.process_binary.clone(),
            process_id: session.app.process_id.filter(|_| cfg!(target_os = "linux")),
            size,
        }
    }
}

/// Finds and renders application icons. May block.
pub trait Resolver: Send + Sync {
    /// A `key.size` square image, or `None` if nothing usable was found.
    fn resolve(&self, key: &IconKey) -> Option<Rgba>;
}

/// Finds nothing; every session gets the fallback tile.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(
    all(target_os = "linux", not(test)),
    expect(dead_code, reason = "the fallback on other platforms and in tests")
)]
pub struct NoIcons;

impl Resolver for NoIcons {
    fn resolve(&self, _: &IconKey) -> Option<Rgba> {
        None
    }
}

/// The resolver for this platform.
pub fn system_resolver() -> Arc<dyn Resolver> {
    #[cfg(target_os = "linux")]
    return Arc::new(linux::LinuxResolver::new(linux::Dirs::from_env()));
    #[cfg(not(target_os = "linux"))]
    return Arc::new(NoIcons);
}

/// A finished lookup, as posted to the inbox.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub key: IconKey,
    pub icon: Option<image::Handle>,
}

/// Resolves keys one at a time, in order, on a background thread.
#[derive(Debug, Clone)]
pub struct IconWorker(mpsc::Sender<IconKey>);

impl IconWorker {
    pub fn spawn(resolver: Arc<dyn Resolver>, inbox: Inbox) -> Self {
        let (tx, rx) = mpsc::channel::<IconKey>();
        let spawned = std::thread::Builder::new()
            .name("app-icons".into())
            .spawn(move || {
                for key in rx {
                    // A panicking lookup counts as a miss rather than
                    // ending all later lookups.
                    let icon =
                        std::panic::catch_unwind(AssertUnwindSafe(|| resolver.resolve(&key)))
                            .unwrap_or_else(|_| {
                                tracing::warn!(app = %key.name, "icon lookup panicked");
                                None
                            });
                    let icon = icon.map(|i| image::Handle::from_rgba(i.width, i.height, i.pixels));
                    if !inbox.send(Inbound::AppIcon(Resolved { key, icon })) {
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the icon thread: {e}");
        }
        Self(tx)
    }

    /// Queues a lookup. Returns false if the thread is not running, so no
    /// result will come.
    pub fn request(&self, key: IconKey) -> bool {
        self.0.send(key).is_ok()
    }
}
