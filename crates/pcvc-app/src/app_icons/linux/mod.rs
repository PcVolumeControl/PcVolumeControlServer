//! Finding a session's icon on Linux from desktop entries and icon themes.

mod desktop;
mod steam;
mod theme;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use desktop::DesktopIndex;
use theme::IconThemes;

use super::{Request, Resolver, raster};
use crate::icons::Rgba;

/// Longest wait for `gsettings`.
const GSETTINGS_TIMEOUT: Duration = Duration::from_secs(2);

/// Where to look. Built from the environment in production and from a
/// fixture tree in tests.
#[derive(Debug, Clone, Default)]
pub struct Dirs {
    pub home: Option<PathBuf>,
    /// `$XDG_DATA_HOME`.
    pub data_home: Option<PathBuf>,
    /// `$XDG_DATA_DIRS`.
    pub data_dirs: Vec<PathBuf>,
    /// `$XDG_CONFIG_HOME`, for the GTK and KDE theme settings.
    pub config_home: Option<PathBuf>,
    /// Data directories outside the XDG list: Flatpak and Snap exports.
    pub extra_data_dirs: Vec<PathBuf>,
    pub pixmaps: Vec<PathBuf>,
    pub proc_root: PathBuf,
    /// The icon theme; `None` asks the desktop.
    pub theme: Option<String>,
}

impl Dirs {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
        let home = var("HOME").map(PathBuf::from);
        let data_home = var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        let data_dirs = var("XDG_DATA_DIRS")
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_else(|| vec!["/usr/local/share".into(), "/usr/share".into()]);
        let config_home = var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let mut extra_data_dirs: Vec<PathBuf> = data_home
            .iter()
            .map(|d| d.join("flatpak/exports/share"))
            .collect();
        extra_data_dirs.push("/var/lib/flatpak/exports/share".into());
        extra_data_dirs.push("/var/lib/snapd/desktop".into());
        Self {
            home,
            data_home,
            data_dirs,
            config_home,
            extra_data_dirs,
            pixmaps: vec!["/usr/share/pixmaps".into()],
            proc_root: "/proc".into(),
            theme: None,
        }
    }

    /// Data directories, most important first, without duplicates.
    fn data(&self) -> Vec<PathBuf> {
        let mut all: Vec<PathBuf> = Vec::new();
        let candidates = self
            .data_home
            .iter()
            .chain(&self.data_dirs)
            .chain(&self.extra_data_dirs);
        for dir in candidates {
            if dir.is_absolute() && !all.contains(dir) {
                all.push(dir.clone());
            }
        }
        all
    }

    fn application_dirs(&self) -> Vec<PathBuf> {
        self.data()
            .into_iter()
            .map(|d| d.join("applications"))
            .collect()
    }

    /// Icon theme base directories: `~/.icons`, then each data directory's
    /// `icons`.
    fn icon_bases(&self) -> Vec<PathBuf> {
        self.home
            .iter()
            .map(|h| h.join(".icons"))
            .chain(self.data().into_iter().map(|d| d.join("icons")))
            .filter(|d| d.is_dir())
            .collect()
    }

    fn current_theme(&self) -> String {
        if let Some(theme) = &self.theme {
            return theme.clone();
        }
        let config = |file: &str, parse: fn(&str) -> Option<String>| {
            let path = self.config_home.as_ref()?.join(file);
            parse(&std::fs::read_to_string(path).ok()?)
        };
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("KDE"));
        let kde_theme = || config("kdeglobals", theme::parse_kdeglobals);
        let detected = if kde { kde_theme() } else { None }
            .or_else(gsettings_theme)
            .or_else(|| config("gtk-3.0/settings.ini", theme::parse_gtk_settings))
            .or_else(|| config("gtk-4.0/settings.ini", theme::parse_gtk_settings))
            .or_else(kde_theme);
        detected.unwrap_or_else(|| "hicolor".to_owned())
    }
}

/// The GNOME icon theme setting, if `gsettings` answers in time.
fn gsettings_theme() -> Option<String> {
    let mut child = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + GSETTINGS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    theme::parse_gsettings(&out)
}

/// Resolves icons from desktop entries and icon themes. The desktop
/// entries and themes are read once, on first use.
pub struct LinuxResolver {
    dirs: Dirs,
    desktop: OnceLock<DesktopIndex>,
    themes: OnceLock<IconThemes>,
}

/// Which step found an icon, for the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    IconName,
    AppId,
    Steam,
    Binary,
    Name,
}

impl LinuxResolver {
    pub fn new(dirs: Dirs) -> Self {
        Self {
            dirs,
            desktop: OnceLock::new(),
            themes: OnceLock::new(),
        }
    }

    fn desktop(&self) -> &DesktopIndex {
        self.desktop.get_or_init(|| {
            let index = DesktopIndex::scan(&self.dirs.application_dirs());
            tracing::debug!(entries = index.len(), "indexed desktop entries");
            index
        })
    }

    fn themes(&self) -> &IconThemes {
        self.themes.get_or_init(|| {
            let current = self.dirs.current_theme();
            tracing::debug!(theme = %current, "icon theme");
            IconThemes::load(self.dirs.icon_bases(), self.dirs.pixmaps.clone(), &current)
        })
    }

    /// The file for an `Icon` value: an absolute path as is, otherwise a
    /// theme lookup. A file extension on a name is tolerated.
    fn icon_file(&self, icon: &str, px: u32) -> Option<PathBuf> {
        let icon = icon.trim();
        let path = Path::new(icon);
        if path.is_absolute() {
            return path.is_file().then(|| path.to_path_buf());
        }
        let name = ["png", "svg", "svgz", "xpm"]
            .iter()
            .find_map(|ext| icon.strip_suffix(&format!(".{ext}")))
            .unwrap_or(icon);
        self.themes().lookup(name, px)
    }

    /// The icon files to try for a request, in order, with the step that
    /// produced each. Computed lazily: later steps only run if earlier
    /// files are missing or unreadable.
    pub fn find(&self, request: &Request) -> Option<(PathBuf, Rgba, Via)> {
        let key = &request.key;
        let px = key.size;
        let attempt = |icon: Option<String>, via: Via| -> Option<(PathBuf, Rgba, Via)> {
            let path = self.icon_file(&icon?, px)?;
            let rgba = raster::load(&path, px)?;
            Some((path, rgba, via))
        };
        let desktop = || self.desktop();
        attempt(key.icon_name.as_deref().map(str::to_owned), Via::IconName)
            .or_else(|| {
                let app_id = key.app_id.as_deref()?;
                attempt(desktop().by_app_id(app_id).map(str::to_owned), Via::AppId)
            })
            .or_else(|| {
                let app = steam::app_id_of(&self.dirs.proc_root, request.process_id?)?;
                attempt(Some(steam::icon_name(app)), Via::Steam)
            })
            .or_else(|| {
                let binary = key.process_binary.as_deref()?;
                attempt(desktop().by_binary(binary).map(str::to_owned), Via::Binary)
            })
            .or_else(|| attempt(desktop().by_name(&key.name).map(str::to_owned), Via::Name))
    }
}

impl Resolver for LinuxResolver {
    fn resolve(&self, request: &Request) -> Option<Rgba> {
        let found = self.find(request);
        match &found {
            Some((path, _, via)) => tracing::debug!(
                app = %request.key.name,
                path = %path.display(),
                ?via,
                size = request.key.size,
                "resolved application icon"
            ),
            None => tracing::debug!(app = %request.key.name, "no application icon found"),
        }
        found.map(|(_, rgba, _)| rgba)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::app_icons::IconKey;

    /// A fixture XDG tree with a theme, desktop entries and a fake /proc.
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="#f00"/></svg>"##;

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let f = Self { _tmp: tmp, root };
            f.write(
                "usr/share/icons/hicolor/index.theme",
                "[Icon Theme]\nDirectories=48x48/apps,scalable/apps\n[48x48/apps]\nSize=48\nType=Fixed\n[scalable/apps]\nSize=48\nType=Scalable\nMinSize=8\nMaxSize=512\n",
            );
            for name in ["firefox", "steam", "declared", "flat-app", "named"] {
                f.write(
                    &format!("usr/share/icons/hicolor/scalable/apps/{name}.svg"),
                    SVG,
                );
            }
            f.write(
                "home/.local/share/icons/hicolor/48x48/apps/steam_icon_2950790.svg",
                SVG,
            );
            f.write("usr/share/pixmaps/broken.svg", "not svg");
            f.write("opt/app/icon.svg", SVG);
            let entry = |icon: &str, exec: &str, name: &str| {
                format!(
                    "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\nIcon={icon}\n"
                )
            };
            f.write(
                "usr/share/applications/firefox.desktop",
                &entry("firefox", "firefox %u", "Firefox Web Browser"),
            );
            f.write(
                "usr/share/applications/steam.desktop",
                &entry("steam", "steam %U", "Steam"),
            );
            f.write(
                "home/.local/share/applications/IRON NEST.desktop",
                &entry(
                    "steam_icon_2950790",
                    "steam steam://rungameid/2950790",
                    "IRON NEST",
                ),
            );
            f.write(
                "flatpak/exports/share/applications/org.example.Flat.desktop",
                &entry(
                    "flat-app",
                    "flatpak run --command=flat org.example.Flat",
                    "Flat",
                ),
            );
            f.write(
                "usr/share/applications/absolute.desktop",
                &entry(
                    &f.root.join("opt/app/icon.svg").display().to_string(),
                    "absolute-bin",
                    "Absolute",
                ),
            );
            f.write(
                "usr/share/applications/named.desktop",
                &entry("named", "something-else", "Named App"),
            );
            f.write(
                "usr/share/applications/broken.desktop",
                &entry("broken", "broken-bin", "Broken"),
            );
            f.write("proc/4242/environ", "SteamAppId=2950790\0");
            f
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn resolver(&self) -> LinuxResolver {
            LinuxResolver::new(Dirs {
                home: Some(self.root.join("home")),
                data_home: Some(self.root.join("home/.local/share")),
                data_dirs: vec![self.root.join("usr/share")],
                config_home: None,
                extra_data_dirs: vec![self.root.join("flatpak/exports/share")],
                pixmaps: vec![self.root.join("usr/share/pixmaps")],
                proc_root: self.root.join("proc"),
                theme: Some("Missing".into()),
            })
        }
    }

    fn request(name: &str, configure: impl FnOnce(&mut IconKey)) -> Request {
        let mut key = IconKey {
            name: Arc::from(name),
            icon_name: None,
            app_id: None,
            process_binary: None,
            size: 32,
        };
        configure(&mut key);
        Request {
            key,
            process_id: None,
        }
    }

    fn via(resolver: &LinuxResolver, request: &Request) -> Option<(String, Via)> {
        resolver.find(request).map(|(path, rgba, via)| {
            assert_eq!((rgba.width, rgba.height), (32, 32));
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                via,
            )
        })
    }

    #[test]
    fn resolution_order() {
        let f = Fixture::new();
        let r = f.resolver();
        let arc = |s: &str| Some(Arc::from(s));
        assert_eq!(
            via(
                &r,
                &request("Firefox", |k| k.process_binary = arc("firefox"))
            ),
            Some(("firefox.svg".into(), Via::Binary))
        );
        // A declared icon wins over everything else.
        assert_eq!(
            via(
                &r,
                &request("Firefox", |k| {
                    k.icon_name = arc("declared");
                    k.process_binary = arc("firefox");
                })
            ),
            Some(("declared.svg".into(), Via::IconName))
        );
        // An icon name that does not resolve falls through.
        assert_eq!(
            via(
                &r,
                &request("Firefox", |k| {
                    k.icon_name = arc("snap.telegram-desktop.");
                    k.process_binary = arc("firefox");
                })
            ),
            Some(("firefox.svg".into(), Via::Binary))
        );
        assert_eq!(
            via(&r, &request("Flat", |k| k.app_id = arc("org.example.Flat"))),
            Some(("flat-app.svg".into(), Via::AppId))
        );
        assert_eq!(
            via(&r, &request("x", |k| k.process_binary = arc("flat"))),
            Some(("flat-app.svg".into(), Via::Binary))
        );
        assert_eq!(
            via(&r, &request("Named App", |_| {})),
            Some(("named.svg".into(), Via::Name))
        );
        assert_eq!(
            via(
                &r,
                &request("x", |k| k.process_binary = arc("/usr/bin/absolute-bin"))
            ),
            Some(("icon.svg".into(), Via::Binary))
        );
        // The Steam client itself, despite every game running `steam`.
        assert_eq!(
            via(&r, &request("Steam", |k| k.process_binary = arc("steam"))),
            Some(("steam.svg".into(), Via::Binary))
        );
        // An unreadable icon file is skipped, leaving nothing here.
        assert_eq!(
            via(
                &r,
                &request("Broken", |k| k.process_binary = arc("broken-bin"))
            ),
            None
        );
        assert_eq!(via(&r, &request("Nothing", |_| {})), None);
    }

    #[test]
    fn steam_games_by_environment() {
        let f = Fixture::new();
        let r = f.resolver();
        let mut req = request("IronNest.exe", |k| {
            k.process_binary = Some(Arc::from("wine64-preloader"))
        });
        assert_eq!(via(&r, &req), None);
        req.process_id = Some(4242);
        assert_eq!(
            via(&r, &req),
            Some(("steam_icon_2950790.svg".into(), Via::Steam))
        );
        req.process_id = Some(1);
        assert_eq!(via(&r, &req), None);
    }

    #[test]
    fn absolute_icon_names() {
        let f = Fixture::new();
        let r = f.resolver();
        let path = f.root.join("opt/app/icon.svg").display().to_string();
        let req = request("x", |k| k.icon_name = Some(Arc::from(path.as_str())));
        assert_eq!(via(&r, &req), Some(("icon.svg".into(), Via::IconName)));
        let req = request("x", |k| {
            k.icon_name = Some(Arc::from("/nonexistent/icon.png"))
        });
        assert_eq!(via(&r, &req), None);
        let req = request("x", |k| k.icon_name = Some(Arc::from("firefox.png")));
        assert_eq!(via(&r, &req), Some(("firefox.svg".into(), Via::IconName)));
    }

    #[test]
    fn data_dirs_are_ordered_and_deduplicated() {
        let dirs = Dirs {
            data_home: Some("/home/u/.local/share".into()),
            data_dirs: vec!["/usr/share".into(), "relative".into(), "/usr/share".into()],
            extra_data_dirs: vec!["/var/lib/flatpak/exports/share".into()],
            ..Dirs::default()
        };
        assert_eq!(
            dirs.application_dirs(),
            [
                "/home/u/.local/share/applications",
                "/usr/share/applications",
                "/var/lib/flatpak/exports/share/applications",
            ]
            .map(PathBuf::from)
        );
    }
}
