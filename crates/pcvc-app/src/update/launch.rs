//! Starting a verified installer.

use std::path::Path;
use std::process::Child;

use super::release::InstallKind;

/// The Inno Setup switches for an unattended upgrade (see
/// packaging/windows/pcvolumecontrol.iss): the progress window only, no
/// questions or message boxes, no reboot and no Cancel button (cancelling
/// after the running app was stopped would leave nothing running), and
/// the same install mode as the existing installation so it is upgraded in
/// place. The installer restarts the app afterwards because it finds it
/// running.
#[cfg(any(target_os = "windows", test))]
pub fn installer_args(kind: InstallKind, log: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = [
        "/SILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/NOCANCEL",
        "/SP-",
    ]
    .map(str::to_owned)
    .into();
    if let InstallKind::WindowsInstaller { all_users } = kind {
        args.push(
            if all_users {
                "/ALLUSERS"
            } else {
                "/CURRENTUSER"
            }
            .to_owned(),
        );
    }
    if let Some(log) = log {
        args.push(format!("/LOG={}", log.display()));
    }
    args
}

#[cfg(target_os = "windows")]
pub fn start(installer: &Path, kind: InstallKind) -> std::io::Result<Child> {
    let log = crate::paths::log_dir().map(|dir| dir.join("update-install.log"));
    let args = installer_args(kind, log.as_deref());
    tracing::info!("starting {} {}", installer.display(), args.join(" "));
    std::process::Command::new(installer).args(args).spawn()
}

#[cfg(not(target_os = "windows"))]
pub fn start(_installer: &Path, _kind: InstallKind) -> std::io::Result<Child> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Opens an HTTPS page in the default browser.
pub fn open_url(url: &str) -> std::io::Result<()> {
    if !url.starts_with("https://") || url.chars().any(char::is_control) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    open(url)
}

#[cfg(target_os = "windows")]
fn open(url: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = |s: &str| -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (verb, target) = (wide("open"), wide(url));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if result as isize > 32 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "ShellExecute failed ({})",
            result as isize
        )))
    }
}

#[cfg(not(target_os = "windows"))]
fn open(url: &str) -> std::io::Result<()> {
    let mut child = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .spawn()?;
    // Reaped in the background so it leaves no zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_in_the_existing_install_mode() {
        let user = installer_args(InstallKind::WindowsInstaller { all_users: false }, None);
        assert_eq!(
            user,
            [
                "/SILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/NOCANCEL",
                "/SP-",
                "/CURRENTUSER"
            ]
        );
        let all = installer_args(
            InstallKind::WindowsInstaller { all_users: true },
            Some(Path::new("C:\\Users\\A B\\update-install.log")),
        );
        assert_eq!(
            &all[5..],
            ["/ALLUSERS", "/LOG=C:\\Users\\A B\\update-install.log"]
        );
    }
}
