//! Release versions: tags such as `v1.2.3` compared as SemVer.

use semver::Version;

/// A release's version and its text as the release workflow spells it in
/// asset names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseVersion {
    pub semver: Version,
    /// Without the leading `v`.
    pub text: String,
}

impl ReleaseVersion {
    /// Parses `1.2.3`, `v1.2.3` or `v1.2.3-rc.1`. Build metadata is refused
    /// (the release workflow never makes it), and so is any spelling SemVer
    /// would print differently, so the text is safe in URLs and file names.
    pub fn parse(tag: &str) -> Option<Self> {
        let text = tag.strip_prefix('v').unwrap_or(tag);
        let semver = Version::parse(text).ok()?;
        if !semver.build.is_empty() || semver.to_string() != text {
            return None;
        }
        Some(Self {
            semver,
            text: text.to_owned(),
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.semver.pre.is_empty()
    }

    /// The tag a release of this version has.
    pub fn tag(&self) -> String {
        format!("v{}", self.text)
    }
}

impl std::fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// The running build's version.
pub fn current() -> ReleaseVersion {
    ReleaseVersion::parse(env!("CARGO_PKG_VERSION")).expect("the package version is SemVer")
}

/// Whether `latest` is offered to a build at `current`: only a stable
/// release newer than it, so a pre-release build is offered the release it
/// previews (1.2.0 for 1.2.0-rc.1) but never another pre-release.
pub fn is_update(latest: &ReleaseVersion, current: &ReleaseVersion) -> bool {
    !latest.is_prerelease() && latest.semver > current.semver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> ReleaseVersion {
        ReleaseVersion::parse(text).unwrap()
    }

    #[test]
    fn parses_tags() {
        assert_eq!(v("v1.2.3").text, "1.2.3");
        assert_eq!(v("1.2.3").text, "1.2.3");
        assert_eq!(v("v1.2.3-rc.1").text, "1.2.3-rc.1");
        assert!(v("v1.2.3-rc.1").is_prerelease());
        assert_eq!(v("1.2.3").tag(), "v1.2.3");
        for bad in [
            "",
            "v",
            "vv1.2.3",
            "V1.2.3",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3+build.5",
            "1.2.3-rc/1",
            " 1.2.3",
            "1.2.3\n",
            "../1.2.3",
        ] {
            assert!(ReleaseVersion::parse(bad).is_none(), "{bad:?}");
        }
        current();
    }

    #[test]
    fn offers_only_newer_stable_releases() {
        assert!(is_update(&v("v1.2.4"), &v("1.2.3")));
        assert!(is_update(&v("v2.0.0"), &v("1.10.0")));
        assert!(is_update(&v("v1.10.0"), &v("1.9.0")), "numeric, not text");
        assert!(!is_update(&v("v1.2.3"), &v("1.2.3")), "same version");
        assert!(!is_update(&v("v1.2.2"), &v("1.2.3")), "older");
        assert!(!is_update(&v("v1.3.0-rc.1"), &v("1.2.3")), "pre-release");
    }

    #[test]
    fn prerelease_builds_get_the_release_they_preview() {
        let rc = v("1.2.0-rc.1");
        assert!(is_update(&v("v1.2.0"), &rc));
        assert!(is_update(&v("v1.2.1"), &rc));
        assert!(!is_update(&v("v1.1.9"), &rc));
        assert!(!is_update(&v("v1.2.0-rc.2"), &rc));
    }
}
