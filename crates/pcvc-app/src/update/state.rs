//! What the update feature is doing, as the UI shows it, and what is kept
//! between runs.

use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::download::Progress;
use super::release::{CheckError, CheckOutcome, Release};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Checking {
        manual: bool,
    },
    UpToDate,
    NoReleases,
    Available,
    Downloading {
        done: u64,
        total: u64,
    },
    Verifying,
    /// The installer runs; it stops the app and starts the new version.
    Launching,
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A check the user asked for.
    Check(String),
    /// Installing; the release stays available to try again.
    Install(String),
}

/// The update state machine. Results arrive from the worker; the methods
/// that start work return whether (or what) to start, so nothing starts
/// twice.
#[derive(Debug, Clone)]
pub struct Updater {
    phase: Phase,
    /// The phase an automatic check returns to if it fails.
    before_check: Phase,
    /// The newer release last found.
    release: Option<Release>,
    pub saved: Saved,
    /// The last automatic attempt, successful or not.
    pub last_attempt: Option<SystemTime>,
}

impl Updater {
    pub fn new(saved: Saved) -> Self {
        Self {
            phase: Phase::Idle,
            before_check: Phase::Idle,
            release: None,
            saved,
            last_attempt: None,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The release to show, unless the user skipped its version.
    pub fn offered(&self) -> Option<&Release> {
        self.release
            .as_ref()
            .filter(|r| self.saved.skipped_version.as_deref() != Some(r.version.text.as_str()))
    }

    pub fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Checking { .. }
                | Phase::Downloading { .. }
                | Phase::Verifying
                | Phase::Launching
        )
    }

    pub fn installing(&self) -> bool {
        matches!(
            self.phase,
            Phase::Downloading { .. } | Phase::Verifying | Phase::Launching
        )
    }

    /// Starts a check unless one is running or an install is under way.
    pub fn start_check(&mut self, manual: bool, now: SystemTime) -> bool {
        if self.busy() {
            return false;
        }
        if !manual {
            self.last_attempt = Some(now);
        }
        self.before_check = std::mem::replace(&mut self.phase, Phase::Checking { manual });
        true
    }

    /// Records a check's result. Returns whether [`Saved`] changed.
    pub fn check_finished(
        &mut self,
        manual: bool,
        result: Result<CheckOutcome, CheckError>,
        now: SystemTime,
    ) -> bool {
        if self.phase != (Phase::Checking { manual }) {
            return false;
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!("update check failed: {e}");
                self.phase = if manual {
                    Phase::Failed(Failure::Check(e.to_string()))
                } else {
                    std::mem::replace(&mut self.before_check, Phase::Idle)
                };
                return false;
            }
        };
        self.saved.last_checked = Some(unix_seconds(now));
        match outcome {
            CheckOutcome::UpToDate { latest } => {
                tracing::info!("up to date; the latest release is {latest}");
                self.release = None;
                self.phase = Phase::UpToDate;
            }
            CheckOutcome::NoReleases => {
                tracing::info!("no releases published yet");
                self.release = None;
                self.phase = Phase::NoReleases;
            }
            CheckOutcome::Available(release) => {
                tracing::info!("version {} is available", release.version);
                // Checking by hand shows a skipped version again.
                if manual
                    && self.saved.skipped_version.as_deref() == Some(release.version.text.as_str())
                {
                    self.saved.skipped_version = None;
                }
                self.release = Some(*release);
                self.phase = Phase::Available;
            }
        }
        true
    }

    /// Skips the offered version. Returns whether [`Saved`] changed.
    pub fn skip(&mut self) -> bool {
        if self.installing() {
            return false;
        }
        let Some(version) = self.offered().map(|r| r.version.text.clone()) else {
            return false;
        };
        self.saved.skipped_version = Some(version);
        if matches!(
            self.phase,
            Phase::Available | Phase::Failed(Failure::Install(_))
        ) {
            self.phase = Phase::Idle;
        }
        true
    }

    /// Starts installing the offered release, if it is offered and nothing
    /// else runs.
    pub fn start_install(&mut self) -> Option<Release> {
        if !matches!(
            self.phase,
            Phase::Available | Phase::Failed(Failure::Install(_))
        ) {
            return None;
        }
        let release = self.offered()?.clone();
        let total = release.asset.as_ref().map_or(0, |a| a.size);
        self.phase = Phase::Downloading { done: 0, total };
        Some(release)
    }

    pub fn progress(&mut self, progress: Progress) {
        if !matches!(self.phase, Phase::Downloading { .. } | Phase::Verifying) {
            return;
        }
        self.phase = match progress {
            Progress::Downloading { done, total } => Phase::Downloading { done, total },
            Progress::Verifying => Phase::Verifying,
        };
    }

    pub fn launched(&mut self) {
        if matches!(self.phase, Phase::Downloading { .. } | Phase::Verifying) {
            self.phase = Phase::Launching;
        }
    }

    pub fn install_failed(&mut self, message: String) {
        if self.installing() {
            tracing::warn!("installing the update failed: {message}");
            self.phase = Phase::Failed(Failure::Install(message));
        }
    }

    /// When the last successful check ran.
    pub fn last_checked(&self) -> Option<SystemTime> {
        self.saved.last_checked.and_then(|s| {
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(s.try_into().ok()?))
        })
    }
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

/// Kept between runs, in its own file next to the config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    /// Unix time of the last successful check.
    pub last_checked: Option<i64>,
    pub skipped_version: Option<String>,
}

impl Saved {
    /// A missing or unreadable file gives the defaults.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("ignoring invalid {}: {e}", path.display());
                Self::default()
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!("reading {} failed: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Writes atomically, keeping keys this version does not know. Fields
    /// that are `None` are left out, which removes them from the file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        const KEYS: [&str; 2] = ["last_checked", "skipped_version"];
        let mut table = toml::Table::try_from(self).map_err(io::Error::other)?;
        if let Some(old) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
        {
            for (key, value) in old {
                if !KEYS.contains(&key.as_str()) {
                    table.entry(key).or_insert(value);
                }
            }
        }
        let text = toml::to_string_pretty(&table).map_err(io::Error::other)?;
        crate::config::write_atomic(path, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::super::release::{Asset, Endpoints};
    use super::super::version::ReleaseVersion;
    use super::*;

    fn release(version: &str) -> Release {
        let version = ReleaseVersion::parse(version).unwrap();
        Release {
            page_url: format!("{}/v{version}", Endpoints::github().page),
            asset: Some(Asset {
                name: "setup.exe".into(),
                size: 100,
                url: "https://github.com/x".into(),
            }),
            checksums: None,
            signature: None,
            version,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn available(u: &mut Updater, version: &str, manual: bool) {
        assert!(u.start_check(manual, now()));
        assert!(u.check_finished(
            manual,
            Ok(CheckOutcome::Available(Box::new(release(version)))),
            now()
        ));
    }

    #[test]
    fn check_outcomes() {
        let mut u = Updater::new(Saved::default());
        assert_eq!(u.phase(), &Phase::Idle);
        assert!(u.start_check(false, now()));
        assert_eq!(u.phase(), &Phase::Checking { manual: false });
        assert!(!u.start_check(true, now()), "one check at a time");
        assert!(u.check_finished(false, Ok(CheckOutcome::NoReleases), now()));
        assert_eq!(u.phase(), &Phase::NoReleases);
        assert_eq!(u.last_checked(), Some(now()));
        assert_eq!(u.last_attempt, Some(now()));

        assert!(u.start_check(true, now()));
        let latest = ReleaseVersion::parse("1.0.0").unwrap();
        u.check_finished(true, Ok(CheckOutcome::UpToDate { latest }), now());
        assert_eq!(u.phase(), &Phase::UpToDate);

        available(&mut u, "2.0.0", false);
        assert_eq!(u.phase(), &Phase::Available);
        assert_eq!(u.offered().unwrap().version.text, "2.0.0");
        // A later check that finds nothing newer withdraws it.
        assert!(u.start_check(true, now()));
        u.check_finished(true, Ok(CheckOutcome::NoReleases), now());
        assert!(u.offered().is_none());
    }

    #[test]
    fn failures_are_quiet_unless_asked_for() {
        let mut u = Updater::new(Saved::default());
        available(&mut u, "2.0.0", false);
        let error = || Err(CheckError::Malformed("x".into()));
        assert!(u.start_check(false, now()));
        assert!(!u.check_finished(false, error(), now()));
        assert_eq!(u.phase(), &Phase::Available, "back to what it was");
        assert!(u.offered().is_some());
        assert_eq!(u.saved.last_checked, Some(unix_seconds(now())));

        assert!(u.start_check(true, now()));
        u.check_finished(true, error(), now());
        assert!(matches!(u.phase(), Phase::Failed(Failure::Check(_))));
        // A stale result for another check is ignored.
        assert!(!u.check_finished(false, Ok(CheckOutcome::NoReleases), now()));
    }

    #[test]
    fn install_runs_once_through_its_phases() {
        let mut u = Updater::new(Saved::default());
        assert!(u.start_install().is_none(), "nothing offered");
        available(&mut u, "2.0.0", false);
        assert!(u.start_install().is_some());
        assert_eq!(
            u.phase(),
            &Phase::Downloading {
                done: 0,
                total: 100
            }
        );
        assert!(u.start_install().is_none(), "no double trigger");
        assert!(!u.start_check(true, now()), "no check while installing");
        assert!(!u.skip(), "no skipping while installing");
        u.progress(Progress::Downloading {
            done: 50,
            total: 100,
        });
        assert_eq!(
            u.phase(),
            &Phase::Downloading {
                done: 50,
                total: 100
            }
        );
        u.progress(Progress::Verifying);
        assert_eq!(u.phase(), &Phase::Verifying);
        u.launched();
        assert_eq!(u.phase(), &Phase::Launching);
        assert!(u.start_install().is_none());
        u.install_failed("installer exited".into());
        assert_eq!(
            u.phase(),
            &Phase::Failed(Failure::Install("installer exited".into()))
        );
        assert!(u.offered().is_some(), "still offered");
        assert!(u.start_install().is_some(), "can try again");
        u.install_failed("checksum".into());
        assert!(u.start_check(true, now()));
    }

    #[test]
    fn progress_outside_an_install_is_ignored() {
        let mut u = Updater::new(Saved::default());
        u.progress(Progress::Verifying);
        u.launched();
        u.install_failed("x".into());
        assert_eq!(u.phase(), &Phase::Idle);
    }

    #[test]
    fn skipping() {
        let mut u = Updater::new(Saved::default());
        assert!(!u.skip(), "nothing to skip");
        available(&mut u, "2.0.0", false);
        assert!(u.skip());
        assert_eq!(u.saved.skipped_version.as_deref(), Some("2.0.0"));
        assert!(u.offered().is_none());
        assert_eq!(u.phase(), &Phase::Idle);
        assert!(u.start_install().is_none());
        // Automatic checks keep it hidden; a newer version shows.
        available(&mut u, "2.0.0", false);
        assert!(u.offered().is_none());
        available(&mut u, "2.1.0", false);
        assert_eq!(u.offered().unwrap().version.text, "2.1.0");
        // Checking by hand shows a skipped version again.
        assert!(u.skip());
        available(&mut u, "2.1.0", true);
        assert_eq!(u.offered().unwrap().version.text, "2.1.0");
        assert_eq!(u.saved.skipped_version, None);
    }

    #[test]
    fn saved_state_round_trips_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-state.toml");
        assert_eq!(Saved::load(&path), Saved::default());
        std::fs::write(&path, "future = 1\nlast_checked = 5\n").unwrap();
        assert_eq!(Saved::load(&path).last_checked, Some(5));
        let saved = Saved {
            last_checked: Some(1_800_000_000),
            skipped_version: Some("1.2.3".into()),
        };
        saved.save(&path).unwrap();
        assert_eq!(Saved::load(&path), saved);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("future = 1"), "{text}");
        // Clearing the skip removes the key.
        let cleared = Saved {
            skipped_version: None,
            ..saved
        };
        cleared.save(&path).unwrap();
        assert_eq!(Saved::load(&path), cleared);

        std::fs::write(&path, "last_checked = \"soon\"").unwrap();
        assert_eq!(Saved::load(&path), Saved::default());
    }
}
