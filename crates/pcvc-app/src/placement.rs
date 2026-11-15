//! Where the mini mixer opens: beside the tray icon, on the side away from
//! the taskbar, inside the work area of the icon's monitor.
//!
//! The geometry is in physical pixels, as the platforms report it. On
//! Windows the tray click gives the icon's rectangle and the monitor comes
//! from Win32; on X11 the click (or the pointer) gives a point and RandR the
//! monitors. Wayland does not let an application position its own windows,
//! so there the compositor places it.

use iced::{Point, Size};

use crate::style;

/// A rectangle in physical screen pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ScreenRect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn point(x: f64, y: f64) -> Self {
        Self::new(x, y, 0.0, 0.0)
    }

    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }

    fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    #[cfg(target_os = "linux")]
    fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    #[cfg(any(target_os = "linux", test))]
    fn intersection(&self, other: &Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Self::new(x, y, right - x, bottom - y))
    }
}

/// The screen edge a taskbar or panel sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// A monitor's bounds, the part of it panels and taskbars leave free, and
/// its scale factor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monitor {
    pub bounds: ScreenRect,
    pub work: ScreenRect,
    pub scale: f64,
}

/// The edge of the taskbar holding a tray icon at `anchor` on `monitor`.
pub fn taskbar_edge(monitor: &Monitor, anchor: ScreenRect) -> Edge {
    let (cx, cy) = anchor.center();
    let (bounds, work) = (monitor.bounds, monitor.work);
    // An icon outside the work area is on the panel that took that strip.
    if cy >= work.bottom() {
        return Edge::Bottom;
    }
    if cy < work.y {
        return Edge::Top;
    }
    if cx < work.x {
        return Edge::Left;
    }
    if cx >= work.right() {
        return Edge::Right;
    }
    // Inside it (an auto-hiding taskbar, or the pointer after a menu
    // click): the side a panel takes, else the nearest monitor edge.
    if work.bottom() < bounds.bottom() {
        return Edge::Bottom;
    }
    if work.y > bounds.y {
        return Edge::Top;
    }
    if work.x > bounds.x {
        return Edge::Left;
    }
    if work.right() < bounds.right() {
        return Edge::Right;
    }
    [
        (bounds.bottom() - cy, Edge::Bottom),
        (cy - bounds.y, Edge::Top),
        (cx - bounds.x, Edge::Left),
        (bounds.right() - cx, Edge::Right),
    ]
    .into_iter()
    .min_by(|a, b| a.0.total_cmp(&b.0))
    .map_or(Edge::Bottom, |(_, edge)| edge)
}

/// Top-left corner, in physical pixels, of a window of logical `size` at
/// `scale` placed beside `anchor` on the side away from a taskbar on
/// `edge`, centred on the anchor along the taskbar, and kept inside `work`
/// with a margin.
pub fn place(
    anchor: ScreenRect,
    work: ScreenRect,
    size: Size,
    scale: f64,
    edge: Edge,
) -> (f64, f64) {
    let width = f64::from(size.width) * scale;
    let height = f64::from(size.height) * scale;
    let margin = f64::from(style::MINI_SCREEN_MARGIN) * scale;
    let (cx, cy) = anchor.center();
    let (x, y) = match edge {
        Edge::Bottom => (
            cx - width / 2.0,
            anchor.y.min(work.bottom()) - margin - height,
        ),
        Edge::Top => (cx - width / 2.0, anchor.bottom().max(work.y) + margin),
        Edge::Left => (anchor.right().max(work.x) + margin, cy - height / 2.0),
        Edge::Right => (
            anchor.x.min(work.right()) - margin - width,
            cy - height / 2.0,
        ),
    };
    (
        within(x, work.x, work.width, width, margin).round(),
        within(y, work.y, work.height, height, margin).round(),
    )
}

/// `start` of a span of `len` moved inside the range from `from` of
/// `extent`, `margin` from its ends; centred in it when it does not fit.
fn within(start: f64, from: f64, extent: f64, len: f64, margin: f64) -> f64 {
    let (low, high) = (from + margin, from + extent - margin - len);
    if high < low {
        from + ((extent - len) / 2.0).max(0.0)
    } else {
        start.clamp(low, high)
    }
}

/// What the mini mixer was placed against, kept to place it again when its
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

    /// The window's position in iced's logical coordinates, which the
    /// platform turns back into physical ones at the window's scale
    /// (`monitor.scale` until the window reports its own).
    pub fn position(&self, size: Size) -> Point {
        let scale = self.monitor.scale;
        let (x, y) = place(self.anchor, self.monitor.work, size, scale, self.edge);
        Point::new((x / scale) as f32, (y / scale) as f32)
    }
}

/// Where to anchor the mini mixer opened by a tray click at `click` (`None`
/// from the menu), or `None` to leave it to the platform. `scale` is the
/// app's last known scale factor, used where the platform does not say.
pub fn anchoring(click: Option<ScreenRect>, scale: Option<f32>) -> Option<Anchoring> {
    let anchoring = platform::anchoring(click, scale.map_or(1.0, f64::from));
    match &anchoring {
        Some(a) => tracing::debug!(
            "mini mixer: anchored to {:?} on the {:?} edge, work area {:?}, scale {}",
            a.anchor,
            a.edge,
            a.monitor.work,
            a.monitor.scale
        ),
        None => tracing::debug!("mini mixer: placed by the platform"),
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

    use super::{Anchoring, Monitor, ScreenRect};

    /// Beside the clicked icon, or at the bottom right of the primary
    /// monitor's work area without one.
    pub fn anchoring(click: Option<ScreenRect>, _scale: f64) -> Option<Anchoring> {
        match click {
            Some(rect) => {
                let (x, y) = rect.center();
                let monitor = monitor_at(x as i32, y as i32, MONITOR_DEFAULTTONEAREST)?;
                Some(Anchoring::new(rect, monitor))
            }
            None => {
                let monitor = monitor_at(0, 0, MONITOR_DEFAULTTOPRIMARY)?;
                let work = monitor.work;
                let corner = ScreenRect::point(work.right(), work.bottom());
                Some(Anchoring {
                    anchor: corner,
                    monitor,
                    edge: super::Edge::Bottom,
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
        let dpi = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
        let scale = if dpi == 0 && dpi_x > 0 {
            f64::from(dpi_x) / 96.0
        } else {
            1.0
        };
        Some(Monitor {
            bounds: rect(info.rcMonitor),
            work: rect(info.rcWork),
            scale,
        })
    }

    fn rect(r: RECT) -> ScreenRect {
        ScreenRect::new(
            f64::from(r.left),
            f64::from(r.top),
            f64::from(r.right - r.left),
            f64::from(r.bottom - r.top),
        )
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};

    use super::{Anchoring, Monitor, ScreenRect};

    /// On X11, beside the click, or the pointer when the tray host gave no
    /// position (the menu, or hosts that send 0, 0).
    pub fn anchoring(click: Option<ScreenRect>, scale: f64) -> Option<Anchoring> {
        if is_wayland() {
            return None;
        }
        match x11(click, scale) {
            Ok(anchoring) => anchoring,
            Err(e) => {
                tracing::debug!("mini mixer: reading the X11 screen failed: {e}");
                None
            }
        }
    }

    /// Whether winit picks Wayland, as it does when a Wayland display is
    /// named.
    fn is_wayland() -> bool {
        ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .iter()
            .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
    }

    type Error = Box<dyn std::error::Error>;

    fn x11(click: Option<ScreenRect>, scale: f64) -> Result<Option<Anchoring>, Error> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let anchor = match click {
            Some(rect) => rect,
            None => {
                let pointer = conn.query_pointer(root)?.reply()?;
                ScreenRect::point(f64::from(pointer.root_x), f64::from(pointer.root_y))
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
        let work = work_area(&conn, root)
            .ok()
            .flatten()
            .and_then(|work| work.intersection(&bounds))
            .unwrap_or(bounds);
        Ok(Some(Anchoring::new(
            anchor,
            Monitor {
                bounds,
                work,
                scale,
            },
        )))
    }

    /// RandR monitors, or the whole screen without RandR 1.5.
    fn monitors(conn: &impl Connection, root: Window) -> Result<Vec<ScreenRect>, Error> {
        if let Ok(reply) = conn.randr_get_monitors(root, true)?.reply() {
            let rects: Vec<ScreenRect> = reply
                .monitors
                .iter()
                .map(|m| {
                    ScreenRect::new(
                        f64::from(m.x),
                        f64::from(m.y),
                        f64::from(m.width),
                        f64::from(m.height),
                    )
                })
                .collect();
            if !rects.is_empty() {
                return Ok(rects);
            }
        }
        let geometry = conn.get_geometry(root)?.reply()?;
        Ok(vec![ScreenRect::new(
            0.0,
            0.0,
            f64::from(geometry.width),
            f64::from(geometry.height),
        )])
    }

    /// The window manager's work area on the current desktop, which spans
    /// all monitors.
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
        Ok(area.map(|a| {
            ScreenRect::new(
                f64::from(a[0]),
                f64::from(a[1]),
                f64::from(a[2]),
                f64::from(a[3]),
            )
        }))
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    use super::{Anchoring, ScreenRect};

    pub fn anchoring(_click: Option<ScreenRect>, _scale: f64) -> Option<Anchoring> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size::new(400.0, 300.0);

    /// A 1920x1080 monitor with a 48 px taskbar on `edge`.
    fn monitor(edge: Edge, scale: f64) -> Monitor {
        let bounds = ScreenRect::new(0.0, 0.0, 1920.0, 1080.0);
        let bar = 48.0;
        let work = match edge {
            Edge::Bottom => ScreenRect::new(0.0, 0.0, 1920.0, 1080.0 - bar),
            Edge::Top => ScreenRect::new(0.0, bar, 1920.0, 1080.0 - bar),
            Edge::Left => ScreenRect::new(bar, 0.0, 1920.0 - bar, 1080.0),
            Edge::Right => ScreenRect::new(0.0, 0.0, 1920.0 - bar, 1080.0),
        };
        Monitor {
            bounds,
            work,
            scale,
        }
    }

    fn margin(scale: f64) -> f64 {
        f64::from(style::MINI_SCREEN_MARGIN) * scale
    }

    #[test]
    fn bottom_taskbar_opens_above_the_icon() {
        let m = monitor(Edge::Bottom, 1.0);
        let icon = ScreenRect::new(1500.0, 1040.0, 32.0, 40.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Bottom);
        let (x, y) = place(icon, m.work, SIZE, 1.0, Edge::Bottom);
        assert_eq!(x, 1516.0 - 200.0);
        assert_eq!(y, 1032.0 - margin(1.0) - 300.0);
    }

    #[test]
    fn top_taskbar_opens_below_the_icon() {
        let m = monitor(Edge::Top, 1.0);
        let icon = ScreenRect::new(900.0, 4.0, 32.0, 40.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Top);
        let (x, y) = place(icon, m.work, SIZE, 1.0, Edge::Top);
        assert_eq!((x, y), (916.0 - 200.0, 48.0 + margin(1.0)));
    }

    #[test]
    fn side_taskbars_open_beside_the_icon() {
        let m = monitor(Edge::Left, 1.0);
        let icon = ScreenRect::new(8.0, 600.0, 32.0, 32.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Left);
        let (x, y) = place(icon, m.work, SIZE, 1.0, Edge::Left);
        assert_eq!((x, y), (48.0 + margin(1.0), 616.0 - 150.0));

        let m = monitor(Edge::Right, 1.0);
        let icon = ScreenRect::new(1880.0, 600.0, 32.0, 32.0);
        assert_eq!(taskbar_edge(&m, icon), Edge::Right);
        let (x, y) = place(icon, m.work, SIZE, 1.0, Edge::Right);
        assert_eq!((x, y), (1872.0 - margin(1.0) - 400.0, 616.0 - 150.0));
    }

    #[test]
    fn stays_inside_the_work_area() {
        // An icon in the corner pushes the window back inside.
        let m = monitor(Edge::Bottom, 1.0);
        let corner = ScreenRect::new(1900.0, 1040.0, 20.0, 40.0);
        let (x, _) = place(corner, m.work, SIZE, 1.0, Edge::Bottom);
        assert_eq!(x, 1920.0 - margin(1.0) - 400.0);
        let m = monitor(Edge::Right, 1.0);
        let top = ScreenRect::new(1880.0, 0.0, 32.0, 32.0);
        let (_, y) = place(top, m.work, SIZE, 1.0, Edge::Right);
        assert_eq!(y, margin(1.0));
        let bottom = ScreenRect::new(1880.0, 1060.0, 32.0, 20.0);
        let (_, y) = place(bottom, m.work, SIZE, 1.0, Edge::Right);
        assert_eq!(y, 1080.0 - margin(1.0) - 300.0);
        // Too big for the work area: centred in it, never above or left.
        let small = ScreenRect::new(0.0, 0.0, 300.0, 200.0);
        let (x, y) = place(corner, small, SIZE, 1.0, Edge::Bottom);
        assert_eq!((x, y), (0.0, 0.0));
        let roomy = ScreenRect::new(100.0, 100.0, 404.0, 1000.0);
        let (x, _) = place(corner, roomy, SIZE, 1.0, Edge::Bottom);
        assert_eq!(x, 102.0);
    }

    #[test]
    fn scale_sizes_the_window_and_margin() {
        // 150 %: the window is 600 x 450 physical pixels.
        let m = monitor(Edge::Bottom, 1.5);
        let icon = ScreenRect::new(1500.0, 1040.0, 32.0, 40.0);
        let (x, y) = place(icon, m.work, SIZE, 1.5, Edge::Bottom);
        assert_eq!(x, 1516.0 - 300.0);
        assert_eq!(y, 1032.0 - margin(1.5) - 450.0);
        let anchoring = Anchoring::new(icon, m);
        assert_eq!(anchoring.edge, Edge::Bottom);
        let logical = anchoring.position(SIZE);
        assert!((f64::from(logical.x) - x / 1.5).abs() < 1e-3);
        assert!((f64::from(logical.y) - y / 1.5).abs() < 1e-3);
    }

    #[test]
    fn edge_without_a_visible_taskbar() {
        // An auto-hiding taskbar leaves the whole monitor as work area: the
        // nearest edge to the icon wins.
        let bounds = ScreenRect::new(0.0, 0.0, 1920.0, 1080.0);
        let m = Monitor {
            bounds,
            work: bounds,
            scale: 1.0,
        };
        let edge = |x, y| taskbar_edge(&m, ScreenRect::new(x, y, 32.0, 32.0));
        assert_eq!(edge(1800.0, 1040.0), Edge::Bottom);
        assert_eq!(edge(900.0, 0.0), Edge::Top);
        assert_eq!(edge(0.0, 500.0), Edge::Left);
        assert_eq!(edge(1888.0, 500.0), Edge::Right);
        // A point inside the work area (the pointer after a menu click)
        // goes by the side a panel takes.
        let m = monitor(Edge::Top, 1.0);
        let pointer = ScreenRect::point(1700.0, 120.0);
        assert_eq!(taskbar_edge(&m, pointer), Edge::Top);
        let (_, y) = place(pointer, m.work, SIZE, 1.0, Edge::Top);
        assert_eq!(y, 120.0 + margin(1.0));
    }

    #[test]
    fn second_monitor() {
        // A monitor left of the primary one, with negative coordinates.
        let m = Monitor {
            bounds: ScreenRect::new(-1280.0, 0.0, 1280.0, 1024.0),
            work: ScreenRect::new(-1280.0, 0.0, 1280.0, 984.0),
            scale: 1.0,
        };
        let icon = ScreenRect::new(-60.0, 990.0, 24.0, 30.0);
        let anchoring = Anchoring::new(icon, m);
        assert_eq!(anchoring.edge, Edge::Bottom);
        let p = anchoring.position(SIZE);
        assert_eq!(f64::from(p.x), -margin(1.0) - 400.0);
        assert_eq!(f64::from(p.y), 984.0 - margin(1.0) - 300.0);
    }

    #[test]
    fn intersects_rectangles() {
        let a = ScreenRect::new(0.0, 0.0, 100.0, 100.0);
        let b = ScreenRect::new(50.0, 60.0, 100.0, 100.0);
        assert_eq!(
            a.intersection(&b),
            Some(ScreenRect::new(50.0, 60.0, 50.0, 40.0))
        );
        assert_eq!(a.intersection(&ScreenRect::new(100.0, 0.0, 5.0, 5.0)), None);
    }
}
