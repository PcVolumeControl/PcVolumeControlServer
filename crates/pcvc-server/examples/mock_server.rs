//! Runs the server with the mock audio backend so the phone apps can be
//! pointed at any machine.
//!
//! ```text
//! cargo run -p pcvc-server --example mock_server -- [--port N] [--no-mdns] [--no-icons]
//! ```
//!
//! Icons (the v8 `icons` feature) are on by default and come from a demo
//! provider: a coloured rounded square per app, rendered at the requested
//! size. Like a real provider it resolves ids in the background, so each
//! session first appears without an `iconId` and gains one about half a
//! second later. `--no-icons` serves plain v7.
//!
//! Logging follows `RUST_LOG` (default `info`).

mod common;

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pcvc_audio::{MockBackend, Session};
use pcvc_server::{IconId, IconImage, IconProvider};
use tokio::sync::broadcast;

#[tokio::main]
async fn main() -> ExitCode {
    let (mut config, flags) = match common::init("mock_server", &["--no-icons"]) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    if !flags.contains(&"--no-icons") {
        config = config.with_icons(Arc::new(DemoIcons::default()));
    }
    common::serve(Arc::new(MockBackend::demo()), config, "mock server").await
}

/// Pretend lookup time before a session's icon id becomes known.
const RESOLVE_DELAY: Duration = Duration::from_millis(500);

/// Generated icons: one colour per app name, drawn at any size.
struct DemoIcons {
    inner: Arc<Inner>,
}

struct Inner {
    /// App name to icon id, once resolved.
    ids: Mutex<HashMap<String, IconId>>,
    /// Names being resolved.
    pending: Mutex<HashSet<String>>,
    colours: Mutex<HashMap<IconId, [u8; 3]>>,
    changed: broadcast::Sender<()>,
}

impl Default for DemoIcons {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                ids: Mutex::default(),
                pending: Mutex::default(),
                colours: Mutex::default(),
                changed: broadcast::Sender::new(16),
            }),
        }
    }
}

impl Inner {
    fn resolve(&self, name: &str) {
        let colour = colour_for(name);
        let id = icon_id_for(colour);
        lock(&self.colours).insert(id.clone(), colour);
        lock(&self.ids).insert(name.to_owned(), id);
        lock(&self.pending).remove(name);
        let _ = self.changed.send(());
    }
}

impl IconProvider for DemoIcons {
    fn icon_id(&self, session: &Session) -> Option<IconId> {
        if let Some(id) = lock(&self.inner.ids).get(&session.name) {
            return Some(id.clone());
        }
        if lock(&self.inner.pending).insert(session.name.clone()) {
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
        let colour = *lock(&self.inner.colours).get(id)?;
        Some(IconImage {
            size,
            png: render(colour, size).into(),
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.inner.changed.subscribe()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// FNV-1a, 128 bit.
fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    bytes.iter().fold(OFFSET, |hash, &b| {
        (hash ^ u128::from(b)).wrapping_mul(PRIME)
    })
}

/// The image is a function of the colour alone, so the id is derived from
/// it: apps that happen to share a colour share an icon id. The version
/// tag changes whenever the drawing does.
fn icon_id_for(colour: [u8; 3]) -> IconId {
    // The fixed suffix diffuses the colour bytes through the whole hash.
    let mut content = colour.to_vec();
    content.extend_from_slice(b":pcvc-demo-icon-v1");
    IconId::new(format!("{:032x}", fnv1a_128(&content))).expect("hex is a valid icon id")
}

/// A saturated colour picked by hashing the app name.
fn colour_for(name: &str) -> [u8; 3] {
    let hue = (fnv1a_128(name.as_bytes()) % 360) as f32;
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
