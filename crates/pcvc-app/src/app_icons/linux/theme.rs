//! Icon name to file, following the freedesktop icon theme specification:
//! the current theme, the themes it inherits, `hicolor`, then the pixmap
//! directories.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Extensions tried, best first. XPM is not supported.
const EXTENSIONS: [&str; 3] = ["svg", "svgz", "png"];

const FALLBACK_THEME: &str = "hicolor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirKind {
    Fixed,
    Scalable,
    Threshold,
}

/// One `Directories` entry of an `index.theme`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemeDir {
    path: String,
    size: u32,
    scale: u32,
    kind: DirKind,
    min: u32,
    max: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Theme {
    inherits: Vec<String>,
    dirs: Vec<ThemeDir>,
}

/// Parses an `index.theme`.
fn parse_index(text: &str) -> Theme {
    let mut groups: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Some(name.to_owned());
            groups.entry(name.to_owned()).or_default();
        } else if let (Some(group), Some((key, value))) = (&current, line.split_once('='))
            && !line.starts_with('#')
        {
            groups
                .entry(group.clone())
                .or_default()
                .entry(key.trim().to_owned())
                .or_insert_with(|| value.trim().to_owned());
        }
    }
    let Some(main) = groups.get("Icon Theme") else {
        return Theme::default();
    };
    let list = |key: &str| -> Vec<String> {
        main.get(key)
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut seen = HashSet::new();
    let dirs = list("Directories")
        .into_iter()
        .chain(list("ScaledDirectories"))
        .filter(|d| seen.insert(d.clone()))
        .filter_map(|path| {
            let group = groups.get(&path)?;
            let num = |key: &str| group.get(key).and_then(|v| v.parse::<u32>().ok());
            let size = num("Size")?;
            let kind = match group.get("Type").map(String::as_str) {
                Some("Fixed") => DirKind::Fixed,
                Some("Scalable") => DirKind::Scalable,
                _ => DirKind::Threshold,
            };
            let threshold = num("Threshold").unwrap_or(2);
            let (min, max) = match kind {
                DirKind::Fixed => (size, size),
                DirKind::Scalable => (
                    num("MinSize").unwrap_or(size),
                    num("MaxSize").unwrap_or(size),
                ),
                DirKind::Threshold => (size.saturating_sub(threshold), size + threshold),
            };
            Some(ThemeDir {
                path,
                size,
                scale: num("Scale").unwrap_or(1).max(1),
                kind,
                min,
                max,
            })
        })
        .collect();
    Theme {
        inherits: list("Inherits"),
        dirs,
    }
}

/// How well a file fits a request for `px` pixels; lower is better. Vector
/// files fit any size; otherwise the smallest size at or above `px` wins,
/// then the largest below it.
fn score(dir: &ThemeDir, ext: &str, px: u32) -> (u8, u32) {
    if ext != "png" {
        return (0, 0);
    }
    let lo = dir.min.saturating_mul(dir.scale);
    let hi = dir.max.saturating_mul(dir.scale);
    if (lo..=hi).contains(&px) && dir.kind != DirKind::Fixed {
        return (1, 0);
    }
    let actual = dir.size.saturating_mul(dir.scale);
    if actual >= px {
        (1, actual - px)
    } else {
        (2, px - actual)
    }
}

/// Icon themes found below a set of base directories (`~/.icons`,
/// `$XDG_DATA_DIRS/icons`, ...), parsed on first use.
#[derive(Debug)]
pub struct IconThemes {
    bases: Vec<PathBuf>,
    pixmaps: Vec<PathBuf>,
    /// The theme chain, current theme first and `hicolor` last.
    chain: Vec<String>,
    themes: HashMap<String, Theme>,
}

impl IconThemes {
    /// Loads `current` and every theme it inherits from `bases`.
    pub fn load(bases: Vec<PathBuf>, pixmaps: Vec<PathBuf>, current: &str) -> Self {
        let mut themes = HashMap::new();
        let mut chain = Vec::new();
        let mut pending = vec![current.to_owned()];
        while let Some(name) = pending.pop() {
            if chain.contains(&name) || name.contains('/') || name.is_empty() {
                continue;
            }
            let Some(theme) = bases
                .iter()
                .find_map(|b| std::fs::read_to_string(b.join(&name).join("index.theme")).ok())
                .map(|text| parse_index(&text))
            else {
                continue;
            };
            // Depth first, in declaration order.
            pending.extend(theme.inherits.iter().rev().cloned());
            chain.push(name.clone());
            themes.insert(name, theme);
        }
        if !chain.iter().any(|t| t == FALLBACK_THEME) {
            let fallback = bases
                .iter()
                .find_map(|b| {
                    std::fs::read_to_string(b.join(FALLBACK_THEME).join("index.theme")).ok()
                })
                .map(|text| parse_index(&text));
            if let Some(theme) = fallback {
                chain.push(FALLBACK_THEME.to_owned());
                themes.insert(FALLBACK_THEME.to_owned(), theme);
            }
        } else if let Some(pos) = chain.iter().position(|t| t == FALLBACK_THEME) {
            let hicolor = chain.remove(pos);
            chain.push(hicolor);
        }
        Self {
            bases,
            pixmaps,
            chain,
            themes,
        }
    }

    #[cfg(test)]
    fn chain(&self) -> &[String] {
        &self.chain
    }

    /// The file for icon `name` at `px` physical pixels: the best fit in the
    /// first theme of the chain that has the icon, else a pixmap.
    pub fn lookup(&self, name: &str, px: u32) -> Option<PathBuf> {
        if name.is_empty() || name.contains('/') {
            return None;
        }
        for theme_name in &self.chain {
            let theme = &self.themes[theme_name];
            let mut best: Option<((u8, u32), PathBuf)> = None;
            for dir in &theme.dirs {
                for base in &self.bases {
                    let folder = base.join(theme_name).join(&dir.path);
                    for ext in EXTENSIONS {
                        let path = folder.join(format!("{name}.{ext}"));
                        if !path.is_file() {
                            continue;
                        }
                        let s = score(dir, ext, px);
                        if best.as_ref().is_none_or(|(b, _)| s < *b) {
                            best = Some((s, path));
                        }
                    }
                }
            }
            if let Some((_, path)) = best {
                return Some(path);
            }
        }
        self.pixmaps.iter().find_map(|dir| {
            EXTENSIONS
                .iter()
                .map(|ext| dir.join(format!("{name}.{ext}")))
                .find(|p| p.is_file())
        })
    }
}

/// `gtk-icon-theme-name` from a GTK `settings.ini`.
pub fn parse_gtk_settings(text: &str) -> Option<String> {
    ini_value(text, "Settings", "gtk-icon-theme-name")
}

/// `[Icons] Theme` from KDE's `kdeglobals`.
pub fn parse_kdeglobals(text: &str) -> Option<String> {
    ini_value(text, "Icons", "Theme")
}

fn ini_value(text: &str, group: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = name == group;
        } else if in_group
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            let v = v.trim().trim_matches('"');
            return (!v.is_empty()).then(|| v.to_owned());
        }
    }
    None
}

/// The output of `gsettings get ... icon-theme`: a quoted string.
pub fn parse_gsettings(output: &str) -> Option<String> {
    let v = output.trim().trim_matches(|c| c == '\'' || c == '"').trim();
    (!v.is_empty()).then(|| v.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const INDEX: &str = "\
[Icon Theme]
Name=Test
Inherits=Parent,hicolor
Directories=16x16/apps,48x48/apps,48x48@2x/apps,scalable/apps,bogus

[16x16/apps]
Size=16
Type=Fixed

[48x48/apps]
Size=48
Type=Fixed

[48x48@2x/apps]
Size=48
Scale=2
Type=Fixed

[scalable/apps]
Size=48
MinSize=8
MaxSize=512
Type=Scalable
";

    #[test]
    fn parses_index_theme() {
        let theme = parse_index(INDEX);
        assert_eq!(theme.inherits, vec!["Parent", "hicolor"]);
        assert_eq!(
            theme.dirs.len(),
            4,
            "a directory without a group is dropped"
        );
        assert_eq!(theme.dirs[2].scale, 2);
        assert_eq!(theme.dirs[3].kind, DirKind::Scalable);
        assert_eq!((theme.dirs[3].min, theme.dirs[3].max), (8, 512));
        let threshold = parse_index("[Icon Theme]\nDirectories=a\n[a]\nSize=32\n");
        assert_eq!(threshold.dirs[0].kind, DirKind::Threshold);
        assert_eq!((threshold.dirs[0].min, threshold.dirs[0].max), (30, 34));
        assert_eq!(parse_index("garbage"), Theme::default());
    }

    #[test]
    fn size_preference() {
        let dir = |size, scale| ThemeDir {
            path: String::new(),
            size,
            scale,
            kind: DirKind::Fixed,
            min: size,
            max: size,
        };
        let need = 48;
        let mut ranked = [
            (dir(16, 1), "png"),
            (dir(32, 1), "png"),
            (dir(64, 1), "png"),
            (dir(256, 1), "png"),
            (dir(48, 1), "png"),
            (dir(24, 2), "png"),
            (dir(16, 1), "svg"),
        ];
        ranked.sort_by_key(|(d, ext)| score(d, ext, need));
        let order: Vec<(u32, u32, &str)> =
            ranked.iter().map(|(d, e)| (d.size, d.scale, *e)).collect();
        assert_eq!(
            order,
            vec![
                (16, 1, "svg"),
                (48, 1, "png"),
                (24, 2, "png"),
                (64, 1, "png"),
                (256, 1, "png"),
                (32, 1, "png"),
                (16, 1, "png"),
            ]
        );
    }

    fn touch(path: PathBuf) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn lookup_follows_inheritance_and_prefers_sizes() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user/icons");
        let system = tmp.path().join("system/icons");
        let pixmaps = tmp.path().join("pixmaps");
        let write_index = |base: &Path, theme: &str, text: &str| {
            std::fs::create_dir_all(base.join(theme)).unwrap();
            std::fs::write(base.join(theme).join("index.theme"), text).unwrap();
        };
        write_index(&system, "Test", INDEX);
        write_index(
            &system,
            "Parent",
            "[Icon Theme]\nInherits=Grandparent\nDirectories=32x32/apps\n[32x32/apps]\nSize=32\nType=Fixed\n",
        );
        write_index(
            &system,
            "Grandparent",
            "[Icon Theme]\nDirectories=64x64/apps\n[64x64/apps]\nSize=64\nType=Fixed\n",
        );
        write_index(
            &system,
            "hicolor",
            "[Icon Theme]\nDirectories=128x128/apps\n[128x128/apps]\nSize=128\nType=Fixed\n",
        );
        // The current theme has the icon in two sizes; the user base dir
        // adds a third.
        touch(system.join("Test/16x16/apps/app.png"));
        touch(system.join("Test/48x48@2x/apps/app.png"));
        touch(user.join("Test/48x48/apps/app.png"));
        // Only further down the chain.
        touch(system.join("Parent/32x32/apps/parent-only.png"));
        touch(system.join("Grandparent/64x64/apps/grand-only.png"));
        touch(system.join("hicolor/128x128/apps/steam_icon_1.png"));
        touch(system.join("Grandparent/64x64/apps/steam_icon_1.png"));
        touch(system.join("Test/scalable/apps/vector.svg"));
        touch(system.join("Test/48x48/apps/vector.png"));
        touch(pixmaps.join("legacy.png"));
        touch(pixmaps.join("old.xpm"));

        let themes = IconThemes::load(
            vec![user.clone(), system.clone()],
            vec![pixmaps.clone()],
            "Test",
        );
        assert_eq!(themes.chain(), ["Test", "Parent", "Grandparent", "hicolor"]);
        assert_eq!(
            themes.lookup("app", 48),
            Some(user.join("Test/48x48/apps/app.png"))
        );
        assert_eq!(
            themes.lookup("app", 96),
            Some(system.join("Test/48x48@2x/apps/app.png"))
        );
        assert_eq!(
            themes.lookup("app", 12),
            Some(system.join("Test/16x16/apps/app.png"))
        );
        assert_eq!(
            themes.lookup("parent-only", 48),
            Some(system.join("Parent/32x32/apps/parent-only.png"))
        );
        assert_eq!(
            themes.lookup("grand-only", 24),
            Some(system.join("Grandparent/64x64/apps/grand-only.png"))
        );
        // The chain order beats a better size in hicolor.
        assert_eq!(
            themes.lookup("steam_icon_1", 128),
            Some(system.join("Grandparent/64x64/apps/steam_icon_1.png"))
        );
        assert_eq!(
            themes.lookup("vector", 48),
            Some(system.join("Test/scalable/apps/vector.svg"))
        );
        assert_eq!(
            themes.lookup("legacy", 48),
            Some(pixmaps.join("legacy.png"))
        );
        assert_eq!(themes.lookup("old", 48), None);
        assert_eq!(themes.lookup("missing", 48), None);
        assert_eq!(themes.lookup("../Test/16x16/apps/app", 48), None);

        // An unknown current theme still falls back to hicolor.
        let fallback = IconThemes::load(vec![system.clone()], vec![], "Nope");
        assert_eq!(fallback.chain(), ["hicolor"]);
    }

    #[test]
    fn inheritance_cycles_terminate() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        for (name, parent) in [("A", "B"), ("B", "A")] {
            std::fs::create_dir_all(base.join(name)).unwrap();
            std::fs::write(
                base.join(name).join("index.theme"),
                format!("[Icon Theme]\nInherits={parent}\nDirectories=\n"),
            )
            .unwrap();
        }
        let themes = IconThemes::load(vec![base], vec![], "A");
        assert_eq!(themes.chain(), ["A", "B"]);
    }

    #[test]
    fn theme_settings() {
        assert_eq!(
            parse_gtk_settings(
                "[Settings]\ngtk-theme-name=Adwaita\ngtk-icon-theme-name = Papirus-Dark\n"
            )
            .as_deref(),
            Some("Papirus-Dark")
        );
        assert_eq!(parse_gtk_settings("[Other]\ngtk-icon-theme-name=X\n"), None);
        assert_eq!(
            parse_kdeglobals("[General]\nTheme=no\n[Icons]\nTheme=breeze-dark\n").as_deref(),
            Some("breeze-dark")
        );
        assert_eq!(
            parse_gsettings("'Yaru-dark'\n").as_deref(),
            Some("Yaru-dark")
        );
        assert_eq!(parse_gsettings("''\n"), None);
    }
}
