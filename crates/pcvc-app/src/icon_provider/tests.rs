use std::collections::HashMap;
use std::sync::Condvar;
use std::time::Duration;

use pcvc_audio::AppHint;
use resvg::tiny_skia::Pixmap;
use tokio::sync::broadcast::error::TryRecvError;

use super::*;
use crate::app_icons::NoIcons;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A colour and, for a raster icon, its source edge.
type FakeIcon = ([u8; 4], Option<u32>);

/// Knows icons by declared icon name: a solid colour drawn at any size,
/// from a vector source or from a raster of the given edge. The icon name
/// `panic` panics. Lookups wait while the gate is closed, and lookups at
/// the sizes in `fail_at` find nothing.
pub(super) struct FakeResolver {
    icons: Mutex<HashMap<String, FakeIcon>>,
    calls: Mutex<Vec<IconKey>>,
    gate: (Mutex<bool>, Condvar),
    fail_at: Mutex<Vec<u32>>,
}

impl FakeResolver {
    pub(super) fn new(icons: &[(&str, [u8; 4], Option<u32>)]) -> Arc<Self> {
        Arc::new(Self {
            icons: Mutex::new(
                icons
                    .iter()
                    .map(|(name, colour, source)| ((*name).to_owned(), (*colour, *source)))
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
            gate: (Mutex::new(true), Condvar::new()),
            fail_at: Mutex::new(Vec::new()),
        })
    }

    fn set_icon(&self, name: &str, icon: FakeIcon) {
        self.icons.lock().unwrap().insert(name.to_owned(), icon);
    }

    fn calls(&self) -> Vec<IconKey> {
        self.calls.lock().unwrap().clone()
    }

    fn set_gate(&self, open: bool) {
        *self.gate.0.lock().unwrap() = open;
        self.gate.1.notify_all();
    }
}

impl Resolver for FakeResolver {
    fn resolve_found(&self, key: &IconKey) -> Option<Found> {
        self.calls.lock().unwrap().push(key.clone());
        let (open, wake) = &self.gate;
        drop(
            wake.wait_while(open.lock().unwrap(), |open| !*open)
                .unwrap(),
        );
        if self.fail_at.lock().unwrap().contains(&key.size) {
            return None;
        }
        let name = key.icon_name.as_deref()?;
        assert_ne!(name, "panic", "a resolver bug");
        let (colour, source_edge) = *self.icons.lock().unwrap().get(name)?;
        Some(Found {
            rgba: solid(colour, key.size),
            source_edge,
        })
    }
}

pub(super) fn solid(colour: [u8; 4], size: u32) -> Rgba {
    Rgba {
        pixels: colour.repeat((size * size) as usize),
        width: size,
        height: size,
    }
}

pub(super) fn session(name: &str, icon: &str) -> Session {
    Session::new(format!("dev|{name}"), name, 50.0).with_app(AppHint {
        icon_name: Some(Arc::from(icon)),
        ..AppHint::default()
    })
}

fn provider(resolver: &Arc<FakeResolver>) -> ResolverIcons {
    ResolverIcons::new(Arc::clone(resolver) as Arc<dyn Resolver>).unwrap()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Waits for `session`'s id, which needs a notification first.
fn resolved_id(icons: &ResolverIcons, session: &Session) -> IconId {
    let mut changed = icons.subscribe();
    if let Some(id) = icons.icon_id(session) {
        return id;
    }
    wait_until("a notification", || !changed.is_empty());
    let _ = changed.try_recv();
    icons
        .icon_id(session)
        .expect("an id after the notification")
}

/// Notifications received so far, after the background thread has been
/// given a moment to send any more.
fn drain(changed: &mut broadcast::Receiver<()>) -> usize {
    std::thread::sleep(Duration::from_millis(50));
    let mut count = 0;
    loop {
        match changed.try_recv() {
            Ok(()) => count += 1,
            Err(TryRecvError::Lagged(n)) => count += n as usize,
            Err(TryRecvError::Empty | TryRecvError::Closed) => return count,
        }
    }
}

fn decode(image: &IconImage) -> Pixmap {
    let pixmap = Pixmap::decode_png(&image.png).expect("a PNG");
    assert_eq!((pixmap.width(), pixmap.height()), (image.size, image.size));
    pixmap
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

#[test]
fn miss_then_notify_then_hit() {
    let resolver = FakeResolver::new(&[("red", RED, None)]);
    let icons = provider(&resolver);
    let changed = icons.subscribe();
    let firefox = session("Firefox", "red");
    assert_eq!(icons.icon_id(&firefox), None, "unknown at first");
    wait_until("a notification", || !changed.is_empty());
    assert_eq!(
        icons.icon_id(&firefox),
        Some(content_id(&solid(RED, REFERENCE_EDGE)))
    );
    // One lookup at the reference edge, for the session's identity.
    let calls = resolver.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], IconKey::new(&firefox, REFERENCE_EDGE));
}

#[test]
fn one_notification_per_resolved_icon() {
    let resolver = FakeResolver::new(&[("red", RED, None), ("blue", BLUE, None)]);
    let icons = provider(&resolver);
    let mut changed = icons.subscribe();
    let sessions = [
        session("A", "red"),
        session("B", "blue"),
        session("C", "unknown"),
    ];
    for _ in 0..5 {
        for s in &sessions {
            icons.icon_id(s);
        }
    }
    wait_until("three lookups", || resolver.calls().len() == 3);
    assert_eq!(drain(&mut changed), 2, "one per app with an icon");
    for _ in 0..5 {
        for s in &sessions {
            icons.icon_id(s);
        }
    }
    assert_eq!(drain(&mut changed), 0, "nothing new");
    assert_eq!(resolver.calls().len(), 3, "each app looked up once");
}

#[test]
fn misses_are_cached() {
    let resolver = FakeResolver::new(&[]);
    let icons = provider(&resolver);
    let mut changed = icons.subscribe();
    let unknown = session("Unknown", "nothing");
    assert_eq!(icons.icon_id(&unknown), None);
    wait_until("the lookup", || resolver.calls().len() == 1);
    // Let the miss land, then rebuild a few times.
    std::thread::sleep(Duration::from_millis(50));
    for _ in 0..10 {
        assert_eq!(icons.icon_id(&unknown), None);
    }
    assert_eq!(drain(&mut changed), 0);
    assert_eq!(resolver.calls().len(), 1, "not retried at once");
}

#[test]
fn panicking_lookups_are_misses() {
    let resolver = FakeResolver::new(&[("red", RED, None)]);
    let icons = provider(&resolver);
    assert_eq!(icons.icon_id(&session("Bad", "panic")), None);
    wait_until("the lookup", || resolver.calls().len() == 1);
    // The thread survived.
    resolved_id(&icons, &session("Good", "red"));
}

#[test]
fn ids_are_content_derived() {
    let red = content_id(&solid(RED, 64));
    assert_eq!(red, content_id(&solid(RED, 64)));
    assert_ne!(red, content_id(&solid(BLUE, 64)));
    assert_ne!(red, content_id(&solid(RED, 32)));
    let mut almost = solid(RED, 64);
    almost.pixels[100] ^= 1;
    assert_ne!(red, content_id(&almost));
    for id in [&red, &content_id(&almost)] {
        assert_eq!(id.as_str().len(), ID_LEN);
        assert!(
            id.as_str()
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }
    // Fixed value: a change here changes every client's cached ids.
    assert_eq!(
        content_id(&solid([0, 0, 0, 0], 1)).as_str(),
        "82f794f57c8a954448672b791dddf7d8"
    );
}

#[test]
fn ids_survive_restarts_and_are_shared() {
    let resolver = FakeResolver::new(&[("red", RED, None), ("blue", BLUE, None)]);
    let first = resolved_id(&provider(&resolver), &session("Firefox", "red"));
    // A new provider, as after a restart, and other sessions of the app.
    let icons = provider(&resolver);
    assert_eq!(resolved_id(&icons, &session("Firefox", "red")), first);
    let mut other_tab = session("Firefox", "red");
    other_tab.id = "dev|Firefox#2".into();
    assert_eq!(icons.icon_id(&other_tab), Some(first.clone()));
    // Another app with the same icon shares the id.
    assert_eq!(resolved_id(&icons, &session("Browser", "red")), first);
    assert_ne!(resolved_id(&icons, &session("Other", "blue")), first);
}

#[test]
fn renders_requested_sizes_without_upscaling_rasters() {
    let resolver = FakeResolver::new(&[
        ("vector", RED, None),
        ("raster32", GREEN, Some(32)),
        ("raster4", BLUE, Some(4)),
        ("raster512", WHITE, Some(512)),
    ]);
    let icons = provider(&resolver);
    let id = |icon: &str| resolved_id(&icons, &session(icon, icon));
    let (vector, raster32, raster4, raster512) =
        (id("vector"), id("raster32"), id("raster4"), id("raster512"));
    let edge = |id: &IconId, size: u32| {
        let image = icons.icon_png(id, size).expect("an image");
        decode(&image);
        image.size
    };
    for size in [16, 48, 64, 100, 256] {
        assert_eq!(edge(&vector, size), size, "vector at {size}");
        assert_eq!(edge(&raster512, size), size, "large raster at {size}");
    }
    assert_eq!(edge(&raster32, 48), 48);
    assert_eq!(edge(&raster32, 64), 64);
    assert_eq!(edge(&raster32, 256), 64, "at most twice the source");
    assert_eq!(edge(&raster4, 16), 16);
    assert_eq!(edge(&raster4, 256), 16, "never below the minimum size");
    let pixmap = decode(&icons.icon_png(&vector, 20).unwrap());
    let p = pixmap.pixel(10, 10).unwrap();
    assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (255, 0, 0, 255));

    let unknown = IconId::new("0123456789abcdef0123456789abcdef").unwrap();
    assert_eq!(icons.icon_png(&unknown, 64), None);
}

#[test]
fn renders_are_cached() {
    let resolver = FakeResolver::new(&[("vector", RED, None), ("raster32", GREEN, Some(32))]);
    let icons = provider(&resolver);
    let vector = resolved_id(&icons, &session("V", "vector"));
    let lookups = resolver.calls().len();
    // The reference rendering answers the default size.
    let image = icons.icon_png(&vector, REFERENCE_EDGE).unwrap();
    assert_eq!(image.size, REFERENCE_EDGE);
    assert_eq!(resolver.calls().len(), lookups);
    // Another size: a check at the reference edge, then the render.
    icons.icon_png(&vector, 128).unwrap();
    let sizes: Vec<u32> = resolver.calls()[lookups..].iter().map(|k| k.size).collect();
    assert_eq!(sizes, [REFERENCE_EDGE, 128]);
    assert_eq!(icons.icon_png(&vector, 128).unwrap().size, 128);
    assert_eq!(resolver.calls().len(), lookups + 2);

    // A capped request resolves again at the capped edge, once, and later
    // requests above the cap share that rendering.
    let raster = resolved_id(&icons, &session("R", "raster32"));
    let lookups = resolver.calls().len();
    assert_eq!(icons.icon_png(&raster, 200).unwrap().size, 64);
    let sizes: Vec<u32> = resolver.calls()[lookups..].iter().map(|k| k.size).collect();
    assert_eq!(sizes, [REFERENCE_EDGE, 200, 64]);
    for size in [200, 256, 64] {
        assert_eq!(icons.icon_png(&raster, size).unwrap().size, 64);
    }
    assert_eq!(resolver.calls().len(), lookups + 3);
    assert_eq!(icons.inner.state().pngs.len(), 3, "one PNG per served edge");
}

#[test]
fn capped_renders_fall_back_to_scaling_down() {
    let resolver = FakeResolver::new(&[("raster40", GREEN, Some(40))]);
    let icons = provider(&resolver);
    let id = resolved_id(&icons, &session("R", "raster40"));
    resolver.fail_at.lock().unwrap().push(80);
    let image = icons.icon_png(&id, 200).unwrap();
    assert_eq!(image.size, 80);
    let p = decode(&image).pixel(40, 40).unwrap();
    assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (0, 255, 0, 255));
}

#[test]
fn changed_icons_get_new_ids() {
    let resolver = FakeResolver::new(&[("app", RED, None)]);
    let icons = provider(&resolver);
    let app = session("App", "app");
    let old = resolved_id(&icons, &app);
    // The theme changes: the old id is not rendered from the new artwork,
    // and the app moves to a new id.
    resolver.set_icon("app", (BLUE, None));
    let mut changed = icons.subscribe();
    assert_eq!(icons.icon_png(&old, 128), None);
    assert_eq!(drain(&mut changed), 1);
    let new = icons.icon_id(&app).unwrap();
    assert_eq!(new, content_id(&solid(BLUE, REFERENCE_EDGE)));
    let calls = resolver.calls().len();
    assert_eq!(icons.icon_png(&old, 128), None);
    assert_eq!(
        resolver.calls().len(),
        calls,
        "the app is no longer tried for the old id"
    );
    assert_eq!(icons.icon_png(&new, 128).unwrap().size, 128);
    // What was already rendered for the old id stays valid for it.
    assert!(icons.icon_png(&old, REFERENCE_EDGE).is_some());
}

#[test]
fn shared_ids_render_from_the_first_app_seen() {
    // Identical at the reference edge, different sources.
    let resolver = FakeResolver::new(&[("svg", RED, None), ("png", RED, Some(32))]);
    let icons = provider(&resolver);
    let vector = session("Vector", "svg");
    let raster = session("Raster", "png");
    let id = resolved_id(&icons, &vector);
    assert_eq!(resolved_id(&icons, &raster), id);
    for _ in 0..3 {
        icons.icon_id(&raster);
    }
    assert_eq!(icons.icon_png(&id, 256).unwrap().size, 256);
}

#[test]
fn fallback_apps_do_not_cap_the_id() {
    let resolver = FakeResolver::new(&[("svg", RED, None), ("png", RED, Some(32))]);
    let icons = provider(&resolver);
    let id = resolved_id(&icons, &session("Vector", "svg"));
    assert_eq!(resolved_id(&icons, &session("Raster", "png")), id);
    // The vector app cannot be resolved for a moment: the raster one
    // answers, capped.
    resolver.icons.lock().unwrap().remove("svg");
    assert_eq!(icons.icon_png(&id, 256).unwrap().size, 64);
    resolver.set_icon("svg", (RED, None));
    assert_eq!(icons.icon_png(&id, 256).unwrap().size, 256);
}

#[test]
fn full_queues_drop_lookups_without_recording_misses() {
    let resolver = FakeResolver::new(&[("red", RED, None)]);
    let icons = provider(&resolver);
    let changed = icons.subscribe();
    resolver.set_gate(false);
    // Misses, which notify nobody, then one app with an icon. Each miss has
    // its own icon name: the session name is not part of the key on Windows.
    let mut sessions: Vec<Session> = (0..QUEUE_CAPACITY + 1)
        .map(|i| session(&format!("App{i}"), &format!("unknown{i}")))
        .collect();
    sessions.push(session("Last", "red"));
    for s in &sessions {
        assert_eq!(icons.icon_id(s), None);
    }
    let last = IconKey::new(sessions.last().unwrap(), 0);
    assert!(
        icons.inner.state().apps.get(&last).is_none(),
        "dropped, so the next rebuild asks again"
    );
    resolver.set_gate(true);
    wait_until("a rebuild for the dropped lookup", || !changed.is_empty());
    wait_until("the dropped lookup", || {
        icons.icon_id(sessions.last().unwrap()).is_some()
    });
}

#[test]
fn icon_id_does_not_wait_for_lookups() {
    let resolver = FakeResolver::new(&[("red", RED, None), ("blue", BLUE, None)]);
    let icons = provider(&resolver);
    let red = resolved_id(&icons, &session("Red", "red"));
    resolver.set_gate(false);
    let blue = session("Blue", "blue");
    assert_eq!(icons.icon_id(&blue), None);
    wait_until("the blocked lookup", || {
        resolver
            .calls()
            .last()
            .is_some_and(|k| k.icon_name.as_deref() == Some("blue"))
    });
    // The lookup is blocked inside the resolver; the provider still answers.
    let started = Instant::now();
    assert_eq!(icons.icon_id(&session("Red", "red")), Some(red.clone()));
    assert_eq!(icons.icon_id(&blue), None);
    assert!(icons.icon_png(&red, REFERENCE_EDGE).is_some());
    assert!(started.elapsed() < Duration::from_secs(1));
    resolver.set_gate(true);
    resolved_id(&icons, &blue);
}

#[test]
fn maps_are_bounded() {
    let colours: Vec<(String, [u8; 4])> = (0..300u32)
        .map(|i| (format!("icon{i}"), [i as u8, (i >> 8) as u8, 7, 255]))
        .collect();
    let table: Vec<(&str, [u8; 4], Option<u32>)> = colours
        .iter()
        .map(|(name, colour)| (name.as_str(), *colour, None))
        .collect();
    let resolver = FakeResolver::new(&table);
    let icons = provider(&resolver);
    let sessions: Vec<Session> = colours
        .iter()
        .map(|(name, _)| session(name, name))
        .collect();
    let ids: Vec<IconId> = sessions.iter().map(|s| resolved_id(&icons, s)).collect();
    for id in &ids[ids.len() - 20..] {
        for size in [16, 32, 48, 96, 128, 256] {
            icons.icon_png(id, size).unwrap();
        }
    }
    let state = icons.inner.state();
    assert_eq!(state.apps.len(), APP_CAPACITY);
    assert_eq!(state.ids.len(), ID_CAPACITY);
    assert_eq!(state.pngs.len(), PNG_CAPACITY);
    assert!(state.pngs.weight() <= PNG_CACHE_BYTES);
}

#[test]
fn provider_only_when_icons_can_be_served() {
    let fake: Arc<dyn Resolver> = FakeResolver::new(&[]);
    let none: Arc<dyn Resolver> = Arc::new(NoIcons);
    assert!(for_server(true, &fake).is_some());
    assert!(for_server(false, &fake).is_none());
    assert!(for_server(true, &none).is_none());
}

#[test]
fn served_edges() {
    assert_eq!(served_edge(None, 256), 256);
    assert_eq!(served_edge(Some(256), 64), 64);
    assert_eq!(served_edge(Some(48), 64), 64);
    assert_eq!(served_edge(Some(48), 256), 96);
    assert_eq!(served_edge(Some(1), 64), ICON_SIZE_MIN);
    assert_eq!(served_edge(Some(u32::MAX), 128), 128);
}
