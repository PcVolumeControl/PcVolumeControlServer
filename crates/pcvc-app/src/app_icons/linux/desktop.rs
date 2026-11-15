//! Desktop entries (`*.desktop`): parsing, and an index from what a stream
//! reports about its application to the entry's `Icon`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Directory levels searched below an applications directory.
const MAX_DEPTH: usize = 3;

/// What an entry contributes to matching.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// Desktop file id, such as `org.mozilla.firefox`.
    pub id: String,
    pub name: Option<String>,
    pub icon: String,
    pub exec: Exec,
    pub wm_class: Option<String>,
}

/// The program an `Exec` line starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exec {
    /// File name of the program, after unwrapping `env` and `flatpak run`.
    pub program: Option<String>,
    /// The app id of a `flatpak run` line.
    pub flatpak_app: Option<String>,
}

/// The `[Desktop Entry]` group of a desktop file, if it describes a visible
/// application with an icon. `id` is the desktop file id.
pub fn parse_entry(id: &str, text: &str) -> Option<Entry> {
    let mut in_group = false;
    let mut values: HashMap<&str, String> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_group {
                break;
            }
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            // Localised keys (`Name[de]`) are ignored.
            values
                .entry(key.trim())
                .or_insert_with(|| unescape(value.trim()));
        }
    }
    let flag = |key: &str| values.get(key).is_some_and(|v| v == "true");
    if !in_group && values.is_empty() || flag("Hidden") {
        return None;
    }
    if values.get("Type").is_some_and(|t| t != "Application") {
        return None;
    }
    let non_empty = |key: &str| values.get(key).filter(|v| !v.is_empty()).cloned();
    Some(Entry {
        id: id.to_owned(),
        name: non_empty("Name"),
        icon: non_empty("Icon")?,
        exec: values
            .get("Exec")
            .map(|e| parse_exec(e))
            .unwrap_or_default(),
        wm_class: non_empty("StartupWMClass"),
    })
}

/// Undoes the escapes of desktop file string values.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Splits an `Exec` value into arguments: whitespace separates them, double
/// quotes group them and a backslash escapes the next character inside
/// quotes.
fn split_args(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    args
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The program an `Exec` line runs. Field codes (`%U`) are ignored, `env`
/// with its assignments and options is skipped, and `flatpak run` yields
/// its app id and `--command`.
pub fn parse_exec(exec: &str) -> Exec {
    let args = split_args(exec);
    let mut rest = args
        .iter()
        .map(String::as_str)
        .filter(|a| !is_field_code(a));
    let Some(first) = rest.next() else {
        return Exec::default();
    };
    if file_name(first) != "env" {
        return parse_exec_from(first, rest);
    }
    // `env [-i] [-u NAME] [NAME=VALUE]... PROGRAM`
    let mut skip_next = false;
    while let Some(arg) = rest.next() {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if matches!(arg, "-u" | "--unset" | "-C" | "--chdir") {
            skip_next = true;
        } else if !arg.starts_with('-') && !arg.contains('=') {
            return parse_exec_from(arg, rest);
        }
    }
    Exec::default()
}

fn parse_exec_from<'a>(program: &'a str, mut rest: impl Iterator<Item = &'a str>) -> Exec {
    let name = file_name(program);
    if name != "flatpak" {
        return Exec {
            program: (!name.is_empty()).then(|| name.to_owned()),
            flatpak_app: None,
        };
    }
    if rest.next() != Some("run") {
        return Exec::default();
    }
    let mut command = None;
    let mut app = None;
    for arg in rest {
        if let Some(value) = arg.strip_prefix("--command=") {
            command = Some(file_name(value).to_owned());
        } else if !arg.starts_with('-') && !arg.starts_with("@@") {
            app = Some(arg.to_owned());
            break;
        }
    }
    Exec {
        program: command,
        flatpak_app: app,
    }
}

fn is_field_code(arg: &str) -> bool {
    arg.len() == 2 && arg.starts_with('%')
}

/// Every application entry with an icon, indexed for lookup. Keys are
/// lowercase.
#[derive(Debug, Default)]
pub struct DesktopIndex {
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    /// The last dot-separated part of the id (`firefox` for
    /// `org.mozilla.firefox`).
    by_id_tail: HashMap<String, usize>,
    by_wm_class: HashMap<String, usize>,
    by_exec: HashMap<String, Vec<usize>>,
    by_name: HashMap<String, usize>,
}

impl DesktopIndex {
    /// Reads every desktop file below `dirs`. An id found in an earlier
    /// directory hides the same id in later ones, as the spec requires, even
    /// when the earlier entry is hidden or has no icon.
    pub fn scan(dirs: &[PathBuf]) -> Self {
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        for dir in dirs {
            let mut files = Vec::new();
            collect_desktop_files(dir, dir, 0, &mut files);
            files.sort();
            for (id, path) in files {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if let Some(entry) = parse_entry(&id, &text) {
                    entries.push(entry);
                }
            }
        }
        Self::new(entries)
    }

    /// Indexes `entries`; earlier entries win on equal keys.
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut index = Self::default();
        for (i, e) in entries.iter().enumerate() {
            let id = e.id.to_lowercase();
            if let Some(tail) = id.rsplit('.').next().filter(|t| *t != id) {
                index.by_id_tail.entry(tail.to_owned()).or_insert(i);
            }
            index.by_id.entry(id).or_insert(i);
            if let Some(app) = &e.exec.flatpak_app {
                index.by_id.entry(app.to_lowercase()).or_insert(i);
            }
            if let Some(class) = &e.wm_class {
                index.by_wm_class.entry(class.to_lowercase()).or_insert(i);
            }
            if let Some(program) = &e.exec.program {
                index
                    .by_exec
                    .entry(program.to_lowercase())
                    .or_default()
                    .push(i);
            }
            if let Some(name) = &e.name {
                index.by_name.entry(name.to_lowercase()).or_insert(i);
            }
        }
        index.entries = entries;
        index
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn icon(&self, i: Option<&usize>) -> Option<&str> {
        i.map(|&i| self.entries[i].icon.as_str())
    }

    /// The entry whose id (or `flatpak run` app id) is `app_id`.
    pub fn by_app_id(&self, app_id: &str) -> Option<&str> {
        self.icon(self.by_id.get(&app_id.to_lowercase()))
    }

    /// The entry for a process binary: by id, `StartupWMClass`, `Exec`
    /// program, then id tail. When several entries run the same program
    /// with different icons (every Steam game runs `steam`), the one named
    /// after the program wins, and without one there is no match.
    pub fn by_binary(&self, binary: &str) -> Option<&str> {
        let key = file_name(binary).to_lowercase();
        if key.is_empty() {
            return None;
        }
        self.icon(self.by_id.get(&key))
            .or_else(|| self.icon(self.by_wm_class.get(&key)))
            .or_else(|| self.by_exec(&key))
            .or_else(|| self.icon(self.by_id_tail.get(&key)))
    }

    fn by_exec(&self, key: &str) -> Option<&str> {
        let candidates = self.by_exec.get(key)?;
        let first = &self.entries[*candidates.first()?];
        if candidates
            .iter()
            .all(|&i| self.entries[i].icon == first.icon)
        {
            return Some(&first.icon);
        }
        candidates.iter().map(|&i| &self.entries[i]).find_map(|e| {
            let id_tail = e.id.rsplit('.').next().unwrap_or(&e.id);
            let named = id_tail.eq_ignore_ascii_case(key)
                || e.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(key));
            named.then_some(e.icon.as_str())
        })
    }

    /// The entry for an application name: by `Name`, then id, then
    /// `StartupWMClass`.
    pub fn by_name(&self, name: &str) -> Option<&str> {
        let key = name.trim().to_lowercase();
        if key.is_empty() {
            return None;
        }
        self.icon(self.by_name.get(&key))
            .or_else(|| self.icon(self.by_id.get(&key)))
            .or_else(|| self.icon(self.by_wm_class.get(&key)))
    }
}

/// Desktop files below `root` as (desktop file id, path): the path relative
/// to `root` with `/` replaced by `-` and without the extension.
fn collect_desktop_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // Symlinks are followed: Flatpak and Snap export entries as links.
        let is_dir = kind.is_dir() || (kind.is_symlink() && path.is_dir());
        if is_dir {
            if depth < MAX_DEPTH {
                collect_desktop_files(root, &path, depth + 1, out);
            }
            continue;
        }
        if path.extension().is_none_or(|e| e != "desktop") {
            continue;
        }
        let Some(id) = path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| rel.with_extension("").to_str().map(|s| s.replace('/', "-")))
        else {
            continue;
        };
        out.push((id, path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(line: &str) -> (Option<String>, Option<String>) {
        let e = parse_exec(line);
        (e.program, e.flatpak_app)
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    #[test]
    fn exec_lines() {
        assert_eq!(exec("firefox %u"), (some("firefox"), None));
        assert_eq!(
            exec("/snap/bin/firefox --new-window %u"),
            (some("firefox"), None)
        );
        assert_eq!(
            exec(r#""/opt/My Apps/app bin" --flag "%F""#),
            (some("app bin"), None)
        );
        assert_eq!(exec(r#""/opt/q\"uote/run" %U"#), (some("run"), None));
        assert_eq!(
            exec("env BAMF_DESKTOP_FILE_HINT=/x.desktop GDK_BACKEND=x11 /snap/bin/spotify %U"),
            (some("spotify"), None)
        );
        assert_eq!(exec("/usr/bin/env -u FOO -i mpv"), (some("mpv"), None));
        assert_eq!(exec("env A=1"), (None, None));
        assert_eq!(
            exec(
                "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=firefox --file-forwarding org.mozilla.firefox @@u %u @@"
            ),
            (some("firefox"), some("org.mozilla.firefox"))
        );
        assert_eq!(
            exec("flatpak run com.valvesoftware.Steam"),
            (None, some("com.valvesoftware.Steam"))
        );
        assert_eq!(exec("%U"), (None, None));
        assert_eq!(exec(""), (None, None));
        assert_eq!(
            exec("steam steam://rungameid/2950790"),
            (some("steam"), None)
        );
    }

    #[test]
    fn parses_only_the_main_group() {
        let text = "\
# comment
[Desktop Entry]
Type=Application
Name=Firefox
Name[de]=Feuerfuchs
Exec=/snap/bin/firefox %u
Icon=/snap/firefox/current/default256.png
StartupWMClass=firefox_firefox
Comment=Web\\sBrowser

[Desktop Action new-window]
Name=New Window
Exec=/snap/bin/other %u
Icon=other
";
        let e = parse_entry("firefox_firefox", text).unwrap();
        assert_eq!(e.name.as_deref(), Some("Firefox"));
        assert_eq!(e.icon, "/snap/firefox/current/default256.png");
        assert_eq!(e.exec.program.as_deref(), Some("firefox"));
        assert_eq!(e.wm_class.as_deref(), Some("firefox_firefox"));

        let hidden = "[Desktop Entry]\nName=A\nIcon=a\nHidden=true\n";
        assert!(parse_entry("a", hidden).is_none());
        let link = "[Desktop Entry]\nType=Link\nName=A\nIcon=a\n";
        assert!(parse_entry("a", link).is_none());
        assert!(parse_entry("a", "[Desktop Entry]\nName=A\n").is_none());
        assert!(parse_entry("a", "[Other]\nIcon=a\n").is_none());
        let escaped = "[Desktop Entry]\nIcon=My\\sIcon\n";
        assert_eq!(parse_entry("a", escaped).unwrap().icon, "My Icon");
    }

    fn entry(id: &str, name: &str, exec: &str, icon: &str) -> Entry {
        Entry {
            id: id.into(),
            name: (!name.is_empty()).then(|| name.into()),
            icon: icon.into(),
            exec: parse_exec(exec),
            wm_class: None,
        }
    }

    #[test]
    fn matching_precedence() {
        let index = DesktopIndex::new(vec![
            entry(
                "IRON NEST Heavy Turret Simulator",
                "IRON NEST: Heavy Turret Simulator",
                "steam steam://rungameid/2950790",
                "steam_icon_2950790",
            ),
            entry(
                "Selaco",
                "Selaco",
                "steam steam://rungameid/1592280",
                "steam_icon_1592280",
            ),
            entry("steam", "Steam", "/usr/bin/steam %U", "steam"),
            entry(
                "org.mozilla.firefox",
                "Firefox Web Browser",
                "firefox %u",
                "firefox",
            ),
            entry(
                "org.gnome.Nautilus",
                "Files",
                "nautilus --new-window",
                "org.gnome.Nautilus",
            ),
            Entry {
                wm_class: Some("Code".into()),
                ..entry(
                    "code_code",
                    "Visual Studio Code",
                    "/snap/bin/code --x",
                    "vscode",
                )
            },
            entry(
                "org.example.Flat",
                "Flat",
                "flatpak run --command=flat-bin org.example.Flat",
                "org.example.Flat",
            ),
            entry(
                "mpv",
                "mpv Media Player",
                "mpv --player-operation-mode=pseudo-gui",
                "mpv",
            ),
            entry("mpv-alt", "Other", "mpv", "mpv"),
        ]);
        assert_eq!(index.len(), 9);
        assert_eq!(index.by_app_id("ORG.MOZILLA.FIREFOX"), Some("firefox"));
        assert_eq!(
            index.by_app_id("org.example.Flat"),
            Some("org.example.Flat")
        );
        assert_eq!(index.by_app_id("org.unknown"), None);
        // Exec program shared by every Steam game: the entry named after it.
        assert_eq!(index.by_binary("steam"), Some("steam"));
        assert_eq!(index.by_binary("firefox"), Some("firefox"));
        assert_eq!(index.by_binary("/usr/lib/firefox/firefox"), Some("firefox"));
        assert_eq!(index.by_binary("code"), Some("vscode"));
        assert_eq!(index.by_binary("flat-bin"), Some("org.example.Flat"));
        assert_eq!(index.by_binary("nautilus"), Some("org.gnome.Nautilus"));
        // Same program and the same icon is not ambiguous.
        assert_eq!(index.by_binary("mpv"), Some("mpv"));
        assert_eq!(index.by_binary(""), None);
        assert_eq!(
            index.by_name("iron nest: heavy turret simulator"),
            Some("steam_icon_2950790")
        );
        assert_eq!(index.by_name(" Files "), Some("org.gnome.Nautilus"));
        assert_eq!(index.by_name("selaco"), Some("steam_icon_1592280"));
        assert_eq!(index.by_name("Unknown"), None);

        let games_only = DesktopIndex::new(vec![
            entry("a", "Game A", "steam steam://rungameid/1", "steam_icon_1"),
            entry("b", "Game B", "steam steam://rungameid/2", "steam_icon_2"),
        ]);
        assert_eq!(games_only.by_binary("steam"), None);
    }

    #[test]
    fn scans_directories_in_precedence_order() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home/applications");
        let system = dir.path().join("usr/applications");
        std::fs::create_dir_all(home.join("kde")).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        let write = |path: PathBuf, icon: &str, extra: &str| {
            std::fs::write(
                path,
                format!("[Desktop Entry]\nType=Application\nName=X\nIcon={icon}\n{extra}"),
            )
            .unwrap();
        };
        write(home.join("app.desktop"), "user-icon", "Exec=app");
        write(system.join("app.desktop"), "system-icon", "Exec=app");
        write(home.join("gone.desktop"), "x", "Hidden=true");
        write(system.join("gone.desktop"), "gone-icon", "Exec=gone");
        write(home.join("kde/dolphin.desktop"), "dolphin", "");
        write(system.join("notes.txt"), "x", "");
        let index = DesktopIndex::scan(&[home, system, dir.path().join("missing")]);
        assert_eq!(index.by_app_id("app"), Some("user-icon"));
        assert_eq!(index.by_binary("gone"), None);
        assert_eq!(index.by_app_id("kde-dolphin"), Some("dolphin"));
        assert_eq!(index.len(), 2);
    }
}
