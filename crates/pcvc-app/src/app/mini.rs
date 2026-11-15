//! The mini mixer: a small borderless window opened from the tray, with the
//! Mixer's device picker, master row and application rows at the compact
//! density. It closes on Escape, on losing focus, from the tray and from
//! its own buttons; `crate::mini` decides when.

use std::time::{Duration, Instant};

use iced::widget::{Space, button, column, container, responsive, row, rule, svg, text};
use iced::{Center, Element, Fill, Size, Task, window};
use pcvc_audio::AudioState;

use super::mixer::{MixerColumns, device_picker, fixed_height, with_tooltip};
use super::{App, Message};
use crate::icons;
use crate::mini::{self, Action, CloseReason, Event};
use crate::style::{self, Button};

/// How long after opening a window that has not had focus is logged as
/// such.
const FOCUS_CHECK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub enum MiniMessage {
    /// Its button that opens the full window.
    OpenFull,
    /// Its close button.
    Close,
    /// Logs whether the window has had focus by now.
    FocusCheck(window::Id),
}

impl App {
    /// Feeds an event to the mini window's rules and carries out what they
    /// decide.
    pub(super) fn mini_event(&mut self, event: Event) -> Task<Message> {
        match self.mini.handle(event, Instant::now()) {
            Action::None => Task::none(),
            Action::Open => self.open_mini(),
            Action::Close(id, reason) => {
                tracing::debug!("mini mixer: closing ({reason:?})");
                self.forget_window(id);
                window::close(id)
            }
            Action::Refocus(id) => window::gain_focus(id),
        }
    }

    pub(super) fn mini_update(&mut self, message: MiniMessage) -> Task<Message> {
        match message {
            MiniMessage::OpenFull => {
                let close = self.mini_event(Event::Close(CloseReason::OpenFull));
                Task::batch([close, self.open_window()])
            }
            MiniMessage::Close => self.mini_event(Event::Close(CloseReason::CloseButton)),
            MiniMessage::FocusCheck(id) => {
                if self.mini.is(id) && !self.mini.has_had_focus() {
                    tracing::debug!(
                        "mini mixer: no focus {FOCUS_CHECK:?} after opening; close it with \
                         Escape after clicking into it, the tray, or its close button"
                    );
                }
                Task::none()
            }
        }
    }

    fn open_mini(&mut self) -> Task<Message> {
        let height = mini::height(self.mini_rows());
        let size = Size::new(style::MINI_WIDTH, height);
        let (id, opened) = window::open(window::Settings {
            size,
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
        tracing::debug!("mini mixer: opening, {size:?}");
        self.mini.handle(Event::Opened(id), Instant::now());
        self.mini_height = height;
        self.request_icons();
        let check = Task::perform(tokio::time::sleep(FOCUS_CHECK), move |()| {
            Message::Mini(MiniMessage::FocusCheck(id))
        });
        let opened = opened.then(|id| {
            Task::batch([
                window::gain_focus(id),
                window::scale_factor(id).map(move |f| Message::ScaleFactor(id, f)),
            ])
        });
        Task::batch([opened, check])
    }

    /// Application rows the mini mixer shows.
    fn mini_rows(&self) -> usize {
        self.audio
            .as_ref()
            .and_then(|a| a.default_device.as_ref())
            .map_or(0, |d| d.sessions.len())
    }

    /// Fits the open mini mixer to its rows after they changed.
    pub(super) fn fit_mini(&mut self) -> Task<Message> {
        let Some(id) = self.mini.id() else {
            return Task::none();
        };
        let height = mini::height(self.mini_rows());
        if height == self.mini_height {
            return Task::none();
        }
        tracing::debug!("mini mixer: height {} -> {height}", self.mini_height);
        self.mini_height = height;
        let size = Size::new(style::MINI_WIDTH, height);
        // A window that is not resizable keeps its size limits, so they
        // move with it.
        window::set_min_size::<Message>(id, None)
            .chain(window::set_max_size(id, None))
            .chain(window::resize(id, size))
            .chain(window::set_min_size(id, Some(size)))
            .chain(window::set_max_size(id, Some(size)))
    }

    pub(super) fn mini_view(&self) -> Element<'_, Message> {
        let header = row![
            text("PcVolumeControl")
                .size(style::TEXT_SMALL)
                .style(style::muted_text),
            Space::new().width(Fill),
            header_button(
                icons::OPEN_WINDOW,
                "Open the full window",
                MiniMessage::OpenFull
            ),
            header_button(icons::CLOSE, "Close", MiniMessage::Close),
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
        let picker = style::beside_gutter(device_picker(audio, &cols, density));
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
    icon: &'static [u8],
    label: &'static str,
    message: MiniMessage,
) -> Element<'a, Message> {
    let glyph = svg(svg::Handle::from_memory(icon))
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
