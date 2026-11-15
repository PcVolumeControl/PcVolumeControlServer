//! Mixer view and its message handling.

use std::time::Instant;

use iced::widget::{button, column, container, pick_list, row, scrollable, slider, text};
use iced::{Center, Element, Fill, Task};
use pcvc_audio::{Device, DeviceInfo};
use pcvc_protocol::Command;

use super::{App, Message};
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

/// A pick-list entry for a playback device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceChoice {
    id: String,
    name: String,
}

impl From<&DeviceInfo> for DeviceChoice {
    fn from(d: &DeviceInfo) -> Self {
        Self {
            id: d.id.clone(),
            name: d.name.clone(),
        }
    }
}

impl std::fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
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
        let devices: Vec<DeviceChoice> = audio.devices.iter().map(DeviceChoice::from).collect();
        let selected = audio
            .default_device
            .as_ref()
            .and_then(|device| devices.iter().find(|d| d.id == device.id).cloned());
        let picker = row![
            text("Output device").width(style::LABEL_WIDTH),
            pick_list(devices, selected, |c| Message::Mixer(
                MixerMessage::SelectDevice(c)
            ))
            .padding([style::SPACE_S, style::SPACE_M])
            .width(Fill),
        ]
        .spacing(style::SPACE_M)
        .align_y(Center);

        let Some(device) = &audio.default_device else {
            return column![
                container(picker).padding(style::SPACE_L).style(style::card),
                text("No default playback device").style(style::muted_text),
            ]
            .spacing(style::SPACE_M)
            .into();
        };
        let master = self.volume_row(
            text("Master").size(style::TEXT_SECTION),
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
                self.volume_row(
                    text(&s.name).wrapping(text::Wrapping::None),
                    Target::Session(s.id.clone()),
                    s.volume,
                    s.muted,
                )
            }))
            .spacing(style::SPACE_M)
            .into()
        };

        column![
            top,
            text("Applications")
                .size(style::TEXT_SMALL)
                .style(style::muted_text),
            scrollable(
                container(sessions).padding(
                    iced::Padding::ZERO
                        .left(style::SPACE_L)
                        .right(style::SPACE_L)
                )
            )
            .height(Fill),
        ]
        .spacing(style::SPACE_M)
        .into()
    }

    fn volume_row<'a>(
        &self,
        label: impl Into<Element<'a, Message>>,
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
        .on_release(Message::Mixer(MixerMessage::Release(target)));
        row![
            container(label.into()).width(style::LABEL_WIDTH).clip(true),
            fader,
            text(format!("{volume:.0}%"))
                .width(style::PERCENT_WIDTH)
                .align_x(iced::Right),
            mute,
        ]
        .spacing(style::SPACE_M)
        .align_y(Center)
        .into()
    }
}
