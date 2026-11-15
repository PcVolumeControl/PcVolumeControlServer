//! The mini mixer: a small borderless window opened from the tray, with the
//! Mixer's device picker, master row and application rows at the compact
//! density. It closes on Escape, on losing focus, from the tray and from
//! its own buttons; `crate::mini` decides when.

use std::sync::LazyLock;
use std::time::Instant;

use iced::widget::{Space, button, column, container, responsive, row, rule, svg, text};
use iced::{Center, Element, Fill, Size, Task, window};
use pcvc_audio::AudioState;

use super::mixer::{MixerColumns, device_picker, fixed_height, with_tooltip};
use super::{App, Message};
use crate::icons;
use crate::mini::{self, Action, Body, CloseReason, Event};
use crate::placement::{self, Anchoring, ScreenRect};
use crate::style::{self, Button};

static OPEN_WINDOW: LazyLock<svg::Handle> =
    LazyLock::new(|| svg::Handle::from_memory(icons::OPEN_WINDOW));
static CLOSE: LazyLock<svg::Handle> = LazyLock::new(|| svg::Handle::from_memory(icons::CLOSE));

/// Looks up what to place the mini mixer against; may block on the display
/// server.
pub type Locator = fn(Option<ScreenRect>) -> Option<Anchoring>;

#[derive(Debug, Clone)]
pub enum MiniMessage {
    /// The window exists, at this scale factor.
    Created(window::Id, f32),
    /// What to place it against was looked up.
    Located(window::Id, Option<Anchoring>),
    /// Its button that opens the full window.
    OpenFull,
    /// Its close button.
    Close,
    /// The device picker's list opened (`true`) or closed.
    PickerMenu(bool),
}

impl App {
    /// Feeds an event to the mini window's rules and carries out what they
    /// decide.
    pub(super) fn mini_event(&mut self, event: Event) -> Task<Message> {
        let action = self.mini.handle(event, Instant::now());
        self.mini_action(action, None)
    }

    /// A tray click at `area`, or the tray menu's Mixer item without one.
    pub(super) fn mini_toggle(&mut self, area: Option<ScreenRect>) -> Task<Message> {
        let action = self.mini.handle(Event::Toggle, Instant::now());
        self.mini_action(action, area)
    }

    fn mini_action(&mut self, action: Action, area: Option<ScreenRect>) -> Task<Message> {
        match action {
            Action::None => Task::none(),
            Action::Open => self.open_mini(area),
            Action::Close(id, _) => {
                self.forget_window(id);
                window::close(id)
            }
            Action::Refocus(id) => window::gain_focus(id),
        }
    }

    pub(super) fn mini_update(&mut self, message: MiniMessage) -> Task<Message> {
        match message {
            MiniMessage::Created(id, scale) => self.mini_created(id, scale),
            MiniMessage::Located(id, anchoring) => self.show_mini(id, anchoring),
            MiniMessage::OpenFull => {
                let close = self.mini_event(Event::Close(CloseReason::OpenFull));
                Task::batch([close, self.open_window()])
            }
            MiniMessage::Close => self.mini_event(Event::Close(CloseReason::CloseButton)),
            MiniMessage::PickerMenu(open) => {
                self.mini_picker_open = open;
                Task::none()
            }
        }
    }

    /// Escape closes the mini mixer, unless it closes the device list.
    pub(super) fn mini_escape(&mut self, id: window::Id) -> Task<Message> {
        if self.mini.is(id) && self.mini_picker_open {
            // The list does not close on Escape itself; leaving both open
            // beats closing the window under it.
            tracing::debug!("mini mixer: Escape ignored while the device list is open");
            return Task::none();
        }
        self.mini_event(Event::Escape(id))
    }

    /// Opens the window. With a locator it starts hidden and is shown once
    /// placed: positions are converted at the scale of the window, which is
    /// only known once it exists.
    fn open_mini(&mut self, area: Option<ScreenRect>) -> Task<Message> {
        let height = mini::height(self.mini_body());
        let size = Size::new(style::MINI_WIDTH, height);
        let hidden = self.mini_locator.is_some();
        let (id, opened) = window::open(window::Settings {
            size,
            visible: !hidden,
            resizable: false,
            decorations: false,
            minimizable: false,
            level: window::Level::AlwaysOnTop,
            icon: icons::window_icon(),
            exit_on_close_request: false,
            #[cfg(target_os = "linux")]
            platform_specific: window::settings::PlatformSpecific {
                application_id: "pcvolumecontrol".to_owned(),
                ..Default::default()
            },
            #[cfg(target_os = "windows")]
            platform_specific: window::settings::PlatformSpecific {
                skip_taskbar: true,
                corner_preference: window::settings::platform::CornerPreference::Round,
                ..Default::default()
            },
            ..Default::default()
        });
        tracing::debug!("mini mixer: opening, {size:?}, click at {area:?}");
        self.mini.handle(Event::Opened(id), Instant::now());
        self.mini_height = height;
        self.mini_picker_open = false;
        self.mini_anchoring = None;
        self.mini_unplaced = hidden.then_some(area);
        self.request_icons();
        opened.then(|id| {
            window::scale_factor(id).map(move |f| Message::Mini(MiniMessage::Created(id, f)))
        })
    }

    fn mini_created(&mut self, id: window::Id, scale: f32) -> Task<Message> {
        let created = self.mini_event(Event::Created(id));
        if !self.mini.is(id) {
            return created;
        }
        tracing::debug!("mini mixer: created at scale {scale}");
        if scale.is_finite() && scale > 0.0 {
            self.window_scales.insert(id, scale);
            self.update_scale_factor();
        }
        let place = match (self.mini_unplaced.take(), self.mini_locator) {
            (Some(area), Some(locate)) => {
                window::run(id, placement::can_position).then(move |positionable| {
                    if !positionable {
                        tracing::debug!("mini mixer: the platform places it (Wayland)");
                        return Task::done(Message::Mini(MiniMessage::Located(id, None)));
                    }
                    Task::perform(
                        tokio::task::spawn_blocking(move || locate(area)),
                        move |found| Message::Mini(MiniMessage::Located(id, found.ok().flatten())),
                    )
                })
            }
            _ => window::gain_focus(id),
        };
        Task::batch([created, place, self.fit_mini()])
    }

    /// Moves the hidden window into place and shows it.
    fn show_mini(&mut self, id: window::Id, anchoring: Option<Anchoring>) -> Task<Message> {
        if !self.mini.is(id) {
            return Task::none();
        }
        self.mini_anchoring = anchoring;
        let show = window::set_mode(id, window::Mode::Windowed).chain(window::gain_focus(id));
        match anchoring {
            Some(anchoring) => {
                let position = anchoring.position(self.mini_size(), self.window_scale(id));
                tracing::debug!("mini mixer: shown at {position:?}");
                let place = window::move_to(id, position);
                // X11 window managers place a window as it maps and ignore
                // earlier moves; Win32 moves a hidden window as asked.
                if cfg!(target_os = "windows") {
                    place.chain(show)
                } else {
                    show.chain(place)
                }
            }
            None => show,
        }
    }

    fn mini_size(&self) -> Size {
        Size::new(style::MINI_WIDTH, self.mini_height)
    }

    /// The scale factor `id` last reported, 1 until it does.
    pub(super) fn window_scale(&self, id: window::Id) -> f32 {
        self.window_scales.get(&id).copied().unwrap_or(1.0)
    }

    fn mini_body(&self) -> Body {
        match &self.audio {
            None => Body::Loading,
            Some(audio) => match &audio.default_device {
                None => Body::NoDevice,
                Some(device) => Body::Rows(device.sessions.len()),
            },
        }
    }

    /// Fits the mini mixer to what it shows, once its window exists.
    pub(super) fn fit_mini(&mut self) -> Task<Message> {
        let Some(id) = self.mini.id().filter(|_| self.mini.is_created()) else {
            return Task::none();
        };
        let height = mini::height(self.mini_body());
        if height == self.mini_height {
            return Task::none();
        }
        tracing::debug!("mini mixer: height {} -> {height}", self.mini_height);
        let grows = height > self.mini_height;
        self.mini_height = height;
        let size = self.mini_size();
        // A window that is not resizable keeps its size limits, so they
        // move with it.
        let resize = window::set_min_size::<Message>(id, None)
            .chain(window::set_max_size(id, None))
            .chain(window::resize(id, size))
            .chain(window::set_min_size(id, Some(size)))
            .chain(window::set_max_size(id, Some(size)));
        let Some(anchoring) = &self.mini_anchoring else {
            return resize;
        };
        // Keeps it against the taskbar, moving first when it grows so it
        // never reaches over the taskbar.
        let place = window::move_to(id, anchoring.position(size, self.window_scale(id)));
        if grows {
            place.chain(resize)
        } else {
            resize.chain(place)
        }
    }

    /// Places the shown mini mixer again after its scale factor changed
    /// from `previous`: it moved to another monitor, whose scale is now its
    /// own.
    pub(super) fn mini_rescaled(&mut self, id: window::Id, previous: f32) -> Task<Message> {
        let scale = self.window_scale(id);
        let size = self.mini_size();
        let Some(anchoring) = &mut self.mini_anchoring else {
            return Task::none();
        };
        if (scale - previous).abs() < 0.01 {
            return Task::none();
        }
        tracing::debug!("mini mixer: scale {previous} -> {scale}; placing it again");
        anchoring.monitor.scale = None;
        window::move_to(id, anchoring.position(size, scale))
    }

    pub(super) fn mini_view(&self) -> Element<'_, Message> {
        let header = row![
            text("PcVolumeControl")
                .size(style::TEXT_SMALL)
                .style(style::muted_text),
            Space::new().width(Fill),
            header_button(&OPEN_WINDOW, "Open the full window", MiniMessage::OpenFull),
            header_button(&CLOSE, "Close", MiniMessage::Close),
        ]
        .spacing(style::SPACE_XS)
        .height(style::MINI_HEADER)
        .align_y(Center);
        let body: Element<'_, Message> = match &self.audio {
            None => fixed_height(
                text("Reading audio devices...")
                    .size(style::TEXT_COMPACT)
                    .style(style::muted_text),
                &style::COMPACT,
            ),
            Some(audio) => responsive(move |size| self.mini_layout(audio, size.width)).into(),
        };
        container(
            column![style::beside_gutter(header), body]
                .spacing(style::MINI_GAP)
                .height(Fill),
        )
        .padding(style::MINI_PADDING)
        .width(Fill)
        .height(Fill)
        .style(style::mini_frame)
        .into()
    }

    fn mini_layout<'a>(&'a self, audio: &'a AudioState, width: f32) -> Element<'a, Message> {
        let density = &style::COMPACT;
        let cols = MixerColumns::new(width, density);
        let picker = style::beside_gutter(device_picker(
            audio,
            &cols,
            density,
            Some(|open| Message::Mini(MiniMessage::PickerMenu(open))),
        ));
        let Some(device) = &audio.default_device else {
            return column![
                picker,
                style::beside_gutter(fixed_height(
                    text("No default playback device")
                        .size(density.text)
                        .style(style::muted_text),
                    density,
                )),
            ]
            .spacing(style::MINI_GAP)
            .into();
        };
        column![
            picker,
            style::beside_gutter(self.master_row(device, &cols, density)),
            style::beside_gutter(rule::horizontal(style::MINI_DIVIDER)),
            style::scroll_area(self.session_rows(device, &cols, density)),
        ]
        .spacing(style::MINI_GAP)
        .into()
    }
}

/// A small icon button in the header, named by its tooltip.
fn header_button<'a>(
    icon: &svg::Handle,
    label: &'static str,
    message: MiniMessage,
) -> Element<'a, Message> {
    let glyph = svg(icon.clone())
        .width(style::MINI_HEADER_ICON)
        .height(style::MINI_HEADER_ICON)
        .style(style::header_icon);
    let size = style::MINI_HEADER;
    with_tooltip(
        button(container(glyph).center(Fill))
            .width(size)
            .height(size)
            .padding(0)
            .style(style::button(Button::Icon))
            .on_press(Message::Mini(message)),
        Some(label),
    )
}
