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
//! times: larger requests get that edge instead. Each render first checks
//! that the application still draws the requested id at the reference
//! edge (its icon theme or executable may have changed); if not, the
//! application gets its new id and the old one is not rendered from it.

mod lru;
#[cfg(test)]
mod server_tests;
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

use crate::app_icons::{Found, IconCache, IconKey, Resolver, Slot, encode_png, fit_rgba};
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
/// Applications remembered per id to render it from.
const APPS_PER_ID: usize = 4;
/// Rendered PNGs kept, by count and by total size.
const PNG_CAPACITY: usize = 256;
const PNG_CACHE_BYTES: usize = 16 * 1024 * 1024;
/// Lookups waiting for the background thread. A lookup that does not fit
/// is dropped and requested again on a later state rebuild.
const QUEUE_CAPACITY: usize = 256;

/// The icon provider for the server, or `None` when icons are not served:
/// `serve_icons` is off in the config, there is no resolver on this
/// platform, or the lookup thread cannot start. Without a provider the
/// server speaks plain v7.
pub fn for_server(
    serve_icons: bool,
    resolver: &Arc<dyn Resolver>,
) -> Option<Arc<dyn IconProvider>> {
    if !(serve_icons && resolver.finds_icons()) {
        return None;
    }
    match ResolverIcons::new(Arc::clone(resolver)) {
        Ok(icons) => Some(Arc::new(icons)),
        Err(e) => {
            tracing::error!("cannot start the served icon thread: {e}; not serving icons");
            None
        }
    }
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
    ids: Lru<IconId, IdEntry>,
    /// By id and served edge.
    pngs: Lru<(IconId, u32), IconImage>,
    /// Ticks whenever an application is seen with an id, to tell which
    /// application of an id was seen least recently.
    clock: u64,
}

impl State {
    fn saw(&mut self, id: &IconId, app: &IconKey) {
        self.clock += 1;
        let clock = self.clock;
        match self.ids.get_mut(id) {
            Some(entry) => entry.saw(app, clock),
            None => {
                let entry = IdEntry {
                    apps: vec![(app.clone(), clock)],
                    max_edge: None,
                };
                self.ids.insert(id.clone(), entry, 1);
            }
        }
    }
}

struct IdEntry {
    /// Applications seen with this id, in the order first seen, with the
    /// clock when last seen. Renders come from the first that still draws
    /// the id, so they do not depend on which was seen last.
    apps: Vec<(IconKey, u64)>,
    /// Served edges are capped here once a request was answered smaller
    /// than asked (a small raster source).
    max_edge: Option<u32>,
}

impl IdEntry {
    /// Records `app` as seen at `clock`. A new application replaces the
    /// least recently seen one when the entry is full, so an id stays
    /// renderable from running applications (on Linux a Steam game's
    /// lookup depends on its process).
    fn saw(&mut self, app: &IconKey, clock: u64) {
        if let Some(seen) = self.apps.iter_mut().find(|(a, _)| a == app) {
            seen.1 = clock;
            return;
        }
        if self.apps.len() >= APPS_PER_ID
            && let Some(oldest) = (0..self.apps.len()).min_by_key(|&i| self.apps[i].1)
        {
            self.apps.remove(oldest);
        }
        self.apps.push((app.clone(), clock));
    }
}

/// The outcome of rendering an id from one application.
enum Render {
    Image(IconImage),
    /// The application now draws another id.
    Moved,
    Failed,
}

impl ResolverIcons {
    pub fn new(resolver: Arc<dyn Resolver>) -> std::io::Result<Self> {
        let inner = Arc::new(Inner {
            resolver,
            state: Mutex::new(State {
                apps: IconCache::new(APP_CAPACITY),
                ids: Lru::new(ID_CAPACITY, usize::MAX),
                pngs: Lru::new(PNG_CAPACITY, PNG_CACHE_BYTES),
                clock: 0,
            }),
            changed: broadcast::Sender::new(16),
        });
        let (lookups, queue) = mpsc::sync_channel::<IconKey>(QUEUE_CAPACITY);
        let worker = Arc::clone(&inner);
        // The thread ends when the provider, and with it the sender, is
        // dropped.
        std::thread::Builder::new()
            .name("served-icons".into())
            .spawn(move || {
                for app in queue {
                    let found = worker.resolve(&app, REFERENCE_EDGE);
                    match found {
                        Some(found) => worker.record(app, found),
                        None => worker.state().apps.finish(app, None, Instant::now()),
                    }
                }
            })?;
        Ok(Self { inner, lookups })
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `found`, the application's reference rendering, as its icon
    /// and tells the server.
    fn record(&self, app: IconKey, found: Found) {
        let id = content_id(&found.rgba);
        // An upscaled reference rendering is used for the id but not served.
        let png = (served_edge(found.source_edge, REFERENCE_EDGE) == REFERENCE_EDGE)
            .then(|| encode_png(&found.rgba))
            .flatten()
            .filter(|png| png.len() <= MAX_ICON_PNG_BYTES);
        tracing::debug!(app = %app.name, %id, "served icon id");
        {
            let mut guard = self.state();
            let state = &mut *guard;
            if let Some(Slot::Ready(Some(old))) = state.apps.get(&app)
                && *old != id
                && let Some(entry) = state.ids.get_mut(old)
            {
                entry.apps.retain(|(a, _)| *a != app);
            }
            state
                .apps
                .finish(app.clone(), Some(id.clone()), Instant::now());
            state.saw(&id, &app);
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

    /// Renders `id` from `app` for a request of `size` pixels.
    fn render(&self, id: &IconId, app: &IconKey, size: u32) -> Render {
        let Some(reference) = self.resolve(app, REFERENCE_EDGE) else {
            return Render::Failed;
        };
        if content_id(&reference.rgba) != *id {
            self.record(app.clone(), reference);
            return Render::Moved;
        }
        let found = if size == REFERENCE_EDGE {
            reference
        } else {
            match self.resolve(app, size) {
                Some(found) => found,
                None => return Render::Failed,
            }
        };
        let edge = served_edge(found.source_edge, size);
        let rgba = if edge < size {
            // Resolving at the smaller edge can pick a different source, or
            // fail; scaling down what is at hand is the fallback.
            match self.resolve(app, edge) {
                Some(smaller) if served_edge(smaller.source_edge, edge) == edge => smaller.rgba,
                _ => match fit_rgba(&found.rgba, edge) {
                    Some(rgba) => rgba,
                    None => return Render::Failed,
                },
            }
        } else {
            found.rgba
        };
        let Some(png) = encode_png(&rgba) else {
            return Render::Failed;
        };
        if png.len() > MAX_ICON_PNG_BYTES {
            tracing::debug!(app = %app.name, size, bytes = png.len(), "icon PNG too large");
            return Render::Failed;
        }
        Render::Image(IconImage {
            size: rgba.width,
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
                state.saw(id, &app);
            }
            (id, look_up)
        };
        if look_up && let Err(e) = self.lookups.try_send(app) {
            let mut state = self.inner.state();
            match e {
                mpsc::TrySendError::Full(app) => state.apps.forget(&app),
                // The thread is gone, so no lookup will ever finish.
                mpsc::TrySendError::Disconnected(app) => state.apps.finish(app, None, now),
            }
        }
        id
    }

    fn icon_png(&self, id: &IconId, size: u32) -> Option<IconImage> {
        let (apps, edge) = {
            let mut guard = self.inner.state();
            let state = &mut *guard;
            let entry = state.ids.get_mut(id)?;
            let edge = entry.max_edge.map_or(size, |max| size.min(max));
            if let Some(image) = state.pngs.get(&(id.clone(), edge)) {
                return Some(image.clone());
            }
            let apps: Vec<IconKey> = entry.apps.iter().map(|(app, _)| app.clone()).collect();
            (apps, edge)
        };
        for app in apps {
            match self.inner.render(id, &app, edge) {
                Render::Image(image) => {
                    let mut state = self.inner.state();
                    if image.size < edge
                        && let Some(entry) = state.ids.get_mut(id)
                    {
                        entry.max_edge = Some(image.size);
                    }
                    let weight = image.png.len();
                    state
                        .pngs
                        .insert((id.clone(), image.size), image.clone(), weight);
                    return Some(image);
                }
                Render::Moved | Render::Failed => {}
            }
        }
        None
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
