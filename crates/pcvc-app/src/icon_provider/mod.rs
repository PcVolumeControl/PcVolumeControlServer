//! Application icons for phone clients (the protocol's v8 `icons` feature),
//! from the same [`Resolver`] the Mixer uses.
//!
//! [`ResolverIcons::icon_id`] answers from memory. An application seen for
//! the first time is resolved on a background thread at
//! [`REFERENCE_EDGE`]; its id is the SHA-256 of those pixels, so it is the
//! same on every run and shared by applications with the same icon. Misses
//! are remembered and retried as [`IconCache`] decides.
//!
//! [`ResolverIcons::icon_png`] resolves the application again at the
//! requested size, so vector icons are drawn at that size and raster icons
//! resampled, but a raster source is scaled up at most [`MAX_UPSCALE`]
//! times: larger requests get that edge instead.

mod lru;
#[cfg(test)]
mod tests;

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::Instant;

use pcvc_audio::Session;
use pcvc_protocol::{ICON_SIZE_DEFAULT, ICON_SIZE_MIN, MAX_ICON_PNG_BYTES};
use pcvc_server::{IconId, IconImage, IconProvider};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::app_icons::{Found, IconCache, IconKey, Resolver, Slot, encode_png};
use crate::icons::Rgba;
use lru::Lru;

/// Edge at which icons are rendered to derive their ids. It is the
/// protocol's default request size, so that rendering also answers the most
/// common request.
const REFERENCE_EDGE: u32 = ICON_SIZE_DEFAULT;

/// Ids are this many hex digits of a SHA-256.
const ID_LEN: usize = 32;

/// A raster source is drawn at most this many times its own size.
const MAX_UPSCALE: u32 = 2;

/// Application identities remembered, misses included.
const APP_CAPACITY: usize = 256;
/// Ids that can be rendered.
const ID_CAPACITY: usize = 256;
/// Rendered PNGs kept, by count and by total size.
const PNG_CAPACITY: usize = 256;
const PNG_CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Lookups waiting for the background thread; beyond this they count as
/// misses (retried later).
const QUEUE_CAPACITY: usize = 256;

/// The icon provider for the server, or `None` when icons are not served:
/// turned off in the config, or no resolver on this platform. Without a
/// provider the server speaks plain v7.
pub fn for_server(
    serve_icons: bool,
    resolver: &Arc<dyn Resolver>,
) -> Option<Arc<dyn IconProvider>> {
    (serve_icons && resolver.finds_icons())
        .then(|| Arc::new(ResolverIcons::new(Arc::clone(resolver))) as Arc<dyn IconProvider>)
}

/// An [`IconProvider`] backed by a [`Resolver`].
pub struct ResolverIcons {
    inner: Arc<Inner>,
    lookups: mpsc::SyncSender<IconKey>,
}

struct Inner {
    resolver: Arc<dyn Resolver>,
    state: Mutex<State>,
    changed: broadcast::Sender<()>,
}

struct State {
    /// By application identity (an [`IconKey`] of size 0): its id once
    /// resolved, or a miss.
    apps: IconCache<IconKey, IconId>,
    /// By id: the identity of an application last seen with it, which is
    /// what gets resolved to render the id at other sizes.
    ids: Lru<IconId, IconKey>,
    /// By id and requested edge.
    pngs: Lru<(IconId, u32), IconImage>,
}

impl ResolverIcons {
    pub fn new(resolver: Arc<dyn Resolver>) -> Self {
        let inner = Arc::new(Inner {
            resolver,
            state: Mutex::new(State {
                apps: IconCache::new(APP_CAPACITY),
                ids: Lru::new(ID_CAPACITY, usize::MAX),
                pngs: Lru::new(PNG_CAPACITY, PNG_CACHE_BYTES),
            }),
            changed: broadcast::Sender::new(16),
        });
        let (lookups, queue) = mpsc::sync_channel::<IconKey>(QUEUE_CAPACITY);
        let worker = Arc::clone(&inner);
        // The thread ends when the provider, and with it the sender, is
        // dropped.
        let spawned = std::thread::Builder::new()
            .name("served-icons".into())
            .spawn(move || {
                for key in queue {
                    worker.look_up(key);
                }
            });
        if let Err(e) = spawned {
            tracing::error!("cannot start the served icon thread: {e}");
        }
        Self { inner, lookups }
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resolves the application `app` at the reference edge and records its
    /// id, or the miss.
    fn look_up(&self, app: IconKey) {
        let found = self.resolve(&app, REFERENCE_EDGE);
        let now = Instant::now();
        let Some(found) = found else {
            self.state().apps.finish(app, None, now);
            return;
        };
        let id = content_id(&found.rgba);
        // An upscaled reference rendering is used for the id but not served.
        let png = (served_edge(found.source_edge, REFERENCE_EDGE) == REFERENCE_EDGE)
            .then(|| encode_png(&found.rgba))
            .flatten()
            .filter(|png| png.len() <= MAX_ICON_PNG_BYTES);
        tracing::debug!(app = %app.name, %id, "served icon id");
        {
            let mut state = self.state();
            state.apps.finish(app.clone(), Some(id.clone()), now);
            state.ids.insert(id.clone(), app, 1);
            if let Some(png) = png {
                let image = IconImage {
                    size: REFERENCE_EDGE,
                    png: png.into(),
                };
                let weight = image.png.len();
                state.pngs.insert((id, REFERENCE_EDGE), image, weight);
            }
        }
        // An error only means the server is not listening.
        let _ = self.changed.send(());
    }

    /// The application's icon at `size`, as the resolver finds it now. A
    /// panicking lookup counts as a miss.
    fn resolve(&self, app: &IconKey, size: u32) -> Option<Found> {
        let key = IconKey {
            size,
            ..app.clone()
        };
        std::panic::catch_unwind(AssertUnwindSafe(|| self.resolver.resolve_found(&key)))
            .unwrap_or_else(|_| {
                tracing::warn!(app = %key.name, "icon lookup panicked");
                None
            })
    }

    /// Renders `app` for a request of `size` pixels.
    fn render(&self, app: &IconKey, size: u32) -> Option<IconImage> {
        let mut found = self.resolve(app, size)?;
        let edge = served_edge(found.source_edge, size);
        if edge < size {
            found = self.resolve(app, edge)?;
        }
        let png = encode_png(&found.rgba)?;
        if png.len() > MAX_ICON_PNG_BYTES {
            tracing::debug!(app = %app.name, size, bytes = png.len(), "icon PNG too large");
            return None;
        }
        Some(IconImage {
            size: found.rgba.width,
            png: png.into(),
        })
    }
}

impl IconProvider for ResolverIcons {
    fn icon_id(&self, session: &Session) -> Option<IconId> {
        let app = IconKey::new(session, 0);
        let now = Instant::now();
        let (id, look_up) = {
            let mut state = self.inner.state();
            let look_up = state.apps.request(&app, now);
            let id = match state.apps.get(&app) {
                Some(Slot::Ready(Some(id))) => Some(id.clone()),
                _ => None,
            };
            if let Some(id) = &id {
                // Renders the id from a running application, whose lookup
                // can depend on its process (Steam games on Linux).
                state.ids.insert(id.clone(), app.clone(), 1);
            }
            (id, look_up)
        };
        if look_up && let Err(e) = self.lookups.try_send(app) {
            let app = match e {
                mpsc::TrySendError::Full(app) | mpsc::TrySendError::Disconnected(app) => app,
            };
            self.inner.state().apps.finish(app, None, now);
        }
        id
    }

    fn icon_png(&self, id: &IconId, size: u32) -> Option<IconImage> {
        let cache_key = (id.clone(), size);
        let app = {
            let mut state = self.inner.state();
            if let Some(image) = state.pngs.get(&cache_key) {
                return Some(image.clone());
            }
            state.ids.get(id)?.clone()
        };
        let image = self.inner.render(&app, size)?;
        let weight = image.png.len();
        self.inner
            .state()
            .pngs
            .insert(cache_key, image.clone(), weight);
        Some(image)
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.inner.changed.subscribe()
    }
}

/// The id of an image: the first [`ID_LEN`] hex digits of the SHA-256 of
/// its size and straight RGBA pixels.
fn content_id(rgba: &Rgba) -> IconId {
    let mut hash = Sha256::new();
    hash.update(rgba.width.to_be_bytes());
    hash.update(rgba.height.to_be_bytes());
    hash.update(&rgba.pixels);
    let hex: String = hash
        .finalize()
        .iter()
        .flat_map(|b| [b >> 4, b & 0xf])
        .take(ID_LEN)
        .map(|nibble| char::from_digit(u32::from(nibble), 16).expect("a nibble"))
        .collect();
    IconId::new(hex).expect("lower-case hex is a valid icon id")
}

/// The edge to draw a request for `size` at: `size`, except that a raster
/// source of `source_edge` pixels is scaled up at most [`MAX_UPSCALE`]
/// times (and never below the protocol's minimum size).
fn served_edge(source_edge: Option<u32>, size: u32) -> u32 {
    match source_edge {
        Some(edge) => size.min(edge.saturating_mul(MAX_UPSCALE).max(ICON_SIZE_MIN)),
        None => size,
    }
}
