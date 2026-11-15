//! User settings.

use std::net::{IpAddr, Ipv4Addr};

use pcvc_server::{DEFAULT_PORT, ServerConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    /// `None` lets iced follow the system color scheme.
    pub fn theme(self) -> Option<iced::Theme> {
        match self {
            Self::System => None,
            Self::Light => Some(iced::Theme::Light),
            Self::Dark => Some(iced::Theme::Dark),
        }
    }
}

impl std::fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowSize {
    pub width: f32,
    pub height: f32,
}

impl Default for WindowSize {
    fn default() -> Self {
        Self {
            width: 720.0,
            height: 560.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    pub mdns: bool,
    pub start_on_login: bool,
    pub show_on_launch: bool,
    pub theme: ThemeChoice,
    pub check_for_updates: bool,
    pub window: WindowSize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_PORT,
            mdns: true,
            start_on_login: false,
            show_on_launch: false,
            theme: ThemeChoice::System,
            check_for_updates: false,
            window: WindowSize::default(),
        }
    }
}

impl Config {
    pub fn server_config(&self) -> ServerConfig {
        ServerConfig::new(self.bind, self.port).with_mdns(self.mdns)
    }
}
