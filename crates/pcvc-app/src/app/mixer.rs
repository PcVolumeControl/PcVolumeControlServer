//! Mixer view and its message handling.

use std::borrow::Cow;
use std::time::Instant;

use iced::widget::{
    Space, button, column, container, image, pick_list, responsive, row, slider, text, tooltip,
};
use iced::{Center, Element, Fill, Task};
use pcvc_audio::{AudioState, Device, DeviceInfo, Session};
use pcvc_protocol::Command;

use super::{App, Message};
use crate::app_icons::{Slot, fallback};
use crate::fit::fit;
use crate::mixer::{Target, control};
use crate::style::{self, Button};

#[derive(Debug, Clone)]
pub enum MixerMessage {
    SelectDevice(DeviceChoice),
    Slide(Target, f32),
    Release(Target),
    ToggleMute(Target),
    Tick,
}

/// Gives an icon tile an id the view tests can find it by.
#[cfg(test)]
fn tagged<'a>(
    tile: container::Container<'a, Message>,
    kind: &str,
    session: &Session,
) -> container::Container<'a, Message> {
    tile.id(format!("app-icon-{kind}:{}", session.id))
}

#[cfg(not(test))]
fn tagged<'a>(
    tile: container::Container<'a, Message>,
    _: &str,
    _: &Session,
) -> container::Container<'a, Message> {
    tile
}

/// A pick-list entry for a playback device. Entries are equal when their
/// device is, so the selected entry can show a shortened name while the
/// list shows full ones.
#[derive(Debug, Clone)]
pub struct DeviceChoice {
    id: String,
    label: String,
}

impl DeviceChoice {
    fn new(d: &DeviceInfo) -> Self {
        Self {
            id: d.id.clone(),
            label: d.name.clone(),
        }
    }

    fn with_label(self, label: String) -> Self {
        Self { label, ..self }
    }
}

impl PartialEq for DeviceChoice {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

impl App {
    fn default_device(&self) -> Option<&Device> {
        self.audio.as_ref()?.default_device.as_ref()
    }

    pub(super) fn mixer_update(&mut self, message: MixerMessage) -> Task<Message> {
        let now = Instant::now();
        match message {
            MixerMessage::SelectDevice(choice) => {
                if self.default_device().is_none_or(|d| d.id != choice.id) {
                    self.commands.send(Command::SetDefaultDevice {
                        device_id: choice.id,
                    });
                }
            }
            MixerMessage::Slide(target, volume) => {
                if let Some(volume) = self.mixer.slide(target.clone(), volume, now) {
                    self.send_volume(&target, volume);
                }
            }
            MixerMessage::Release(target) => {
                if let Some(volume) = self.mixer.release(&target, now) {
                    self.send_volume(&target, volume);
                }
            }
            MixerMessage::ToggleMute(target) => {
                let Some((actual_volume, actual_muted)) =
                    self.default_device().and_then(|d| control(d, &target))
                else {
                    return Task::none();
                };
                let muted = self.mixer.toggle_mute(target.clone(), actual_muted, now);
                let command = match &target {
                    Target::Master => Command::SetMaster {
                        volume: None,
                        muted: Some(muted),
                    },
                    Target::Session(id) => Command::SetSession {
                        session_id: id.clone(),
                        volume: self.mixer.volume(&target, actual_volume),
                        muted,
                    },
                };
                self.commands.send(command);
            }
            MixerMessage::Tick => {
                self.mixer_timer = None;
                for (target, volume) in self.mixer.tick(now) {
                    self.send_volume(&target, volume);
                }
            }
        }
        self.schedule_mixer_tick()
    }

    /// Sends a volume change. A session command must also carry the mute
    /// state, taken as currently shown, so an unanswered mute toggle is not
    /// undone.
    fn send_volume(&self, target: &Target, volume: f32) {
        let command = match target {
            Target::Master => Command::SetMaster {
                volume: Some(volume),
                muted: None,
            },
            Target::Session(id) => {
                let Some((_, actual_muted)) =
                    self.default_device().and_then(|d| control(d, target))
                else {
                    return;
                };
                Command::SetSession {
                    session_id: id.clone(),
                    volume,
                    muted: self.mixer.muted(target, actual_muted),
                }
            }
        };
        self.commands.send(command);
    }

    /// Wakes the mixer when it next has work, unless a wake-up at or before
    /// then is already scheduled.
    fn schedule_mixer_tick(&mut self) -> Task<Message> {
        let Some(deadline) = self.mixer.next_deadline() else {
            return Task::none();
        };
        if self.mixer_timer.is_some_and(|t| t <= deadline) {
            return Task::none();
        }
        self.mixer_timer = Some(deadline);
        Task::perform(
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)),
            |()| Message::Mixer(MixerMessage::Tick),
        )
    }

    pub(super) fn mixer_view(&self) -> Element<'_, Message> {
        let Some(audio) = &self.audio else {
            return text("Reading audio devices...").into();
        };
        // The columns depend on the width, and labels are shortened to fit.
        responsive(move |size| self.mixer_layout(audio, size.width)).into()
    }

    fn mixer_layout<'a>(&'a self, audio: &'a AudioState, width: f32) -> Element<'a, Message> {
        let cols = MixerColumns::new(width);
        let selected = audio.default_device.as_ref().and_then(|device| {
            let info = audio.devices.iter().find(|d| d.id == device.id)?;
            let label = fit(&info.name, style::TEXT_DEFAULT, cols.picker_text);
            Some(DeviceChoice::new(info).with_label(label.into_owned()))
        });
        let devices: Vec<DeviceChoice> = audio.devices.iter().map(DeviceChoice::new).collect();
        let picker = row![
            text("Output device").width(cols.label),
            pick_list(devices, selected, |c| Message::Mixer(
                MixerMessage::SelectDevice(c)
            ))
            .text_size(style::TEXT_DEFAULT)
            .padding([style::SPACE_S, style::SPACE_M])
            .width(Fill),
        ]
        .spacing(style::SPACE_M)
        .align_y(Center);

        let Some(device) = &audio.default_device else {
            return column![
                style::beside_gutter(container(picker).padding(style::SPACE_L).style(style::card)),
                text("No default playback device").style(style::muted_text),
            ]
            .spacing(style::SPACE_M)
            .into();
        };
        let master = self.volume_row(
            text("Master").size(style::TEXT_SECTION).into(),
            &cols,
            Target::Master,
            device.volume,
            device.muted,
        );
        let top = container(column![picker, master].spacing(style::SPACE_L))
            .padding(style::SPACE_L)
            .style(style::card);

        let sessions: Element<'_, Message> = if device.sessions.is_empty() {
            text("No applications are playing audio")
                .style(style::muted_text)
                .into()
        } else {
            column(device.sessions.iter().map(|s| {
                let muted = self.mixer.muted(&Target::Session(s.id.clone()), s.muted);
                let label = row![self.app_icon(s, muted), session_name(&s.name, cols.name)]
                    .spacing(style::APP_ICON_GAP)
                    .align_y(Center);
                self.volume_row(
                    label.into(),
                    &cols,
                    Target::Session(s.id.clone()),
                    s.volume,
                    s.muted,
                )
            }))
            .spacing(style::SPACE_M)
            .into()
        };

        column![
            style::beside_gutter(top),
            style::beside_gutter(
                text("Applications")
                    .size(style::TEXT_SMALL)
                    .style(style::muted_text)
            ),
            style::scroll_area(container(sessions).padding([0.0, style::SPACE_L])),
        ]
        .spacing(style::SPACE_M)
        .into()
    }

    /// The session's icon once found, the fallback tile when there is
    /// none, and an empty tile of the same size while it is looked up.
    /// Muted sessions show it faded.
    fn app_icon(&self, session: &Session, muted: bool) -> Element<'_, Message> {
        let size = style::APP_ICON;
        let opacity = if muted { style::MUTED_OPACITY } else { 1.0 };
        match self.session_icon(&session.id) {
            Slot::Ready(Some(handle)) => tagged(
                container(
                    image(handle.clone())
                        .width(size)
                        .height(size)
                        .border_radius(style::APP_ICON_RADIUS)
                        .opacity(opacity),
                ),
                "image",
                session,
            )
            .into(),
            Slot::Ready(None) => {
                let index = fallback::color_index(&session.name);
                let letter = text(fallback::initial(&session.name))
                    .size(style::APP_ICON_LETTER)
                    .style(move |theme| text::Style {
                        color: Some(fallback::colors(theme, index).text.scale_alpha(opacity)),
                    });
                let tile = container(letter).center(size).style(move |theme| {
                    style::icon_tile(fallback::colors(theme, index).color.scale_alpha(opacity))
                });
                tagged(tile, "fallback", session).into()
            }
            Slot::Pending => tagged(
                container(Space::new())
                    .width(size)
                    .height(size)
                    .style(style::icon_placeholder),
                "pending",
                session,
            )
            .into(),
        }
    }

    /// Label, slider, percentage and mute button. The label column has the
    /// same width in every row, so the sliders line up. A muted control
    /// shows a greyed slider and percentage and a highlighted Unmute button.
    fn volume_row<'a>(
        &self,
        label: Element<'a, Message>,
        cols: &MixerColumns,
        target: Target,
        actual: f32,
        muted: bool,
    ) -> Element<'a, Message> {
        let volume = self.mixer.volume(&target, actual);
        let muted = self.mixer.muted(&target, muted);
        let mute = button(text(if muted { "Unmute" } else { "Mute" }).center())
            .width(style::MUTE_WIDTH)
            .style(style::button(if muted {
                Button::Danger
            } else {
                Button::Secondary
            }))
            .on_press(Message::Mixer(MixerMessage::ToggleMute(target.clone())));
        let slide_target = target.clone();
        let fader = slider(0.0..=100.0, volume, move |v| {
            Message::Mixer(MixerMessage::Slide(slide_target.clone(), v))
        })
        .step(1.0_f32)
        .on_release(Message::Mixer(MixerMessage::Release(target)))
        .style(style::volume_slider(muted));
        let percent = text(format!("{volume:.0}%"))
            .width(style::PERCENT_WIDTH)
            .align_x(iced::Right);
        let percent = if muted {
            percent.style(style::muted_text)
        } else {
            percent
        };
        row![container(label).width(cols.label), fader, percent, mute]
            .spacing(style::SPACE_M)
            .align_y(Center)
            .into()
    }
}

/// A session name fitted to `width`, with the full name in a tooltip when
/// it had to be shortened.
fn session_name(name: &str, width: f32) -> Element<'_, Message> {
    match fit(name, style::TEXT_DEFAULT, width) {
        Cow::Borrowed(name) => text(name)
            .size(style::TEXT_DEFAULT)
            .wrapping(text::Wrapping::None)
            .into(),
        Cow::Owned(short) => tooltip(
            text(short)
                .size(style::TEXT_DEFAULT)
                .wrapping(text::Wrapping::None),
            container(text(name).size(style::TEXT_SMALL))
                .padding([style::SPACE_XS, style::SPACE_S])
                .style(style::tooltip),
            tooltip::Position::Top,
        )
        .delay(style::TOOLTIP_DELAY)
        .into(),
    }
}

/// Column widths of the Mixer rows for a page `width`. The label column
/// takes a share of the room left by the fixed columns, within bounds, and
/// the slider (or the device picker) the rest.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MixerColumns {
    /// The label column: "Output device", "Master", or icon and name.
    label: f32,
    /// Room for a session name after its icon.
    name: f32,
    /// Room for the device name inside the picker, beside its arrow.
    picker_text: f32,
}

impl MixerColumns {
    fn new(width: f32) -> Self {
        // Rows sit inside the card's padding, to the left of the gutter.
        let row = (width - style::SCROLL_GUTTER - 2.0 * style::SPACE_L).max(0.0);
        let fixed = style::PERCENT_WIDTH + style::MUTE_WIDTH + 3.0 * style::SPACE_M;
        let flexible = (row - fixed).max(0.0);
        let label = (flexible * style::LABEL_SHARE)
            .clamp(style::LABEL_MIN, style::LABEL_MAX)
            .min(flexible);
        let picker = (row - label - style::SPACE_M).max(0.0);
        Self {
            label,
            name: (label - style::APP_ICON - style::APP_ICON_GAP).max(0.0),
            // The picker's padding, and its arrow at the text size.
            picker_text: (picker - 2.0 * style::SPACE_M - style::TEXT_DEFAULT - style::SPACE_XS)
                .max(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_scale_within_bounds() {
        let narrow = MixerColumns::new(400.0);
        let default = MixerColumns::new(500.0);
        let wide = MixerColumns::new(1400.0);
        assert_eq!(narrow.label, style::LABEL_MIN);
        assert!(default.label > narrow.label && default.label < style::LABEL_MAX);
        assert_eq!(wide.label, style::LABEL_MAX);
        for cols in [narrow, default, wide] {
            assert_eq!(
                cols.name,
                cols.label - style::APP_ICON - style::APP_ICON_GAP
            );
            assert!(cols.picker_text > cols.label);
        }
        assert_eq!(MixerColumns::new(0.0).label, 0.0);
    }

    #[test]
    fn device_choices_compare_by_id() {
        let info = DeviceInfo {
            id: "sink".into(),
            name: "Arctis Nova Pro Wireless Analog Stereo".into(),
        };
        let full = DeviceChoice::new(&info);
        let short = DeviceChoice::new(&info).with_label("Arctis Nova\u{2026}".into());
        assert_eq!(full, short);
        assert_eq!(full.to_string(), info.name);
        assert_eq!(short.to_string(), "Arctis Nova\u{2026}");
    }
}
