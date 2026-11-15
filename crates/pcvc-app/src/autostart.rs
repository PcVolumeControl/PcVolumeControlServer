//! Start on login: the `HKCU\...\Run` value "PcVolumeControl" on Windows,
//! `~/.config/autostart/PcVolumeControl.desktop` on Linux.

use auto_launch::{AutoLaunch, AutoLaunchBuilder, LinuxLaunchMode, WindowsEnableMode};

const NAME: &str = "PcVolumeControl";

/// Quotes the executable for a desktop entry's `Exec=` key: reserved
/// characters need double quotes, inside which `"`, `` ` ``, `$` and `\` are
/// backslash-escaped; the string-level escape then doubles every backslash,
/// and `%` (a field code) is doubled.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn desktop_exec_quote(path: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    let quoted = if path.contains(RESERVED) {
        let mut out = String::from("\"");
        for c in path.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        path.to_owned()
    };
    quoted.replace('\\', "\\\\").replace('%', "%%")
}

/// Quotes the executable for the `Run` value, which splits at spaces.
/// Windows paths cannot contain `"`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn run_value_quote(path: &str) -> String {
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.to_owned()
    }
}

fn launcher() -> Result<AutoLaunch, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    let exe = exe
        .to_str()
        .ok_or_else(|| format!("executable path is not UTF-8: {}", exe.display()))?;
    AutoLaunchBuilder::new()
        .set_app_name(NAME)
        .set_app_path(&quote(exe))
        .set_linux_launch_mode(LinuxLaunchMode::XdgAutostart)
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .build()
        .map_err(|e| e.to_string())
}

#[cfg(target_os = "windows")]
fn quote(path: &str) -> String {
    run_value_quote(path)
}

#[cfg(not(target_os = "windows"))]
fn quote(path: &str) -> String {
    desktop_exec_quote(path)
}

/// Rewrites an enabled login item so it points at this executable, in case
/// the app was moved or reinstalled elsewhere. Debug builds leave it alone
/// so a development run does not take over an installed copy's entry.
pub fn refresh() {
    if cfg!(debug_assertions) {
        return;
    }
    if let Err(e) = set_enabled(true) {
        tracing::warn!("refreshing the start on login entry failed: {e}");
    }
}

pub fn is_enabled() -> Result<bool, String> {
    launcher()?.is_enabled().map_err(|e| e.to_string())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let launcher = launcher()?;
    let result = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    result.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_exec_quoting() {
        assert_eq!(
            desktop_exec_quote("/usr/bin/pcvolumecontrol"),
            "/usr/bin/pcvolumecontrol"
        );
        assert_eq!(
            desktop_exec_quote("/opt/My Apps/pcvc"),
            r#""/opt/My Apps/pcvc""#
        );
        assert_eq!(
            desktop_exec_quote("/home/u/Apps(old)/$x/a\\b"),
            r#""/home/u/Apps(old)/\\$x/a\\\\b""#
        );
        assert_eq!(desktop_exec_quote("/opt/100%/pcvc"), "/opt/100%%/pcvc");
    }

    #[test]
    fn run_value_quoting() {
        assert_eq!(
            run_value_quote(r"C:\pcvc\pcvolumecontrol.exe"),
            r"C:\pcvc\pcvolumecontrol.exe"
        );
        assert_eq!(
            run_value_quote(r"C:\Program Files\PcVolumeControl\pcvolumecontrol.exe"),
            r#""C:\Program Files\PcVolumeControl\pcvolumecontrol.exe""#
        );
    }
}
