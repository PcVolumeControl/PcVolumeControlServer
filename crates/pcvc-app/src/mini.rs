//! When the mini mixer opens and closes, and how tall it is. The caller
//! passes the time with every event, so tests drive it with a fake clock.
//!
//! Clicking outside the window reaches the app only as the window losing
//! focus (Wayland apps cannot see other clicks), which has two classic
//! failure modes guarded against here: an unfocus reported as the window
//! maps would close it at once, and the tray click meant to close it first
//! takes its focus (closing it) and then arrives as an activation that
//! would open it again.

use std::time::{Duration, Instant};

use iced::window;

use crate::style;

/// Losing focus this soon after the window is created is not taken as a
/// click outside: some platforms report it while the window maps.
pub const OPEN_GRACE: Duration = Duration::from_millis(300);

/// A tray click this soon after the window closed on losing focus is the
/// click that took the focus, so it leaves the window closed.
pub const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Clicks this soon after a double click belong to it: Windows reports the
/// second release as another click.
pub const DOUBLE_CLICK_QUIET: Duration = Duration::from_millis(500);

/// Why the window closed, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Escape,
    FocusLost,
    /// The tray icon or its Mixer item.
    Toggle,
    /// The full window was opened from it or by a double click.
    OpenFull,
    CloseButton,
    /// The platform asked (Alt+F4, the window list).
    CloseRequest,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A click on the tray icon.
    Toggle,
    /// The tray menu's Mixer item. Opening the menu took the focus, so a
    /// close on losing focus just before does not make it a close.
    MenuToggle,
    /// A double click on the tray icon, which opens the full window.
    DoubleClick,
    /// The app asked for a window with this id.
    Opened(window::Id),
    /// The window exists: window actions for it now take effect.
    Created(window::Id),
    Focused(window::Id),
    Unfocused(window::Id),
    Escape(window::Id),
    Close(CloseReason),
    /// The window is gone, whoever closed it.
    Closed(window::Id),
}

/// What the app should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Open,
    Close(window::Id, CloseReason),
    /// Ask for focus again after losing it while opening.
    Refocus(window::Id),
}

#[derive(Debug, Default)]
pub struct MiniWindow {
    open: Option<Open>,
    /// When the window last closed on losing focus.
    focus_closed_at: Option<Instant>,
    /// Clicks before this belong to a double click.
    quiet_until: Option<Instant>,
}

#[derive(Debug)]
struct Open {
    id: window::Id,
    /// When the window was created, or asked for until then.
    opened_at: Instant,
    /// Until the window exists, closing it is held back: iced drops a close
    /// for a window it has not created yet.
    created: bool,
    close_pending: Option<CloseReason>,
    focused: bool,
    refocused: bool,
}

impl MiniWindow {
    pub fn id(&self) -> Option<window::Id> {
        self.open.as_ref().map(|o| o.id)
    }

    pub fn is(&self, id: window::Id) -> bool {
        self.id() == Some(id)
    }

    /// Whether the window exists, so it can be resized.
    pub fn is_created(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.created)
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Action {
        match event {
            Event::Toggle | Event::MenuToggle => {
                let click = event == Event::Toggle;
                if click && self.quiet_until.is_some_and(|until| now < until) {
                    tracing::debug!("mini mixer: tray click ignored, part of a double click");
                    return Action::None;
                }
                if let Some(open) = &mut self.open {
                    if open.close_pending.take().is_some() {
                        tracing::debug!("mini mixer: stays open, toggled again while opening");
                        return Action::None;
                    }
                    return self.close(CloseReason::Toggle);
                }
                if let Some(closed) = self.focus_closed_at.take()
                    && click
                    && now.saturating_duration_since(closed) < TOGGLE_DEBOUNCE
                {
                    tracing::debug!(
                        "mini mixer: tray click {:?} after it closed on losing focus; \
                         taken as the click that closed it",
                        now.saturating_duration_since(closed)
                    );
                    return Action::None;
                }
                Action::Open
            }
            Event::DoubleClick => {
                self.quiet_until = Some(now + DOUBLE_CLICK_QUIET);
                self.close(CloseReason::OpenFull)
            }
            Event::Opened(id) => {
                self.open = Some(Open {
                    id,
                    opened_at: now,
                    created: false,
                    close_pending: None,
                    focused: false,
                    refocused: false,
                });
                self.focus_closed_at = None;
                Action::None
            }
            Event::Created(id) => {
                let Some(open) = self.open.as_mut().filter(|o| o.id == id) else {
                    return Action::None;
                };
                open.created = true;
                open.opened_at = now;
                match open.close_pending.take() {
                    Some(reason) => self.close(reason),
                    None => Action::None,
                }
            }
            Event::Focused(id) => {
                if let Some(open) = self.open.as_mut().filter(|o| o.id == id) {
                    open.focused = true;
                    tracing::debug!(
                        "mini mixer: focused {:?} after creation",
                        now.saturating_duration_since(open.opened_at)
                    );
                }
                Action::None
            }
            Event::Unfocused(id) => {
                let Some(open) = self.open.as_mut().filter(|o| o.id == id) else {
                    return Action::None;
                };
                let age = now.saturating_duration_since(open.opened_at);
                if !open.focused {
                    tracing::debug!(
                        "mini mixer: unfocus {age:?} after creation, before any focus; ignored"
                    );
                    return Action::None;
                }
                if age < OPEN_GRACE {
                    tracing::debug!("mini mixer: unfocus {age:?} after creation; ignored");
                    if open.refocused {
                        return Action::None;
                    }
                    open.refocused = true;
                    return Action::Refocus(id);
                }
                tracing::debug!("mini mixer: unfocused {age:?} after creation");
                self.focus_closed_at = Some(now);
                self.close(CloseReason::FocusLost)
            }
            Event::Escape(id) if self.is(id) => self.close(CloseReason::Escape),
            Event::Escape(_) => Action::None,
            Event::Close(reason) => self.close(reason),
            Event::Closed(id) => {
                if self.is(id) {
                    tracing::debug!("mini mixer: closed by the platform");
                    self.open = None;
                }
                Action::None
            }
        }
    }

    fn close(&mut self, reason: CloseReason) -> Action {
        let Some(open) = self.open.as_mut() else {
            return Action::None;
        };
        if !open.created {
            tracing::debug!("mini mixer: will close ({reason:?}) once created");
            open.close_pending.get_or_insert(reason);
            return Action::None;
        }
        tracing::debug!(
            "mini mixer: closing ({reason:?}), had focus: {}",
            open.focused
        );
        let id = open.id;
        self.open = None;
        Action::Close(id, reason)
    }
}

/// What the mini mixer shows below its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// A note while the first audio state is read.
    Loading,
    /// The device picker and a note that there is no default device.
    NoDevice,
    /// Picker, master row, divider and this many application rows (or a
    /// note when there are none).
    Rows(usize),
}

/// Height of the mini mixer showing `body`, up to `MINI_MAX_HEIGHT`;
/// beyond that the application rows scroll. Mirrors the layout in
/// `app::mini`, whose rows all have the compact density's fixed height.
pub fn height(body: Body) -> f32 {
    let row = style::MINI_ROW;
    let gap = style::MINI_GAP;
    let content = match body {
        Body::Loading => row,
        Body::NoDevice => row + gap + row,
        Body::Rows(rows) => {
            let list = match rows {
                0 => row,
                n => n as f32 * row + (n - 1) as f32 * style::COMPACT.row_spacing,
            };
            row + gap + row + gap + style::MINI_DIVIDER + gap + list
        }
    };
    let padding = style::MINI_PADDING.top + style::MINI_PADDING.bottom;
    (padding + style::MINI_HEADER + gap + content).min(style::MINI_MAX_HEIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// A mini window driven by events at offsets from a fixed start.
    struct Clock {
        mini: MiniWindow,
        start: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self {
                mini: MiniWindow::default(),
                start: Instant::now(),
            }
        }

        fn at(&mut self, ms: u64, event: Event) -> Action {
            self.mini.handle(event, self.start + ms as u32 * MS)
        }

        /// Opens the window at `ms` as the app does on `Action::Open`.
        fn open(&mut self, ms: u64) -> window::Id {
            assert_eq!(self.at(ms, Event::Toggle), Action::Open);
            let id = window::Id::unique();
            assert_eq!(self.at(ms, Event::Opened(id)), Action::None);
            assert!(!self.mini.is_created());
            assert_eq!(self.at(ms + 5, Event::Created(id)), Action::None);
            assert!(self.mini.is_created());
            id
        }
    }

    #[test]
    fn closes_held_back_until_the_window_exists() {
        let mut c = Clock::new();
        assert_eq!(c.at(0, Event::Toggle), Action::Open);
        let id = window::Id::unique();
        c.at(0, Event::Opened(id));
        // Closed before it exists: held back, still the mini window.
        assert_eq!(c.at(1, Event::Close(CloseReason::OpenFull)), Action::None);
        assert!(c.mini.is(id));
        assert_eq!(
            c.at(2, Event::Created(id)),
            Action::Close(id, CloseReason::OpenFull)
        );
        assert_eq!(c.mini.id(), None);
        // Toggled closed and open again before it exists: stays open.
        assert_eq!(c.at(1000, Event::Toggle), Action::Open);
        let id = window::Id::unique();
        c.at(1000, Event::Opened(id));
        assert_eq!(c.at(1001, Event::Toggle), Action::None);
        assert_eq!(c.at(1002, Event::Toggle), Action::None);
        assert_eq!(c.at(1003, Event::Created(id)), Action::None);
        assert!(c.mini.is(id));
        // A stale creation changes nothing.
        assert_eq!(
            c.at(1004, Event::Created(window::Id::unique())),
            Action::None
        );
        assert_eq!(
            c.at(1005, Event::Toggle),
            Action::Close(id, CloseReason::Toggle)
        );
    }

    #[test]
    fn toggles_open_and_closed() {
        let mut c = Clock::new();
        let id = c.open(0);
        assert!(c.mini.is(id));
        assert_eq!(
            c.at(2000, Event::Toggle),
            Action::Close(id, CloseReason::Toggle)
        );
        assert_eq!(c.mini.id(), None);
        // Events for the closed window change nothing.
        assert_eq!(c.at(2010, Event::Unfocused(id)), Action::None);
        assert_eq!(c.at(2020, Event::Escape(id)), Action::None);
        let again = c.open(2100);
        assert_ne!(again, id);
    }

    #[test]
    fn unfocus_while_opening_is_ignored() {
        let mut c = Clock::new();
        let id = c.open(0);
        // Unfocused before it was ever focused: not a click outside.
        assert_eq!(c.at(50, Event::Unfocused(id)), Action::None);
        assert!(c.mini.is(id));
        // Focused, then unfocused within the grace: asks for focus once.
        c.at(100, Event::Focused(id));
        assert_eq!(c.at(150, Event::Unfocused(id)), Action::Refocus(id));
        c.at(160, Event::Focused(id));
        assert_eq!(c.at(200, Event::Unfocused(id)), Action::None);
        assert!(c.mini.is(id));
        // After the grace, counted from its creation at 5, losing focus
        // closes it.
        c.at(250, Event::Focused(id));
        let after = 5 + OPEN_GRACE.as_millis() as u64;
        assert_eq!(
            c.at(after, Event::Unfocused(id)),
            Action::Close(id, CloseReason::FocusLost)
        );
    }

    #[test]
    fn grace_counts_from_creation() {
        let mut c = Clock::new();
        assert_eq!(c.at(0, Event::Toggle), Action::Open);
        let id = window::Id::unique();
        c.at(0, Event::Opened(id));
        // Created late: an unfocus soon after is still in the grace.
        c.at(1000, Event::Created(id));
        c.at(1050, Event::Focused(id));
        assert_eq!(c.at(1100, Event::Unfocused(id)), Action::Refocus(id));
        assert!(c.mini.is(id));
    }

    #[test]
    fn menu_item_opens_after_the_menu_took_the_focus() {
        let mut c = Clock::new();
        let id = c.open(0);
        c.at(20, Event::Focused(id));
        // Opening the tray menu takes the focus; choosing Mixer at once
        // opens it again, unlike a click on the icon.
        c.at(1000, Event::Unfocused(id));
        assert_eq!(c.at(1100, Event::MenuToggle), Action::Open);
        // Nor does a double click just before swallow it.
        let mut c = Clock::new();
        c.at(0, Event::DoubleClick);
        assert_eq!(c.at(100, Event::MenuToggle), Action::Open);
        // With the window open it closes it.
        let id = c.open(1000);
        assert_eq!(
            c.at(1100, Event::MenuToggle),
            Action::Close(id, CloseReason::Toggle)
        );
    }

    #[test]
    fn never_focused_window_still_closes() {
        let mut c = Clock::new();
        let id = c.open(0);
        // Focus stealing prevention: no focus, unfocus events ignored.
        assert_eq!(c.at(5000, Event::Unfocused(id)), Action::None);
        assert_eq!(
            c.at(6000, Event::Toggle),
            Action::Close(id, CloseReason::Toggle)
        );
        let id = c.open(7000);
        assert_eq!(
            c.at(9000, Event::Close(CloseReason::CloseButton)),
            Action::Close(id, CloseReason::CloseButton)
        );
        // Clicking into it focuses it; then Escape and clicks outside work.
        let id = c.open(10_000);
        c.at(12_000, Event::Focused(id));
        assert_eq!(
            c.at(12_500, Event::Escape(id)),
            Action::Close(id, CloseReason::Escape)
        );
    }

    #[test]
    fn tray_click_after_focus_loss_does_not_reopen() {
        let mut c = Clock::new();
        let id = c.open(0);
        c.at(20, Event::Focused(id));
        // Pressing on the tray icon takes the focus and closes the window;
        // the click's activation arrives just after.
        assert_eq!(
            c.at(1000, Event::Unfocused(id)),
            Action::Close(id, CloseReason::FocusLost)
        );
        let soon = 1000 + TOGGLE_DEBOUNCE.as_millis() as u64 - 1;
        assert_eq!(c.at(soon, Event::Toggle), Action::None);
        // The next click opens it.
        assert_eq!(c.at(soon + 200, Event::Toggle), Action::Open);
    }

    #[test]
    fn tray_click_well_after_focus_loss_reopens() {
        let mut c = Clock::new();
        let id = c.open(0);
        c.at(20, Event::Focused(id));
        c.at(1000, Event::Unfocused(id));
        let late = 1000 + TOGGLE_DEBOUNCE.as_millis() as u64;
        assert_eq!(c.at(late, Event::Toggle), Action::Open);
    }

    #[test]
    fn other_closes_do_not_debounce() {
        let mut c = Clock::new();
        let id = c.open(0);
        c.at(20, Event::Focused(id));
        assert_eq!(
            c.at(1000, Event::Escape(id)),
            Action::Close(id, CloseReason::Escape)
        );
        assert_eq!(c.at(1010, Event::Toggle), Action::Open);
    }

    #[test]
    fn double_click_closes_and_swallows_its_clicks() {
        let mut c = Clock::new();
        // Windows: release, double click, release.
        let id = c.open(0);
        assert_eq!(
            c.at(150, Event::DoubleClick),
            Action::Close(id, CloseReason::OpenFull)
        );
        assert_eq!(c.at(200, Event::Toggle), Action::None);
        assert_eq!(c.mini.id(), None);
        let later = 150 + DOUBLE_CLICK_QUIET.as_millis() as u64;
        assert_eq!(c.at(later, Event::Toggle), Action::Open);
        // A double click with the window closed only starts the quiet time.
        let mut c = Clock::new();
        assert_eq!(c.at(0, Event::DoubleClick), Action::None);
        assert_eq!(c.at(100, Event::Toggle), Action::None);
    }

    #[test]
    fn closed_by_the_platform() {
        let mut c = Clock::new();
        let id = c.open(0);
        c.at(10, Event::Closed(window::Id::unique()));
        assert!(c.mini.is(id));
        c.at(20, Event::Closed(id));
        assert_eq!(c.mini.id(), None);
        assert_eq!(c.at(30, Event::Toggle), Action::Open);
    }

    #[test]
    fn height_follows_rows_up_to_the_cap() {
        let rows = |n| height(Body::Rows(n));
        let row = style::MINI_ROW + style::COMPACT.row_spacing;
        assert_eq!(rows(1), rows(0));
        assert_eq!(rows(2) - rows(1), row);
        assert_eq!(rows(5) - rows(2), 3.0 * row);
        let fits = (1..100)
            .take_while(|&n| rows(n) < style::MINI_MAX_HEIGHT)
            .last();
        let fits = fits.unwrap();
        assert!(fits >= 6, "at least six rows fit without scrolling");
        assert_eq!(rows(fits + 1), style::MINI_MAX_HEIGHT);
        assert_eq!(rows(500), style::MINI_MAX_HEIGHT);
        // Each note takes a row where the rest would be.
        let step = style::MINI_ROW + style::MINI_GAP;
        assert_eq!(height(Body::NoDevice) - height(Body::Loading), step);
        assert_eq!(
            rows(0) - height(Body::NoDevice),
            step + style::MINI_DIVIDER + style::MINI_GAP
        );
    }
}
