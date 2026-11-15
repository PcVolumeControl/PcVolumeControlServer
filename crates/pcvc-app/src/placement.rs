//! Where the mini mixer opens: beside the tray icon, on the side away from
//! the taskbar, inside the work area of the icon's monitor.
//!
//! The geometry is in physical pixels, as the platforms report it. On
//! Windows the tray click gives the icon's rectangle and the monitor comes
//! from Win32; on X11 the click (or the pointer) gives a point and RandR the
//! monitors. Wayland does not let an application position its own windows,
//! so there the compositor places it.

use iced::{Point, Rectangle, Size};

use crate::style;

/// A rectangle in physical screen pixels.
pub type ScreenRect = Rectangle;

fn right(r: &ScreenRect) -> f32 {
    r.x + r.width
}

fn bottom(r: &ScreenRect) -> f32 {
    r.y + r.height
}

/// A zero-sized rectangle at a point.
pub fn point(x: f32, y: f32) -> ScreenRect {
    Rectangle::new(Point::new(x, y), Size::ZERO)
}

/// The screen edge a taskbar or panel sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// A monitor's bounds and the part of it panels and taskbars leave free.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monitor {
    pub bounds: ScreenRect,
    pub work: ScreenRect,
    /// Its scale factor, when the platform says; otherwise the window's
    /// own is used.
    pub scale: Option<f32>,
}

/// The edge of the taskbar holding a tray icon at `anchor` on `monitor`.
pub fn taskbar_edge(monitor: &Monitor, anchor: ScreenRect) -> Edge {
    let Point { x: cx, y: cy } = anchor.center();
    let (bounds, work) = (monitor.bounds, monitor.work);
    // An icon outside the work area is on the panel that took that strip.
    if cy >= bottom(&work) {
        return Edge::Bottom;
    }
    if cy < work.y {
        return Edge::Top;
    }
    if cx < work.x {
        return Edge::Left;
    }
    if cx >= right(&work) {
        return Edge::Right;
    }
    // Inside it (an auto-hiding taskbar, or the pointer after a menu
    // click): the side a panel takes, else the nearest monitor edge.
    if bottom(&work) < bottom(&bounds) {
        return Edge::Bottom;
    }
    if work.y > bounds.y {
        return Edge::Top;
    }
    if work.x > bounds.x {
        return Edge::Left;
    }
    if right(&work) < right(&bounds) {
        return Edge::Right;
    }
    [
        (bottom(&bounds) - cy, Edge::Bottom),
        (cy - bounds.y, Edge::Top),
        (cx - bounds.x, Edge::Left),
        (right(&bounds) - cx, Edge::Right),
    ]
    .into_iter()
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map_or(Edge::Bottom, |(_, edge)| edge)
}

/// Top-left corner, in physical pixels, of a window of logical `size` at
/// `scale` placed beside `anchor` on the side away from a taskbar on
/// `edge`, centred on the anchor along the taskbar, and kept inside `work`
/// with a margin.
pub fn place(anchor: ScreenRect, work: ScreenRect, size: Size, scale: f32, edge: Edge) -> Point {
    let width = size.width * scale;
    let height = size.height * scale;
    let margin = style::MINI_SCREEN_MARGIN * scale;
    let Point { x: cx, y: cy } = anchor.center();
    let (x, y) = match edge {
        Edge::Bottom => (
            cx - width / 2.0,
            anchor.y.min(bottom(&work)) - margin - height,
        ),
        Edge::Top => (cx - width / 2.0, bottom(&anchor).max(work.y) + margin),
        Edge::Left => (right(&anchor).max(work.x) + margin, cy - height / 2.0),
        Edge::Right => (
            anchor.x.min(right(&work)) - margin - width,
            cy - height / 2.0,
        ),
    };
    Point::new(
        within(x, work.x, work.width, width, margin).round(),
        within(y, work.y, work.height, height, margin).round(),
    )
}

/// `start` of a span of `len` moved inside the range from `from` of
/// `extent`, `margin` from its ends; centred in it when it does not fit.
fn within(start: f32, from: f32, extent: f32, len: f32, margin: f32) -> f32 {
    let (low, high) = (from + margin, from + extent - margin - len);
    if high < low {
        from + ((extent - len) / 2.0).max(0.0)
    } else {
        start.clamp(low, high)
    }
}

/// What the mini mixer is placed against, kept to place it again when its
/// size or scale changes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchoring {
    pub anchor: ScreenRect,
    pub monitor: Monitor,
    pub edge: Edge,
}

impl Anchoring {
    pub fn new(anchor: ScreenRect, monitor: Monitor) -> Self {
        Self {
            anchor,
            monitor,
            edge: taskbar_edge(&monitor, anchor),
        }
    }

    /// The position to give iced for a window of logical `size` whose
    /// scale factor is `window_scale`: the platform turns logical
    /// coordinates into physical ones at the window's current scale, which
    /// differs from the monitor's while the window has not moved there.
    pub fn position(&self, size: Size, window_scale: f32) -> Point {
        let scale = self.monitor.scale.unwrap_or(window_scale);
        let p = place(self.anchor, self.monitor.work, size, scale, self.edge);
        Point::new(p.x / window_scale, p.y / window_scale)
    }
}

/// Whether the platform moves a hidden window as asked (Win32). X11 window
/// managers instead place a window as it maps, honouring only the position
/// it was created with.
pub const MOVES_HIDDEN_WINDOWS: bool = cfg!(target_os = "windows");

/// Where to anchor the mini mixer opened by a tray click at `click` (`None`
/// from the menu), or `None` where the app cannot position windows or the
/// platform does not say. Queries the display server: a few round trips to
/// the X server the app's windows already depend on.
pub fn anchoring(click: Option<ScreenRect>) -> Option<Anchoring> {
    let anchoring = platform::anchoring(click);
    match &anchoring {
        Some(a) => tracing::debug!(
            "mini mixer: anchored to {:?} on the {:?} edge, work area {:?}, monitor scale {:?}",
            a.anchor,
            a.edge,
            a.monitor.work,
            a.monitor.scale
        ),
        None => tracing::debug!("mini mixer: nothing to anchor to; the platform places it"),
    }
    anchoring
}

#[cfg(target_os = "windows")]
mod platform {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITOR_FROM_FLAGS,
        MONITORINFO, MonitorFromPoint,
    };
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    use super::{Anchoring, Edge, Monitor, ScreenRect, bottom, point, right};

    /// Beside the clicked icon, or at the bottom right of the primary
    /// monitor's work area without one.
    pub fn anchoring(click: Option<ScreenRect>) -> Option<Anchoring> {
        match click {
            Some(rect) => {
                let c = rect.center();
                let monitor = monitor_at(c.x as i32, c.y as i32, MONITOR_DEFAULTTONEAREST)?;
                Some(Anchoring::new(rect, monitor))
            }
            None => {
                let monitor = monitor_at(0, 0, MONITOR_DEFAULTTOPRIMARY)?;
                let work = monitor.work;
                Some(Anchoring {
                    anchor: point(right(&work), bottom(&work)),
                    monitor,
                    edge: Edge::Bottom,
                })
            }
        }
    }

    fn monitor_at(x: i32, y: i32, flags: MONITOR_FROM_FLAGS) -> Option<Monitor> {
        // SAFETY: MonitorFromPoint takes the point by value and has no
        // other preconditions.
        let handle = unsafe { MonitorFromPoint(POINT { x, y }, flags) };
        if handle.is_null() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        };
        // SAFETY: `handle` is a monitor handle just returned by Win32 and
        // `info` is a MONITORINFO with its size set, as required.
        if unsafe { GetMonitorInfoW(handle, &mut info) } == 0 {
            return None;
        }
        let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
        // SAFETY: `handle` is valid and both out pointers point to live
        // u32s.
        let result = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        let scale = (result == 0 && dpi_x > 0).then(|| dpi_x as f32 / 96.0);
        Some(Monitor {
            bounds: rect(info.rcMonitor),
            work: rect(info.rcWork),
            scale,
        })
    }

    fn rect(r: RECT) -> ScreenRect {
        ScreenRect {
            x: r.left as f32,
            y: r.top as f32,
            width: (r.right - r.left) as f32,
            height: (r.bottom - r.top) as f32,
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use iced::Rectangle;
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};

    use super::{Anchoring, Monitor, ScreenRect, point};

    /// On X11, beside the click, or the pointer when the tray host gave no
    /// position (the menu, or hosts that send 0, 0).
    pub fn anchoring(click: Option<ScreenRect>) -> Option<Anchoring> {
        if is_wayland() {
            return None;
        }
        match x11(click) {
            Ok(anchoring) => anchoring,
            Err(e) => {
                tracing::debug!("mini mixer: reading the X11 screen failed: {e}");
                None
            }
        }
    }

    /// Whether winit runs on Wayland, which it picks when a Wayland display
    /// is named (falling back to X11 only if connecting fails).
    fn is_wayland() -> bool {
        ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
    }

    type Error = Box<dyn std::error::Error>;

    fn x11(click: Option<ScreenRect>) -> Result<Option<Anchoring>, Error> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let anchor = match click {
            Some(rect) => rect,
            None => {
                let pointer = conn.query_pointer(root)?.reply()?;
                point(f32::from(pointer.root_x), f32::from(pointer.root_y))
            }
        };
        let monitors = monitors(&conn, root)?;
        let Some(bounds) = monitors
            .iter()
            .find(|m| m.contains(anchor.center()))
            .or(monitors.first())
            .copied()
        else {
            return Ok(None);
        };
        // The window manager's work area spans all monitors, so a panel on
        // one would shrink the others: it is only used with one monitor.
        let work = match monitors.len() {
            1 => work_area(&conn, root)
                .ok()
                .flatten()
                .and_then(|work| work.intersection(&bounds)),
            _ => None,
        };
        Ok(Some(Anchoring::new(
            anchor,
            Monitor {
                bounds,
                work: work.unwrap_or(bounds),
                scale: None,
            },
        )))
    }

    /// RandR monitors, or the whole screen without RandR 1.5.
    fn monitors(conn: &impl Connection, root: Window) -> Result<Vec<ScreenRect>, Error> {
        if let Ok(reply) = conn.randr_get_monitors(root, true)?.reply() {
            let rects: Vec<ScreenRect> = reply
                .monitors
                .iter()
                .map(|m| Rectangle {
                    x: f32::from(m.x),
                    y: f32::from(m.y),
                    width: f32::from(m.width),
                    height: f32::from(m.height),
                })
                .collect();
            if !rects.is_empty() {
                return Ok(rects);
            }
        }
        let geometry = conn.get_geometry(root)?.reply()?;
        Ok(vec![Rectangle {
            x: 0.0,
            y: 0.0,
            width: f32::from(geometry.width),
            height: f32::from(geometry.height),
        }])
    }

    /// The window manager's work area on the current desktop.
    fn work_area(conn: &impl Connection, root: Window) -> Result<Option<ScreenRect>, Error> {
        let cardinals = |name: &[u8]| -> Result<Vec<u32>, Error> {
            let atom = conn.intern_atom(false, name)?.reply()?.atom;
            let reply = conn
                .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1024)?
                .reply()?;
            Ok(reply.value32().into_iter().flatten().collect())
        };
        let desktop = cardinals(b"_NET_CURRENT_DESKTOP")?
            .first()
            .copied()
            .unwrap_or(0) as usize;
        let areas = cardinals(b"_NET_WORKAREA")?;
        let (areas, _) = areas.as_chunks::<4>();
        let area = areas.get(desktop).or(areas.first());
        Ok(area.map(|a| Rectangle {
            x: a[0] as f32,
            y: a[1] as f32,
            width: a[2] as f32,
            height: a[3] as f32,
        }))
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    use super::{Anchoring, ScreenRect};

    pub fn anchoring(_click: Option<ScreenRect>) -> Option<Anchoring> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size::new(400.0, 300.0);

    fn rect(x: f32, y: f32, width: f32, height: f32) -> ScreenRect {
        Rectangle {
            x,
            y,
            width,
            height,
        }
    }

    /// A 1920x1080 monitor with a 48 px taskbar on `edge`.
    fn monitor(edge: Edge, scale: f32) -> Monitor {
        let bar = 48.0;
        let work = match edge {
            Edge::Bottom => rect(0.0, 0.0, 1920.0, 1080.0 - bar),
            Edge::Top => rect(0.0, bar, 1920.0, 1080.0 - bar),
            Edge::Left => rect(bar, 0.0, 1920.0 - bar, 1080.0),
            Edge::Right => rect(0.0, 0.0, 1920.0 - bar, 1080.0),
        };
        Monitor {
            bounds: rect(0.0, 0.0, 1920.0, 1080.0),
            work,
            scale: Some(scale),
        }
    }

    fn margin(scale: f32) -> f32 {
        style::MINI_SCREEN_MARGIN * scale
    }

    #[test]
    fn bottom_taskbar_opens_above_the_icon() {
        let m = monitor(Edge::Bottom, 1.0);
        let icon = rect(1500.0, 1040.0, 32.0, 40.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Bottom);
        let p = place(icon, m.work, SIZE, 1.0, Edge::Bottom);
        assert_eq!(p, Point::new(1516.0 - 200.0, 1032.0 - margin(1.0) - 300.0));
    }

    #[test]
    fn top_taskbar_opens_below_the_icon() {
        let m = monitor(Edge::Top, 1.0);
        let icon = rect(900.0, 4.0, 32.0, 40.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Top);
        let p = place(icon, m.work, SIZE, 1.0, Edge::Top);
        assert_eq!(p, Point::new(916.0 - 200.0, 48.0 + margin(1.0)));
    }

    #[test]
    fn side_taskbars_open_beside_the_icon() {
        let m = monitor(Edge::Left, 1.0);
        let icon = rect(8.0, 600.0, 32.0, 32.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Left);
        let p = place(icon, m.work, SIZE, 1.0, Edge::Left);
        assert_eq!(p, Point::new(48.0 + margin(1.0), 616.0 - 150.0));

        let m = monitor(Edge::Right, 1.0);
        let icon = rect(1880.0, 600.0, 32.0, 32.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Right);
        let p = place(icon, m.work, SIZE, 1.0, Edge::Right);
        assert_eq!(p, Point::new(1872.0 - margin(1.0) - 400.0, 616.0 - 150.0));
    }

    #[test]
    fn stays_inside_the_work_area() {
        // An icon in the corner pushes the window back inside.
        let m = monitor(Edge::Bottom, 1.0);
        let corner = rect(1900.0, 1040.0, 20.0, 40.0);
        let p = place(corner, m.work, SIZE, 1.0, Edge::Bottom);
        assert_eq!(p.x, 1920.0 - margin(1.0) - 400.0);
        let m = monitor(Edge::Right, 1.0);
        let top = rect(1880.0, 0.0, 32.0, 32.0);
        assert_eq!(place(top, m.work, SIZE, 1.0, Edge::Right).y, margin(1.0));
        let low = rect(1880.0, 1060.0, 32.0, 20.0);
        let p = place(low, m.work, SIZE, 1.0, Edge::Right);
        assert_eq!(p.y, 1080.0 - margin(1.0) - 300.0);
        // Too big for the work area: centred in it, never above or left.
        let small = rect(0.0, 0.0, 300.0, 200.0);
        assert_eq!(place(corner, small, SIZE, 1.0, Edge::Bottom), Point::ORIGIN);
        let roomy = rect(100.0, 100.0, 404.0, 1000.0);
        assert_eq!(place(corner, roomy, SIZE, 1.0, Edge::Bottom).x, 102.0);
    }

    #[test]
    fn scale_sizes_the_window_and_margin() {
        // 150 %: the window is 600 x 450 physical pixels.
        let m = monitor(Edge::Bottom, 1.5);
        let icon = rect(1500.0, 1040.0, 32.0, 40.0);
        let p = place(icon, m.work, SIZE, 1.5, Edge::Bottom);
        assert_eq!(p, Point::new(1516.0 - 300.0, 1032.0 - margin(1.5) - 450.0));
        let anchoring = Anchoring::new(icon, m);
        assert_eq!(anchoring.edge, Edge::Bottom);
        // A window already at 150 % gets logical coordinates at 150 %.
        let logical = anchoring.position(SIZE, 1.5);
        assert!((logical.x - p.x / 1.5).abs() < 1e-3);
        assert!((logical.y - p.y / 1.5).abs() < 1e-3);
        // One still at 100 % (created on another monitor) gets the same
        // physical spot at its own scale.
        assert_eq!(anchoring.position(SIZE, 1.0), p);
        // Without the monitor's scale, the window's own is used for both.
        let unknown = Anchoring {
            monitor: Monitor { scale: None, ..m },
            ..anchoring
        };
        assert_eq!(
            unknown.position(SIZE, 1.0),
            place(icon, m.work, SIZE, 1.0, Edge::Bottom)
        );
    }

    #[test]
    fn edge_without_a_visible_taskbar() {
        // An auto-hiding taskbar leaves the whole monitor as work area: the
        // nearest edge to the icon wins.
        let bounds = rect(0.0, 0.0, 1920.0, 1080.0);
        let m = Monitor {
            bounds,
            work: bounds,
            scale: None,
        };
        let edge = |x, y| taskbar_edge(&m, rect(x, y, 32.0, 32.0));
        assert_eq!(edge(1800.0, 1040.0), Edge::Bottom);
        assert_eq!(edge(900.0, 0.0), Edge::Top);
        assert_eq!(edge(0.0, 500.0), Edge::Left);
        assert_eq!(edge(1888.0, 500.0), Edge::Right);
        // A point inside the work area (the pointer after a menu click)
        // goes by the side a panel takes.
        let m = monitor(Edge::Top, 1.0);
        let pointer = point(1700.0, 120.0);
        assert_eq!(taskbar_edge(&m, pointer), Edge::Top);
        assert_eq!(
            place(pointer, m.work, SIZE, 1.0, Edge::Top).y,
            120.0 + margin(1.0)
        );
    }

    #[test]
    fn second_monitor() {
        // A monitor left of the primary one, with negative coordinates.
        let m = Monitor {
            bounds: rect(-1280.0, 0.0, 1280.0, 1024.0),
            work: rect(-1280.0, 0.0, 1280.0, 984.0),
            scale: Some(1.0),
        };
        let anchoring = Anchoring::new(rect(-60.0, 990.0, 24.0, 30.0), m);
        assert_eq!(anchoring.edge, Edge::Bottom);
        assert_eq!(
            anchoring.position(SIZE, 1.0),
            Point::new(-margin(1.0) - 400.0, 984.0 - margin(1.0) - 300.0)
        );
    }
}
