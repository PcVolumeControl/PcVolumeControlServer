//! Application icons for the Mixer's session rows.
//!
//! A [`Resolver`] turns what a session says about its application
//! ([`pcvc_audio::AppHint`]) into a square RGBA image; the platform
//! resolvers search desktop entries and icon themes (Linux) or ask the shell
//! for the executable's icon (Windows). Resolution blocks on the file
//! system, so [`IconWorker`] runs it on a background thread and posts each
//! result to the inbox. [`IconCache`] remembers results, including misses.

mod cache;
#[cfg(target_os = "linux")]
mod linux;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod raster;

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, mpsc};

use iced::widget::image;
use pcvc_audio::Session;

pub use cache::IconCache;

use crate::icons::Rgba;
use crate::inbox::{Inbound, Inbox};

/// What identifies a session's icon. Sessions of one application share it,
/// whatever their process id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IconKey {
    pub name: Arc<str>,
    pub icon_name: Option<Arc<str>>,
    pub app_id: Option<Arc<str>>,
    pub process_binary: Option<Arc<str>>,
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
            size,
        }
    }
}

/// A key plus what only helps to find its icon.
#[derive(Debug, Clone)]
pub struct Request {
    pub key: IconKey,
    /// Used on Linux to find Steam games.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub process_id: Option<u32>,
}

impl Request {
    pub fn new(session: &Session, size: u32) -> Self {
        Self {
            key: IconKey::new(session, size),
            process_id: session.app.process_id,
        }
    }
}

/// Finds and renders application icons. May block.
pub trait Resolver: Send + Sync {
    /// A `key.size` square image, or `None` if nothing usable was found.
    fn resolve(&self, request: &Request) -> Option<Rgba>;
}

/// Finds nothing; every session gets the fallback tile.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(
    all(target_os = "linux", not(test)),
    expect(dead_code, reason = "the fallback on other platforms and in tests")
)]
pub struct NoIcons;

impl Resolver for NoIcons {
    fn resolve(&self, _: &Request) -> Option<Rgba> {
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

/// Resolves requests one at a time, in order, on a background thread.
#[derive(Debug, Clone)]
pub struct IconWorker(mpsc::Sender<Request>);

impl IconWorker {
    pub fn spawn(resolver: Arc<dyn Resolver>, inbox: Inbox) -> Self {
        let (tx, rx) = mpsc::channel::<Request>();
        let spawned = std::thread::Builder::new()
            .name("app-icons".into())
            .spawn(move || {
                for request in rx {
                    // A panicking lookup counts as a miss rather than
                    // ending all later lookups.
                    let icon =
                        std::panic::catch_unwind(AssertUnwindSafe(|| resolver.resolve(&request)))
                            .unwrap_or_else(|_| {
                                tracing::warn!(app = %request.key.name, "icon lookup panicked");
                                None
                            });
                    let icon = icon.map(|i| image::Handle::from_rgba(i.width, i.height, i.pixels));
                    if !inbox.send(Inbound::AppIcon(Resolved {
                        key: request.key,
                        icon,
                    })) {
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the icon thread: {e}");
        }
        Self(tx)
    }

    /// Queues a lookup. Without a running thread the session keeps its
    /// placeholder.
    pub fn request(&self, request: Request) {
        let _ = self.0.send(request);
    }
}
