//! Steam games, including Proton ones whose stream is named after a Windows
//! executable: Steam starts them with their app id in the environment, and
//! its desktop shortcuts use the icon `steam_icon_<app id>`.

use std::path::Path;

/// Environ files larger than this are not read.
const MAX_ENVIRON_BYTES: u64 = 1024 * 1024;

/// The Steam app id in a `/proc/<pid>/environ` blob: `SteamAppId`, else
/// `SteamGameId`. Ids of non-Steam shortcuts (64-bit) and zero are ignored.
pub fn app_id_from_environ(environ: &[u8]) -> Option<u32> {
    let value = |key: &[u8]| {
        environ.split(|&b| b == 0).find_map(|var| {
            let rest = var.strip_prefix(key)?.strip_prefix(b"=")?;
            std::str::from_utf8(rest)
                .ok()?
                .parse::<u32>()
                .ok()
                .filter(|&id| id != 0)
        })
    };
    value(b"SteamAppId").or_else(|| value(b"SteamGameId"))
}

/// The Steam app id of process `pid`, if its environment is readable (the
/// process is the user's own) and has one.
pub fn app_id_of(proc_root: &Path, pid: u32) -> Option<u32> {
    let path = proc_root.join(pid.to_string()).join("environ");
    let meta = std::fs::metadata(&path).ok()?;
    // procfs reports a size of 0; a regular file (tests) has its real size.
    if meta.len() > MAX_ENVIRON_BYTES {
        return None;
    }
    let mut data = Vec::new();
    use std::io::Read;
    std::fs::File::open(&path)
        .ok()?
        .take(MAX_ENVIRON_BYTES)
        .read_to_end(&mut data)
        .ok()?;
    app_id_from_environ(&data)
}

pub fn icon_name(app_id: u32) -> String {
    format!("steam_icon_{app_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_environ() {
        let env = b"HOME=/home/a\0SteamGameId=2950790\0SteamAppId=2950790\0PATH=/bin\0";
        assert_eq!(app_id_from_environ(env), Some(2950790));
        assert_eq!(app_id_from_environ(b"SteamGameId=1234\0"), Some(1234));
        assert_eq!(
            app_id_from_environ(b"SteamAppId=0\0SteamGameId=77\0"),
            Some(77)
        );
        // A non-Steam shortcut's 64-bit game id.
        assert_eq!(
            app_id_from_environ(b"SteamGameId=12345678901234567890\0"),
            None
        );
        assert_eq!(app_id_from_environ(b"XSteamAppId=5\0SteamAppIdx=6\0"), None);
        assert_eq!(app_id_from_environ(b"SteamAppId=\xff\0"), None);
        assert_eq!(app_id_from_environ(b""), None);
        assert_eq!(icon_name(42), "steam_icon_42");
    }

    #[test]
    fn reads_proc_environ() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("77")).unwrap();
        std::fs::write(root.path().join("77/environ"), b"SteamAppId=9\0").unwrap();
        assert_eq!(app_id_of(root.path(), 77), Some(9));
        assert_eq!(app_id_of(root.path(), 78), None);
    }
}
