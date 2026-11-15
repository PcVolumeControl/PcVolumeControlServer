//! Settings view: an editable copy of the config, applied on Save.

use std::net::{IpAddr, Ipv4Addr};

use iced::widget::{
    button, column, container, pick_list, row, scrollable, text, text_input, toggler,
};
use iced::{Center, Element, Fill, Task};

use super::{App, Message};
use crate::config::{Config, ThemeChoice};
use crate::server_ctl::ServerStatus;
use crate::style::{self, Button};
use crate::{autostart, config};

#[derive(Debug, Clone)]
pub enum SettingsMessage {
    Bind(BindChoice),
    Port(String),
    Mdns(bool),
    StartOnLogin(bool),
    ShowOnLaunch(bool),
    Theme(ThemeChoice),
    CheckForUpdates(bool),
    Save,
    Revert,
}

/// A bind address in the pick list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindChoice(pub IpAddr);

impl std::fmt::Display for BindChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_unspecified() {
            write!(f, "All interfaces ({})", self.0)
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// The bind addresses offered: all interfaces, each local IPv4 address, and
/// the configured address even if no interface has it any more.
pub fn bind_choices(local: &[Ipv4Addr], configured: IpAddr) -> Vec<BindChoice> {
    let mut choices = vec![BindChoice(IpAddr::V4(Ipv4Addr::UNSPECIFIED))];
    choices.extend(local.iter().map(|ip| BindChoice(IpAddr::V4(*ip))));
    if !choices.contains(&BindChoice(configured)) {
        choices.push(BindChoice(configured));
    }
    choices
}

/// The Settings page's unsaved edits.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    pub bind: IpAddr,
    pub port: String,
    pub mdns: bool,
    pub start_on_login: bool,
    pub show_on_launch: bool,
    pub theme: ThemeChoice,
    pub check_for_updates: bool,
}

impl From<&Config> for SettingsForm {
    fn from(config: &Config) -> Self {
        Self {
            bind: config.bind,
            port: config.port.to_string(),
            mdns: config.mdns,
            start_on_login: config.start_on_login,
            show_on_launch: config.show_on_launch,
            theme: config.theme,
            check_for_updates: config.check_for_updates,
        }
    }
}

pub fn parse_port(input: &str) -> Result<u16, String> {
    match input.trim().parse::<u16>() {
        Ok(0) | Err(_) => Err("Port must be a number from 1 to 65535".to_owned()),
        Ok(port) => Ok(port),
    }
}

impl SettingsForm {
    /// `config` with this form's edits applied.
    pub fn apply_to(&self, config: &Config) -> Result<Config, String> {
        Ok(Config {
            bind: self.bind,
            port: parse_port(&self.port)?,
            mdns: self.mdns,
            start_on_login: self.start_on_login,
            show_on_launch: self.show_on_launch,
            theme: self.theme,
            check_for_updates: self.check_for_updates,
            window: config.window,
        })
    }

    pub fn is_dirty(&self, config: &Config) -> bool {
        *self != Self::from(config)
    }
}

/// Feedback shown under the Save button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Saved,
    Error(String),
}

impl App {
    pub(super) fn settings_update(&mut self, message: SettingsMessage) -> Task<Message> {
        let form = &mut self.settings;
        match message {
            SettingsMessage::Bind(choice) => form.bind = choice.0,
            SettingsMessage::Port(port) => {
                form.port = port.chars().filter(char::is_ascii_digit).collect();
            }
            SettingsMessage::Mdns(on) => form.mdns = on,
            SettingsMessage::StartOnLogin(on) => form.start_on_login = on,
            SettingsMessage::ShowOnLaunch(on) => form.show_on_launch = on,
            SettingsMessage::Theme(theme) => form.theme = theme,
            SettingsMessage::CheckForUpdates(on) => form.check_for_updates = on,
            SettingsMessage::Revert => {
                *form = SettingsForm::from(&self.config);
                self.notice = None;
                return Task::none();
            }
            SettingsMessage::Save => {
                self.notice = Some(match self.save_settings() {
                    Ok(()) => Notice::Saved,
                    Err(e) => Notice::Error(e),
                });
                return Task::none();
            }
        }
        self.notice = None;
        Task::none()
    }

    fn save_settings(&mut self) -> Result<(), String> {
        let mut new = self.settings.apply_to(&self.config)?;
        let login = if new.start_on_login == self.config.start_on_login {
            Ok(())
        } else {
            let result = set_start_on_login(new.start_on_login);
            if result.is_err() {
                new.start_on_login = self.config.start_on_login;
            }
            result
        };
        let saved = self.apply_config(new);
        self.settings = SettingsForm::from(&self.config);
        login.and(saved)
    }

    /// Changes start on login from the tray, where failures can only be
    /// logged.
    pub(super) fn toggle_start_on_login(&mut self) {
        let enable = !self.config.start_on_login;
        if set_start_on_login(enable).is_ok() {
            self.settings.start_on_login = enable;
            let new = Config {
                start_on_login: enable,
                ..self.config.clone()
            };
            let _ = self.apply_config(new);
        }
    }

    /// Makes `new` the active config: persists it and restarts the server
    /// if its settings changed and it is meant to be running.
    pub(super) fn apply_config(&mut self, new: Config) -> Result<(), String> {
        let restart = self.config.server_changed(&new)
            && !matches!(self.server_status, ServerStatus::Stopped);
        self.config = new;
        if restart {
            tracing::info!("server settings changed; restarting");
            self.server.start(self.config.server_config());
        }
        self.persist_config()
    }

    /// Writes the config file; failures are logged and returned for display.
    pub(super) fn persist_config(&self) -> Result<(), String> {
        let Some(path) = &self.config_path else {
            return Ok(());
        };
        config::save(path, &self.config).map_err(|e| {
            tracing::error!("saving {} failed: {e}", path.display());
            format!("Could not save settings: {e}")
        })
    }

    pub(super) fn settings_view(&self) -> Element<'_, Message> {
        let form = &self.settings;
        let msg = |m| Message::Settings(m);
        let port_error = parse_port(&form.port).err();

        let server = column![
            setting(
                "Listen on",
                pick_list(
                    bind_choices(&self.local_ips, form.bind),
                    Some(BindChoice(form.bind)),
                    move |c| msg(SettingsMessage::Bind(c)),
                )
                .padding([style::SPACE_S, style::SPACE_M])
                .width(Fill),
            ),
            setting(
                "Port",
                column![
                    text_input("3000", &form.port)
                        .on_input(move |p| msg(SettingsMessage::Port(p)))
                        .padding([style::SPACE_S, style::SPACE_M])
                        .width(120),
                ]
                .push(
                    port_error
                        .clone()
                        .map(|e| text(e).size(style::TEXT_SMALL).style(text::danger)),
                )
                .spacing(style::SPACE_XS),
            ),
            switch("Advertise over mDNS", form.mdns, move |on| {
                msg(SettingsMessage::Mdns(on))
            }),
        ]
        .spacing(style::SPACE_L);

        let app = column![
            switch("Start on login", form.start_on_login, move |on| {
                msg(SettingsMessage::StartOnLogin(on))
            }),
            switch("Show window on launch", form.show_on_launch, move |on| {
                msg(SettingsMessage::ShowOnLaunch(on))
            }),
            switch(
                "Check for updates on launch",
                form.check_for_updates,
                move |on| msg(SettingsMessage::CheckForUpdates(on)),
            ),
            setting(
                "Theme",
                pick_list(ThemeChoice::ALL, Some(form.theme), move |t| {
                    msg(SettingsMessage::Theme(t))
                })
                .padding([style::SPACE_S, style::SPACE_M])
                .width(160),
            ),
        ]
        .spacing(style::SPACE_L);

        let dirty = form.is_dirty(&self.config);
        let save = button(text("Save"))
            .padding([style::SPACE_S, style::SPACE_L])
            .style(style::button(Button::Primary))
            .on_press_maybe((dirty && port_error.is_none()).then_some(msg(SettingsMessage::Save)));
        let revert = button(text("Revert"))
            .padding([style::SPACE_S, style::SPACE_L])
            .style(style::button(Button::Secondary))
            .on_press_maybe(dirty.then_some(msg(SettingsMessage::Revert)));
        let notice = match &self.notice {
            Some(Notice::Saved) => Some(text("Saved").style(text::success)),
            Some(Notice::Error(e)) => Some(text(e.as_str()).style(text::danger)),
            None => None,
        };
        let actions = row![save, revert]
            .push(notice.map(|n| n.size(style::TEXT_SMALL)))
            .spacing(style::SPACE_M)
            .align_y(Center);

        let hint = text("Changing the address, port or mDNS restarts the server.")
            .size(style::TEXT_SMALL)
            .style(style::muted_text);

        // The actions stay visible below the scrolling groups.
        column![
            scrollable(
                column![group("Server", server), group("Application", app), hint]
                    .spacing(style::SPACE_L)
                    .padding(iced::Padding::ZERO.right(style::SPACE_M)),
            )
            .height(Fill),
            actions,
        ]
        .spacing(style::SPACE_L)
        .into()
    }
}

fn group<'a>(title: &'a str, body: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![
        text(title).size(style::TEXT_SMALL).style(style::muted_text),
        container(body)
            .padding(style::SPACE_L)
            .width(Fill)
            .style(style::card),
    ]
    .spacing(style::SPACE_S)
    .into()
}

fn setting<'a>(label: &'a str, control: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    row![text(label).width(style::LABEL_WIDTH), control.into()]
        .spacing(style::SPACE_M)
        .align_y(Center)
        .into()
}

fn switch<'a>(
    label: &'a str,
    on: bool,
    message: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        text(label).width(Fill),
        toggler(on).on_toggle(message).size(22),
    ]
    .spacing(style::SPACE_M)
    .align_y(Center)
    .into()
}

fn set_start_on_login(enabled: bool) -> Result<(), String> {
    autostart::set_enabled(enabled).map_err(|e| {
        tracing::warn!("changing start on login failed: {e}");
        format!("Could not change start on login: {e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_validation() {
        assert_eq!(parse_port("3000"), Ok(3000));
        assert_eq!(parse_port(" 65535 "), Ok(65535));
        for bad in ["", "0", "65536", "abc", "-1"] {
            assert!(parse_port(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn form_round_trip_and_dirty() {
        let config = Config::default();
        let mut form = SettingsForm::from(&config);
        assert!(!form.is_dirty(&config));
        assert_eq!(form.apply_to(&config), Ok(config.clone()));
        form.port = "4000".into();
        form.theme = ThemeChoice::Dark;
        assert!(form.is_dirty(&config));
        let applied = form.apply_to(&config).unwrap();
        assert_eq!((applied.port, applied.theme), (4000, ThemeChoice::Dark));
        assert_eq!(applied.window, config.window);
        form.port = "0".into();
        assert!(form.apply_to(&config).is_err());
    }

    #[test]
    fn bind_choices_keep_configured_address() {
        let lan = Ipv4Addr::new(192, 168, 1, 20);
        let gone: IpAddr = "10.0.0.9".parse().unwrap();
        let labels: Vec<String> = bind_choices(&[lan], gone)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            labels,
            ["All interfaces (0.0.0.0)", "192.168.1.20", "10.0.0.9"]
        );
        assert_eq!(bind_choices(&[lan], IpAddr::V4(lan)).len(), 2);
    }
}
