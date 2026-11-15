//! Update checks against GitHub Releases and, on Windows, installing an
//! update with the release's installer.
//!
//! Only GitHub is contacted ([`http::Policy`]); downloads are verified
//! against the release's `SHA256SUMS` and, when a key is built in, its
//! minisign signature before anything runs them. [`Worker`] does the
//! network and file work on its own thread; [`Updater`] is the state the
//! UI shows.

mod download;
mod http;
mod install_kind;
mod launch;
mod release;
pub mod schedule;
mod state;
#[cfg(test)]
mod tests;
mod verify;
mod version;
mod worker;

#[cfg(test)]
pub use download::Progress;
pub use install_kind::detect as detect_install_kind;
pub use launch::open_url;
pub use release::{InstallKind, Release};
pub use state::{Failure, Phase, Saved, Updater};
pub use worker::{Context, Event, Worker};

/// Whether the release workflow built this binary (it sets
/// `PCVC_OFFICIAL_BUILD`, which build.rs turns into this cfg). Other builds
/// never check automatically or install.
pub const OFFICIAL_BUILD: bool = cfg!(pcvc_official_build);

/// What the app offers for an available release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    InstallAndRestart,
    ViewRelease,
}

/// Installing needs an official build installed by the Windows installer,
/// and a release with everything verification needs; anything else gets
/// the release page.
pub fn action(release: &Release, kind: InstallKind, official: bool, signed: bool) -> Action {
    if official && kind.installs() && release.installable(signed) {
        Action::InstallAndRestart
    } else {
        Action::ViewRelease
    }
}

/// Whether installs must carry a signature (a key is built in, or the
/// built-in key is broken, in which case installs fail).
pub fn signature_required() -> bool {
    !matches!(verify::signing_key(), Ok(None))
}

/// The running version, as shown in the UI.
pub fn current_version() -> String {
    version::current().text
}

/// What the worker reports for a check by hand that found `version`, with
/// an installer and its checksums attached.
#[cfg(test)]
pub fn found(version: &str) -> Event {
    let asset = |name: &str| release::Asset {
        name: name.into(),
        size: 100,
        url: format!("https://github.com/{name}"),
    };
    Event::Checked {
        manual: true,
        result: Ok(release::CheckOutcome::Available(Box::new(Release {
            version: version::ReleaseVersion::parse(version).unwrap(),
            page_url: format!("https://github.com/x/releases/tag/v{version}"),
            asset: Some(asset("setup.exe")),
            checksums: Some(asset("SHA256SUMS")),
            signature: None,
        }))),
    }
}

/// What the worker reports for a check by hand with `result`.
#[cfg(test)]
pub fn checked(result: Result<(), http::HttpError>) -> Event {
    Event::Checked {
        manual: true,
        result: result
            .map(|()| release::CheckOutcome::NoReleases)
            .map_err(release::CheckError::Http),
    }
}

#[cfg(test)]
pub use http::HttpError;

#[cfg(test)]
mod action_tests {
    use super::release::Asset;
    use super::version::ReleaseVersion;
    use super::*;

    #[test]
    fn only_official_installed_windows_builds_install() {
        let asset = |name: &str| Asset {
            name: name.into(),
            size: 1,
            url: format!("https://github.com/{name}"),
        };
        let release = Release {
            version: ReleaseVersion::parse("2.0.0").unwrap(),
            page_url: "https://github.com/x".into(),
            asset: Some(asset("setup.exe")),
            checksums: Some(asset("SHA256SUMS")),
            signature: None,
        };
        let installer = InstallKind::WindowsInstaller { all_users: true };
        assert_eq!(
            action(&release, installer, true, false),
            Action::InstallAndRestart
        );
        assert_eq!(
            action(&release, installer, false, false),
            Action::ViewRelease,
            "development build"
        );
        assert_eq!(
            action(&release, installer, true, true),
            Action::ViewRelease,
            "unsigned"
        );
        for kind in [
            InstallKind::WindowsPortable,
            InstallKind::LinuxDeb,
            InstallKind::LinuxTarball,
        ] {
            assert_eq!(action(&release, kind, true, false), Action::ViewRelease);
        }
        let no_sums = Release {
            checksums: None,
            ..release.clone()
        };
        assert_eq!(
            action(&no_sums, installer, true, false),
            Action::ViewRelease
        );
        let signed = Release {
            signature: Some(asset("SHA256SUMS.minisig")),
            ..release
        };
        assert_eq!(
            action(&signed, installer, true, true),
            Action::InstallAndRestart
        );
    }
}
