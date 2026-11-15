//! Runs the server with the mock audio backend so the phone apps can be
//! pointed at any machine.
//!
//! ```text
//! cargo run -p pcvc-server --example mock_server -- [--port N] [--no-mdns] [--no-icons] [--no-detail]
//! ```
//!
//! Icons (the v8 `icons` feature) are on by default and come from a demo
//! provider: a coloured rounded square per app, rendered at the requested
//! size. Like a real provider it resolves ids in the background, so a session
//! the provider has not seen before appears without an `iconId` and gains one
//! about half a second later. Sessions on the default device at startup are
//! resolved before anyone can connect; switching the default device to the
//! headphones or headset shows the delayed path.
//!
//! Session details (the v8 `detail` feature) are on by default too. The
//! speakers have three "Firefox" sessions: the demo's own without a detail,
//! and two tabs with their title as detail, one of which moves on to another
//! title every few seconds. `--no-detail` leaves details out; with
//! `--no-icons` as well the server speaks plain v7.
//!
//! Logging follows `RUST_LOG` (default `info`).

mod common;

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pcvc_audio::{AudioBackend, MockBackend, Session};
use pcvc_server::{IconId, IconImage, IconProvider};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> ExitCode {
    let (mut config, flags) = match common::init("mock_server", &["--no-icons", "--no-detail"]) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    if !flags.contains(&"--no-icons") {
        config = config.with_icons(Arc::new(DemoIcons::default()));
    }
    config = config.with_details(!flags.contains(&"--no-detail"));
    let backend = Arc::new(MockBackend::demo());
    let Some(speakers) = backend
        .snapshot()
        .ok()
        .and_then(|s| s.default_device)
        .map(|d| d.id)
    else {
        eprintln!("the demo has no default device");
        return ExitCode::FAILURE;
    };
    let tab = |app: &str, title: &str, volume: f32| {
        Session::new(MockBackend::session_id(&speakers, app), "Firefox", volume).with_detail(title)
    };
    for session in [
        tab("firefox-radio", "Radio Paradise - Main Mix", 70.0),
        tab("firefox-video", TAB_TITLES[0], 90.0),
    ] {
        backend
            .add_session(&speakers, session)
            .expect("demo tabs are valid");
    }
    tokio::spawn(browse(Arc::clone(&backend), speakers));
    common::serve(backend, config, "mock server").await
}

/// Titles the demo video tab goes through.
const TAB_TITLES: [&str; 3] = [
    "Big Buck Bunny - YouTube",
    "Sintel - YouTube",
    "Tears of Steel - YouTube",
];

/// How long the demo video tab keeps each title.
const TAB_PERIOD: Duration = Duration::from_secs(5);

/// Moves the video tab on to the next title every [`TAB_PERIOD`], keeping
/// its volume and mute. Skipped while the speakers are not the default
/// device, since the mock only reports the default device's sessions.
async fn browse(backend: Arc<MockBackend>, speakers: String) {
    let id = MockBackend::session_id(&speakers, "firefox-video");
    let mut interval = tokio::time::interval(TAB_PERIOD);
    interval.tick().await;
    for title in TAB_TITLES.iter().cycle().skip(1) {
        interval.tick().await;
        let video = backend
            .snapshot()
            .ok()
            .and_then(|s| s.default_device)
            .and_then(|d| d.sessions.into_iter().find(|s| s.id == id));
        if let Some(video) = video {
            let _ = backend.add_session(&speakers, video.with_detail(*title));
        }
    }
}

/// Pretend lookup time before a session's icon id becomes known.
const RESOLVE_DELAY: Duration = Duration::from_millis(500);

/// Prefix of demo icon ids, followed by the colour as six hex digits. The
/// image is a function of the colour alone, so this is content-derived: apps
/// that happen to share a colour share an id. The version digit changes
/// whenever the drawing does.
const ID_PREFIX: &str = "demo1";

/// Generated icons: one colour per app name, drawn at any size.
struct DemoIcons {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    changed: broadcast::Sender<()>,
}

#[derive(Default)]
struct State {
    /// App name to icon id, once resolved.
    ids: HashMap<String, IconId>,
    /// Names being resolved.
    pending: HashSet<String>,
    /// Rendered PNGs by id and edge length.
    pngs: HashMap<(IconId, u32), Arc<[u8]>>,
}

impl Default for DemoIcons {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::default(),
                changed: broadcast::Sender::new(16),
            }),
        }
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn resolve(&self, name: &str) {
        {
            let mut state = self.state();
            state
                .ids
                .insert(name.to_owned(), icon_id_for(colour_for(name)));
            state.pending.remove(name);
        }
        let _ = self.changed.send(());
    }
}

impl IconProvider for DemoIcons {
    fn icon_id(&self, session: &Session) -> Option<IconId> {
        let mut state = self.inner.state();
        if let Some(id) = state.ids.get(&session.name) {
            return Some(id.clone());
        }
        if state.pending.insert(session.name.clone()) {
            let inner = Arc::clone(&self.inner);
            let name = session.name.clone();
            std::thread::spawn(move || {
                std::thread::sleep(RESOLVE_DELAY);
                inner.resolve(&name);
            });
        }
        None
    }

    fn icon_png(&self, id: &IconId, size: u32) -> Option<IconImage> {
        let key = (id.clone(), size);
        if let Some(png) = self.inner.state().pngs.get(&key) {
            return Some(IconImage {
                size,
                png: Arc::clone(png),
            });
        }
        let png: Arc<[u8]> = render(colour_of(id)?, size).into();
        self.inner.state().pngs.insert(key, Arc::clone(&png));
        Some(IconImage { size, png })
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.inner.changed.subscribe()
    }
}

fn icon_id_for([r, g, b]: [u8; 3]) -> IconId {
    IconId::new(format!("{ID_PREFIX}{r:02x}{g:02x}{b:02x}")).expect("a valid icon id")
}

/// The colour a demo id encodes, if it is one.
fn colour_of(id: &IconId) -> Option<[u8; 3]> {
    let hex = id.as_str().strip_prefix(ID_PREFIX)?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// FNV-1a, 64 bit.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

/// A saturated colour picked by hashing the app name.
fn colour_for(name: &str) -> [u8; 3] {
    let hue = (fnv1a(name.as_bytes()) % 360) as f32;
    let (s, v) = (0.65, 0.85);
    let c = v * s;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let channel = |f: f32| ((f + m) * 255.0).round() as u8;
    [channel(r), channel(g), channel(b)]
}

/// An RGBA PNG of `size` square pixels: a rounded square in `colour` on a
/// transparent background with a white disc in the middle, antialiased by
/// 4x4 supersampling.
fn render(colour: [u8; 3], size: u32) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    let edge = size as f32;
    let radius = edge * 0.22;
    let disc = edge * 0.2;
    let centre = edge / 2.0;
    let in_square = |x: f32, y: f32| {
        let dx = (x - centre).abs() - (centre - radius);
        let dy = (y - centre).abs() - (centre - radius);
        dx.max(0.0).hypot(dy.max(0.0)) <= radius
    };
    let in_disc = |x: f32, y: f32| (x - centre).hypot(y - centre) <= disc;

    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut square, mut white) = (0u32, 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    if in_square(x, y) {
                        square += 1;
                        white += u32::from(in_disc(x, y));
                    }
                }
            }
            let total = SAMPLES * SAMPLES;
            let mix = |c: u8| {
                (u32::from(c) * (square - white) + 255 * white)
                    .checked_div(square)
                    .unwrap_or(0) as u8
            };
            pixels.extend_from_slice(&[
                mix(colour[0]),
                mix(colour[1]),
                mix(colour[2]),
                (255 * square / total) as u8,
            ]);
        }
    }

    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("writing to a Vec");
    writer.write_image_data(&pixels).expect("writing to a Vec");
    writer.finish().expect("writing to a Vec");
    png
}
