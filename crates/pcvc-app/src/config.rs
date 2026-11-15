//! User settings.

use std::net::{IpAddr, Ipv4Addr};

use pcvc_server::{DEFAULT_PORT, ServerConfig};

#[derive(Debug, Clone, Copy, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    pub mdns: bool,
    pub start_on_login: bool,
    pub show_on_launch: bool,
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
