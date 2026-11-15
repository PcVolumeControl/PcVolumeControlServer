//! How the running copy of the app was installed.

use std::path::Path;

use super::release::InstallKind;

/// Looks at where the running executable is and, on Windows, at the
/// installer's uninstall entry.
pub fn detect() -> InstallKind {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            tracing::warn!("cannot tell where the app runs from: {e}");
            return fallback();
        }
    };
    let kind = from_exe(&exe);
    tracing::debug!("install kind: {kind:?} ({})", exe.display());
    kind
}

#[cfg(target_os = "windows")]
fn fallback() -> InstallKind {
    InstallKind::WindowsPortable
}

#[cfg(not(target_os = "windows"))]
fn fallback() -> InstallKind {
    InstallKind::LinuxTarball
}

/// The .deb installs the binary to /usr/bin; anything else came from the
/// tarball or a build.
#[cfg(not(target_os = "windows"))]
fn from_exe(exe: &Path) -> InstallKind {
    if exe == Path::new("/usr/bin/pcvolumecontrol") {
        InstallKind::LinuxDeb
    } else {
        InstallKind::LinuxTarball
    }
}

/// Installed when the installer's uninstall entry, per user (HKCU) or for
/// all users (HKLM), names the executable's folder as its install
/// location; a copy elsewhere is portable even on a machine that also has
/// the app installed.
#[cfg(target_os = "windows")]
fn from_exe(exe: &Path) -> InstallKind {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let Some(dir) = exe.parent() else {
        return InstallKind::WindowsPortable;
    };
    for (root, all_users) in [(HKEY_CURRENT_USER, false), (HKEY_LOCAL_MACHINE, true)] {
        if let Some(location) = registry::install_location(root)
            && same_dir(Path::new(&location), dir)
        {
            return InstallKind::WindowsInstaller { all_users };
        }
    }
    InstallKind::WindowsPortable
}

/// Compares two folders as Windows does: after resolving them, ignoring
/// case.
#[cfg(target_os = "windows")]
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()),
        _ => false,
    }
}

#[cfg(target_os = "windows")]
mod registry {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
    };

    use crate::win::wide;

    /// Inno Setup's uninstall entry for the installer's fixed AppId
    /// (packaging/windows/pcvolumecontrol.iss).
    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{63ED99E8-31F1-4FA3-A5B5-DDAAF2F7463A}_is1";

    /// The entry's `InstallLocation` under `root`, in the 64-bit view
    /// where the 64-bit installer writes it.
    pub fn install_location(root: HKEY) -> Option<String> {
        let key = wide(UNINSTALL_KEY);
        let value = wide("InstallLocation");
        let flags = RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY;
        let mut bytes = 0u32;
        // SAFETY: the strings are NUL-terminated and outlive the call; a
        // null buffer asks only for the size.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS || bytes == 0 || bytes > 64 * 1024 {
            return None;
        }
        let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: as above, with a buffer of `size` bytes.
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                value.as_ptr(),
                flags,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        OsString::from_wide(&buffer[..len]).into_string().ok()
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn deb_installs_to_usr_bin() {
        assert_eq!(
            from_exe(Path::new("/usr/bin/pcvolumecontrol")),
            InstallKind::LinuxDeb
        );
        for other in [
            "/usr/local/bin/pcvolumecontrol",
            "/home/a/.local/bin/pcvolumecontrol",
            "/home/a/src/target/release/pcvolumecontrol",
        ] {
            assert_eq!(from_exe(Path::new(other)), InstallKind::LinuxTarball);
        }
        detect();
    }
}
