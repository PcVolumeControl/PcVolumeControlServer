//! When automatic checks run: shortly after launch if the last successful
//! check is a day old, then once a day while the app runs, and an hour
//! after a failed attempt.

use std::time::{Duration, SystemTime};

/// Keeps the check out of the way of startup.
pub const STARTUP_DELAY: Duration = Duration::from_secs(20);
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// How often the app asks [`is_due`].
pub const POLL: Duration = Duration::from_secs(60);

/// Whether an automatic check is due at `now`, `uptime` after launch. A
/// clock set back before a recorded time counts that time as long past.
pub fn is_due(
    now: SystemTime,
    uptime: Duration,
    last_success: Option<SystemTime>,
    last_attempt: Option<SystemTime>,
) -> bool {
    let elapsed = |since: SystemTime, at_least: Duration| {
        now.duration_since(since)
            .map_or(true, |elapsed| elapsed >= at_least)
    };
    uptime >= STARTUP_DELAY
        && last_attempt.is_none_or(|t| elapsed(t, RETRY_AFTER))
        && last_success.is_none_or(|t| elapsed(t, INTERVAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    /// A fake clock: `hours` after an arbitrary epoch.
    fn at(hours: f64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000) + HOUR.mul_f64(hours)
    }

    #[test]
    fn waits_for_startup() {
        assert!(!is_due(at(0.0), Duration::ZERO, None, None));
        assert!(!is_due(
            at(0.0),
            STARTUP_DELAY - Duration::from_secs(1),
            None,
            None
        ));
        assert!(is_due(at(0.0), STARTUP_DELAY, None, None));
    }

    #[test]
    fn once_a_day() {
        let up = HOUR;
        let checked = Some(at(10.0));
        assert!(!is_due(at(10.0), up, checked, checked));
        assert!(!is_due(at(33.9), up, checked, checked));
        assert!(is_due(at(34.0), up, checked, checked));
        // A restart soon after a check waits for the rest of the day.
        assert!(!is_due(at(12.0), STARTUP_DELAY, checked, None));
        assert!(is_due(at(40.0), STARTUP_DELAY, checked, None));
    }

    #[test]
    fn retries_failures_hourly() {
        let up = HOUR;
        let success = Some(at(0.0));
        let failed = Some(at(30.0));
        assert!(!is_due(at(30.5), up, success, failed));
        assert!(is_due(at(31.0), up, success, failed));
        assert!(!is_due(at(0.5), up, None, Some(at(0.0))), "never succeeded");
        assert!(is_due(at(1.0), up, None, Some(at(0.0))));
    }

    #[test]
    fn clock_set_back() {
        let up = HOUR;
        assert!(is_due(at(5.0), up, Some(at(100.0)), None));
        assert!(is_due(at(5.0), up, Some(at(100.0)), Some(at(100.0))));
    }
}
