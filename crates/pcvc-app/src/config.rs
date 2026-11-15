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

impl WindowSize {
    /// Below this the Mixer's columns no longer fit.
    pub const MIN: Self = Self {
        width: 680.0,
        height: 440.0,
    };
    const MAX: f32 = 16384.0;

    /// Within the window's limits; non-finite values give the default.
    pub fn sanitized(self) -> Self {
        if !(self.width.is_finite() && self.height.is_finite()) {
            return Self::default();
        }
        Self {
            width: self.width.clamp(Self::MIN.width, Self::MAX),
            height: self.height.clamp(Self::MIN.height, Self::MAX),
        }
    }
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

/// Settings with no control in the window, set by editing the file. They
/// are kept apart from [`Config`], which the Settings page rebuilds field by
/// field, and [`save`] leaves them in the file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileOnly {
    /// Serve application icons to phone clients (the protocol's v8
    /// `icons` feature). Read at startup.
    pub serve_icons: bool,
    /// Send each session's detail, such as a browser tab's title, to phone
    /// clients (the protocol's v8 `detail` feature). Read at startup; when
    /// off, the audio backend does not read titles at all.
    pub serve_details: bool,
}

impl Default for FileOnly {
    fn default() -> Self {
        Self {
            serve_icons: true,
            serve_details: true,
        }
    }
}

/// The outcome of [`load`]: a usable config plus anything worth logging.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub file_only: FileOnly,
    pub warning: Option<String>,
    /// `false` when the file exists but could not be read or moved aside;
    /// saving would then overwrite the user's settings with defaults.
    pub writable: bool,
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
                file_only: FileOnly::default(),
                warning: None,
                writable: true,
            };
        }
        Err(e) => {
            return Loaded {
                config: Config::default(),
                file_only: FileOnly::default(),
                warning: Some(format!(
                    "could not read {}: {e}; using defaults and not saving",
                    path.display()
                )),
                writable: false,
            };
        }
    };
    match toml::from_str::<Config>(&text) {
        Ok(mut config) => {
            config.window = config.window.sanitized();
            let mut warnings = Vec::new();
            if config.port == 0 {
                config.port = DEFAULT_PORT;
                warnings.push(format!(
                    "port 0 in {} replaced by {DEFAULT_PORT}",
                    path.display()
                ));
            }
            let file_only = toml::from_str::<FileOnly>(&text).unwrap_or_else(|e| {
                warnings.push(format!(
                    "ignoring invalid settings in {}: {e}",
                    path.display()
                ));
                FileOnly::default()
            });
            Loaded {
                config,
                file_only,
                warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
                writable: true,
            }
        }
        Err(e) => {
            let backup = backup_path(path);
            let (moved, writable) = match std::fs::rename(path, &backup) {
                Ok(()) => (format!("moved to {}", backup.display()), true),
                Err(e) => (format!("could not be moved aside: {e}; not saving"), false),
            };
            Loaded {
                config: Config::default(),
                file_only: FileOnly::default(),
                warning: Some(format!(
                    "invalid config {} ({moved}); using defaults: {e}",
                    path.display()
                )),
                writable,
            }
        }
    }
}

/// Adds the keys of `old` that `new` lacks, in nested tables too.
fn keep_missing(new: &mut toml::Table, old: toml::Table) {
    for (key, old_value) in old {
        match (new.get_mut(&key), old_value) {
            (None, old_value) => {
                new.insert(key, old_value);
            }
            (Some(toml::Value::Table(new)), toml::Value::Table(old)) => keep_missing(new, old),
            (Some(_), _) => {}
        }
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".corrupt-{stamp}"));
    path.with_file_name(name)
}

/// Writes the config atomically (synced temporary file, then rename),
/// creating the directory if needed. Keys in the existing file that
/// `config` does not have ([`FileOnly`] settings, or settings of a newer
/// version) are kept.
pub fn save(path: &Path, config: &Config) -> io::Result<()> {
    use std::io::Write as _;

    let mut table = toml::Table::try_from(config).map_err(io::Error::other)?;
    if let Some(old) = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
    {
        keep_missing(&mut table, old);
    }
    let text = toml::to_string_pretty(&table).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
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
    fn file_only_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // A file from before the setting existed.
        std::fs::write(&path, "port = 4000\n").unwrap();
        let loaded = load(&path);
        assert!(loaded.file_only.serve_icons, "on by default");
        assert!(loaded.file_only.serve_details, "on by default");
        assert!(loaded.warning.is_none());

        std::fs::write(
            &path,
            "port = 4000\nserve_icons = false\nserve_details = false\nfuture = 1\n[window]\nwidth = 900.0\nlater = true\n",
        )
        .unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.config.port, 4000);
        assert!(!loaded.file_only.serve_icons);
        assert!(!loaded.file_only.serve_details);
        // Saving from the Settings page keeps it, and keys it does not know.
        let edited = Config {
            port: 4001,
            ..loaded.config
        };
        save(&path, &edited).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.config, edited);
        assert!(!loaded.file_only.serve_icons);
        assert!(!loaded.file_only.serve_details);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("future = 1"));
        assert!(text.contains("later = true"), "nested keys too:\n{text}");

        std::fs::write(&path, "port = 4000\nserve_icons = \"no\"\n").unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.config.port, 4000, "the rest still loads");
        assert!(loaded.file_only.serve_icons);
        assert!(loaded.warning.unwrap().contains("serve_icons"));
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
    fn window_size_is_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[window]\nwidth = 0.0\nheight = 1e9\n").unwrap();
        let window = load(&path).config.window;
        assert_eq!(
            (window.width, window.height),
            (WindowSize::MIN.width, 16384.0)
        );
        assert_eq!(
            WindowSize {
                width: f32::NAN,
                height: 500.0
            }
            .sanitized(),
            WindowSize::default()
        );
    }

    #[test]
    fn unreadable_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        let loaded = load(&path);
        assert!(!loaded.writable);
        assert!(loaded.warning.is_some());
        assert!(path.exists());
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
