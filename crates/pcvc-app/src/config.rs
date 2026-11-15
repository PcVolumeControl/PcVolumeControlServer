//! User settings, persisted as TOML.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

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
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

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
#[serde(default)]
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

/// Every field has a default, so a file written by an older or newer
/// version still loads.
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

    /// Whether going from `self` to `other` needs a server restart.
    pub fn server_changed(&self, other: &Self) -> bool {
        (self.bind, self.port, self.mdns) != (other.bind, other.port, other.mdns)
    }
}

/// The outcome of [`load`]: a usable config plus anything worth logging.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warning: Option<String>,
}

/// Reads the config at `path`. A missing file gives the defaults; an
/// unreadable or invalid one gives the defaults and a warning, and an
/// invalid one is renamed aside so it is not overwritten by the next save.
pub fn load(path: &Path) -> Loaded {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Loaded {
                config: Config::default(),
                warning: None,
            };
        }
        Err(e) => {
            return Loaded {
                config: Config::default(),
                warning: Some(format!(
                    "could not read {}: {e}; using defaults",
                    path.display()
                )),
            };
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(config) if config.port != 0 => Loaded {
            config,
            warning: None,
        },
        Ok(config) => Loaded {
            config: Config {
                port: DEFAULT_PORT,
                ..config
            },
            warning: Some(format!(
                "port 0 in {} replaced by {DEFAULT_PORT}",
                path.display()
            )),
        },
        Err(e) => {
            let backup = backup_path(path);
            let moved = match std::fs::rename(path, &backup) {
                Ok(()) => format!("moved to {}", backup.display()),
                Err(e) => format!("could not be moved aside: {e}"),
            };
            Loaded {
                config: Config::default(),
                warning: Some(format!(
                    "invalid config {} ({moved}); using defaults: {e}",
                    path.display()
                )),
            }
        }
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".corrupt-{stamp}"));
    path.with_file_name(name)
}

/// Writes the config atomically (temporary file, then rename), creating
/// the directory if needed.
pub fn save(path: &Path, config: &Config) -> io::Result<()> {
    let text = toml::to_string_pretty(config).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(&dir.path().join("config.toml"));
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let config = Config {
            bind: "192.168.1.20".parse().unwrap(),
            port: 4000,
            mdns: false,
            start_on_login: true,
            show_on_launch: true,
            theme: ThemeChoice::Dark,
            check_for_updates: true,
            window: WindowSize {
                width: 800.0,
                height: 600.0,
            },
        };
        save(&path, &config).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.config, config);
        assert!(loaded.warning.is_none());
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 4000\ntheme = \"light\"\nunknown = 1\n").unwrap();
        let loaded = load(&path);
        assert_eq!(
            loaded.config,
            Config {
                port: 4000,
                theme: ThemeChoice::Light,
                ..Config::default()
            }
        );
    }

    #[test]
    fn corrupt_file_is_backed_up_and_defaults_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = \"not a number\"").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warning.unwrap().contains("moved to"));
        assert!(!path.exists());
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].starts_with("config.toml.corrupt-"));
    }

    #[test]
    fn port_zero_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 0\nmdns = false\n").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.config.port, DEFAULT_PORT);
        assert!(!loaded.config.mdns);
        assert!(loaded.warning.is_some());
    }

    #[test]
    fn server_changes() {
        let a = Config::default();
        assert!(!a.server_changed(&Config {
            theme: ThemeChoice::Dark,
            ..a.clone()
        }));
        assert!(a.server_changed(&Config {
            mdns: false,
            ..a.clone()
        }));
    }
}
