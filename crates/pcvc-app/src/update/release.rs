//! The latest GitHub release: asking for it, reading the answer, and
//! picking the assets this installation needs.

use std::time::Duration;

use serde::Deserialize;

use super::http::{Client, HttpError, Limits};
use super::version::{self, ReleaseVersion};

const REPO: &str = "PcVolumeControl/PcVolumeControlServer";

pub const CHECKSUMS: &str = "SHA256SUMS";
pub const SIGNATURE: &str = "SHA256SUMS.minisig";

/// A release's JSON, including its notes, is a few KiB.
const API_LIMITS: Limits = Limits {
    accept: "application/vnd.github+json",
    max_bytes: 1024 * 1024,
    total: Duration::from_secs(30),
};
const MAX_ASSETS: usize = 100;

/// The URLs a check and a download use; tests point them at a local server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The latest release (drafts and pre-releases excluded).
    pub latest: String,
    /// `<download>/<tag>/<asset name>` downloads an asset.
    pub download: String,
    /// `<page>/<tag>` is a release's page.
    pub page: String,
}

impl Endpoints {
    pub fn github() -> Self {
        Self {
            latest: format!("https://api.github.com/repos/{REPO}/releases/latest"),
            download: format!("https://github.com/{REPO}/releases/download"),
            page: format!("https://github.com/{REPO}/releases/tag"),
        }
    }
}

/// How this copy of the app was installed, which decides the asset it is
/// updated from and whether it can install updates itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "each platform detects only its own kinds")]
pub enum InstallKind {
    /// By the Windows installer; `all_users` for an install in Program
    /// Files, for which the installer asks for elevation.
    WindowsInstaller {
        all_users: bool,
    },
    WindowsPortable,
    LinuxDeb,
    LinuxTarball,
}

impl InstallKind {
    /// The release asset this kind of installation comes from, named as the
    /// release workflow names it.
    pub fn asset_name(self, version: &ReleaseVersion) -> String {
        let v = &version.text;
        match self {
            Self::WindowsInstaller { .. } => format!("PcVolumeControl-{v}-setup.exe"),
            Self::WindowsPortable => format!("pcvolumecontrol-{v}-windows-x86_64.zip"),
            Self::LinuxDeb => format!("pcvolumecontrol_{v}-1_amd64.deb"),
            Self::LinuxTarball => format!("pcvolumecontrol-{v}-linux-x86_64.tar.gz"),
        }
    }

    /// Whether the app can install an update itself.
    pub fn installs(self) -> bool {
        matches!(self, Self::WindowsInstaller { .. })
    }
}

/// A release asset at a URL built from its expected name, never one taken
/// from the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// As listed by the API.
    pub size: u64,
    pub url: String,
}

/// A release newer than the running version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: ReleaseVersion,
    pub page_url: String,
    /// The asset for this installation, if the release has it.
    pub asset: Option<Asset>,
    pub checksums: Option<Asset>,
    pub signature: Option<Asset>,
}

/// What is kept of a found release between runs: its version and the sizes
/// of its assets, from which [`SavedRelease::restore`] rebuilds the URLs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct SavedRelease {
    pub version: String,
    pub asset_size: Option<u64>,
    pub checksums_size: Option<u64>,
    pub signature_size: Option<u64>,
}

impl SavedRelease {
    pub fn new(release: &Release) -> Self {
        Self {
            version: release.version.text.clone(),
            asset_size: release.asset.as_ref().map(|a| a.size),
            checksums_size: release.checksums.as_ref().map(|a| a.size),
            signature_size: release.signature.as_ref().map(|a| a.size),
        }
    }

    /// The release again, if it is still newer than `current`.
    pub fn restore(
        &self,
        endpoints: &Endpoints,
        current: &ReleaseVersion,
        kind: InstallKind,
    ) -> Option<Release> {
        let version = ReleaseVersion::parse(&self.version)?;
        if !version::is_update(&version, current) {
            return None;
        }
        let tag = version.tag();
        let asset = |name: String, size: Option<u64>| {
            size.filter(|&s| s > 0).map(|size| Asset {
                url: format!("{}/{tag}/{name}", endpoints.download),
                size,
                name,
            })
        };
        Some(Release {
            page_url: format!("{}/{tag}", endpoints.page),
            asset: asset(kind.asset_name(&version), self.asset_size),
            checksums: asset(CHECKSUMS.to_owned(), self.checksums_size),
            signature: asset(SIGNATURE.to_owned(), self.signature_size),
            version,
        })
    }
}

impl Release {
    /// Whether everything an install needs is attached: the asset, its
    /// checksums and, when installs must be signed, their signature.
    pub fn installable(&self, signed: bool) -> bool {
        self.asset.is_some() && self.checksums.is_some() && (!signed || self.signature.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate {
        latest: ReleaseVersion,
    },
    Available(Box<Release>),
    /// The repository has no published stable release.
    NoReleases,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("{0}")]
    Http(#[from] HttpError),
    #[error("unexpected answer from GitHub: {0}")]
    Malformed(String),
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    size: u64,
    /// "uploaded" once complete.
    #[serde(default)]
    state: Option<String>,
}

/// Asks GitHub for the latest release and compares it with `current`.
pub fn check(
    client: &Client,
    endpoints: &Endpoints,
    current: &ReleaseVersion,
    kind: InstallKind,
) -> Result<CheckOutcome, CheckError> {
    match client.get_bytes(&endpoints.latest, API_LIMITS) {
        Ok(body) => parse_latest(&body, endpoints, current, kind),
        Err(HttpError::NotFound) => Ok(CheckOutcome::NoReleases),
        Err(e) => Err(e.into()),
    }
}

/// Reads the API's answer for the latest release.
pub fn parse_latest(
    body: &[u8],
    endpoints: &Endpoints,
    current: &ReleaseVersion,
    kind: InstallKind,
) -> Result<CheckOutcome, CheckError> {
    let malformed = |what: &str| CheckError::Malformed(what.to_owned());
    let release: ApiRelease =
        serde_json::from_slice(body).map_err(|e| CheckError::Malformed(e.to_string()))?;
    if release.draft || release.prerelease {
        return Err(malformed("the latest release is a draft or pre-release"));
    }
    let latest = ReleaseVersion::parse(&release.tag_name)
        .ok_or_else(|| malformed("the latest release's tag is not a version"))?;
    if latest.is_prerelease() {
        return Err(malformed("the latest release has a pre-release version"));
    }
    if release.assets.len() > MAX_ASSETS {
        return Err(malformed("too many assets"));
    }
    if !version::is_update(&latest, current) {
        return Ok(CheckOutcome::UpToDate { latest });
    }
    let tag = latest.tag();
    let find = |name: String| {
        release
            .assets
            .iter()
            .find(|a| {
                a.name == name && a.size > 0 && a.state.as_deref().is_none_or(|s| s == "uploaded")
            })
            .map(|a| Asset {
                url: format!("{}/{tag}/{name}", endpoints.download),
                size: a.size,
                name,
            })
    };
    Ok(CheckOutcome::Available(Box::new(Release {
        page_url: format!("{}/{tag}", endpoints.page),
        asset: find(kind.asset_name(&latest)),
        checksums: find(CHECKSUMS.to_owned()),
        signature: find(SIGNATURE.to_owned()),
        version: latest,
    })))
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn v(text: &str) -> ReleaseVersion {
        ReleaseVersion::parse(text).unwrap()
    }

    pub fn release_json(tag: &str, names: &[&str]) -> String {
        let assets: Vec<_> = names
            .iter()
            .map(|n| {
                serde_json::json!({
                    "name": n,
                    "size": 1234,
                    "state": "uploaded",
                    "browser_download_url": format!("https://evil.example/{n}"),
                })
            })
            .collect();
        serde_json::json!({
            "tag_name": tag,
            "html_url": "https://evil.example/release",
            "draft": false,
            "prerelease": false,
            "body": "notes",
            "assets": assets,
        })
        .to_string()
    }

    fn all_assets(version: &str) -> Vec<String> {
        [
            format!("PcVolumeControl-{version}-setup.exe"),
            format!("pcvolumecontrol-{version}-windows-x86_64.zip"),
            format!("pcvolumecontrol_{version}-1_amd64.deb"),
            format!("pcvolumecontrol-{version}-linux-x86_64.tar.gz"),
            "SHA256SUMS".to_owned(),
        ]
        .into()
    }

    fn parse(json: &str, current: &str, kind: InstallKind) -> Result<CheckOutcome, CheckError> {
        parse_latest(json.as_bytes(), &Endpoints::github(), &v(current), kind)
    }

    #[test]
    fn picks_the_asset_for_each_kind_of_install() {
        let names = all_assets("1.2.3");
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let json = release_json("v1.2.3", &names);
        let base = "https://github.com/PcVolumeControl/PcVolumeControlServer/releases";
        for (kind, name) in [
            (
                InstallKind::WindowsInstaller { all_users: false },
                "PcVolumeControl-1.2.3-setup.exe",
            ),
            (
                InstallKind::WindowsPortable,
                "pcvolumecontrol-1.2.3-windows-x86_64.zip",
            ),
            (InstallKind::LinuxDeb, "pcvolumecontrol_1.2.3-1_amd64.deb"),
            (
                InstallKind::LinuxTarball,
                "pcvolumecontrol-1.2.3-linux-x86_64.tar.gz",
            ),
        ] {
            let Ok(CheckOutcome::Available(release)) = parse(&json, "1.2.2", kind) else {
                panic!("{kind:?}");
            };
            assert_eq!(release.page_url, format!("{base}/tag/v1.2.3"));
            assert!(release.installable(false));
            assert!(!release.installable(true), "unsigned");
            let asset = release.asset.unwrap();
            assert_eq!(asset.name, name);
            assert_eq!(asset.size, 1234);
            assert_eq!(asset.url, format!("{base}/download/v1.2.3/{name}"));
            let checksums = release.checksums.unwrap();
            assert_eq!(checksums.url, format!("{base}/download/v1.2.3/SHA256SUMS"));
            assert_eq!(release.signature, None);
        }
    }

    #[test]
    fn missing_or_unexpected_assets_are_not_used() {
        let json = release_json(
            "v1.2.3",
            &[
                "PcVolumeControl-1.2.4-setup.exe",
                "../SHA256SUMS",
                "evil.exe",
            ],
        );
        let Ok(CheckOutcome::Available(release)) = parse(
            &json,
            "1.0.0",
            InstallKind::WindowsInstaller { all_users: true },
        ) else {
            panic!()
        };
        assert!(!release.installable(false));
        assert_eq!((release.asset, release.checksums), (None, None));

        let mut value: serde_json::Value =
            serde_json::from_str(&release_json("v1.2.3", &["SHA256SUMS"])).unwrap();
        value["assets"][0]["state"] = "new".into();
        let Ok(CheckOutcome::Available(release)) =
            parse(&value.to_string(), "1.0.0", InstallKind::LinuxDeb)
        else {
            panic!()
        };
        assert_eq!(release.checksums, None, "incomplete upload");
    }

    #[test]
    fn compares_with_the_running_version() {
        let json = release_json("v1.2.3", &[]);
        let kind = InstallKind::LinuxTarball;
        assert_eq!(
            parse(&json, "1.2.3", kind),
            Ok(CheckOutcome::UpToDate { latest: v("1.2.3") })
        );
        assert_eq!(
            parse(&json, "1.3.0", kind),
            Ok(CheckOutcome::UpToDate { latest: v("1.2.3") })
        );
        assert!(matches!(
            parse(&json, "1.2.3-rc.1", kind),
            Ok(CheckOutcome::Available(_))
        ));
    }

    #[test]
    fn saved_releases_restore_while_still_newer() {
        let names = all_assets("1.2.3");
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let json = release_json("v1.2.3", &names);
        let kind = InstallKind::WindowsInstaller { all_users: false };
        let Ok(CheckOutcome::Available(release)) = parse(&json, "1.2.2", kind) else {
            panic!()
        };
        let saved = SavedRelease::new(&release);
        let endpoints = Endpoints::github();
        assert_eq!(
            saved.restore(&endpoints, &v("1.2.2"), kind).as_ref(),
            Some(&*release)
        );
        assert_eq!(saved.restore(&endpoints, &v("1.2.3"), kind), None);
        let bad = SavedRelease {
            version: "../1.2.3".into(),
            ..saved
        };
        assert_eq!(bad.restore(&endpoints, &v("1.0.0"), kind), None);
    }

    #[test]
    fn refuses_malformed_answers() {
        let kind = InstallKind::LinuxDeb;
        for bad in [
            "",
            "{",
            "[]",
            "{\"tag_name\": 5}",
            &release_json("latest", &[]),
            &release_json("v1.2.3+x", &[]),
            &release_json("v1.3.0-rc.1", &[]),
        ] {
            assert!(
                matches!(parse(bad, "1.0.0", kind), Err(CheckError::Malformed(_))),
                "{bad}"
            );
        }
        let mut value: serde_json::Value =
            serde_json::from_str(&release_json("v1.2.3", &[])).unwrap();
        value["prerelease"] = true.into();
        assert!(parse(&value.to_string(), "1.0.0", kind).is_err());
        let many: Vec<String> = (0..=MAX_ASSETS).map(|i| format!("a{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(parse(&release_json("v1.2.3", &many), "1.0.0", kind).is_err());
    }
}
