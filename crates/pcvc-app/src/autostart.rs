//! Start on login: the `HKCU\...\Run` value "PcVolumeControl" on Windows,
//! `~/.config/autostart/PcVolumeControl.desktop` on Linux.

use auto_launch::{AutoLaunch, AutoLaunchBuilder, LinuxLaunchMode, WindowsEnableMode};

const NAME: &str = "PcVolumeControl";

/// Both the Run value and the desktop entry's `Exec=` split an unquoted
/// command line at spaces.
fn quote_if_needed(path: &str) -> String {
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
        .set_app_path(&quote_if_needed(exe))
        .set_linux_launch_mode(LinuxLaunchMode::XdgAutostart)
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .build()
        .map_err(|e| e.to_string())
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
    fn quotes_paths_with_spaces() {
        assert_eq!(
            quote_if_needed("/usr/bin/pcvolumecontrol"),
            "/usr/bin/pcvolumecontrol"
        );
        assert_eq!(
            quote_if_needed(r"C:\Program Files\PcVolumeControl\pcvolumecontrol.exe"),
            r#""C:\Program Files\PcVolumeControl\pcvolumecontrol.exe""#
        );
    }
}
