//! Mixer view model: rate-limits volume changes while a slider moves and
//! shows the user's own values until the backend reports them back.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use pcvc_audio::{AudioState, Device};

/// Minimum spacing between two volume commands for the same control.
pub const SEND_INTERVAL: Duration = Duration::from_millis(30);

/// How long a local value is shown after its last change when the backend
/// reports something else (a value it rounded, or a concurrent remote
/// change, which then wins).
pub const OVERRIDE_GRACE: Duration = Duration::from_millis(300);

/// Volumes closer than this are considered equal.
const VOLUME_EPSILON: f32 = 0.5;

/// A volume control on the default device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Master,
    Session(String),
}

/// Current backend volume and mute of `target` on `device`.
pub fn control(device: &Device, target: &Target) -> Option<(f32, bool)> {
    match target {
        Target::Master => Some((device.volume, device.muted)),
        Target::Session(id) => device
            .sessions
            .iter()
            .find(|s| &s.id == id)
            .map(|s| (s.volume, s.muted)),
    }
}

#[derive(Debug)]
struct Entry<T> {
    last_sent: Instant,
    pending: Option<T>,
}

/// Per-target throttle: the first value goes out at once, later ones at
/// most once per interval, and the last value is never dropped.
#[derive(Debug)]
pub struct Throttle<T> {
    interval: Duration,
    entries: HashMap<Target, Entry<T>>,
}

impl<T> Throttle<T> {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            entries: HashMap::new(),
        }
    }

    /// Returns the value if it may be sent now; otherwise it is held as the
    /// target's pending value, replacing any earlier one.
    pub fn push(&mut self, target: Target, value: T, now: Instant) -> Option<T> {
        self.prune(now);
        match self.entries.get_mut(&target) {
            Some(entry) if now.duration_since(entry.last_sent) < self.interval => {
                entry.pending = Some(value);
                None
            }
            _ => {
                self.mark_sent(target, now);
                Some(value)
            }
        }
    }

    /// Records a send made outside the throttle, which supersedes any
    /// pending value for the target.
    pub fn mark_sent(&mut self, target: Target, now: Instant) {
        self.prune(now);
        self.entries.insert(
            target,
            Entry {
                last_sent: now,
                pending: None,
            },
        );
    }

    /// Takes the target's pending value regardless of the interval.
    pub fn flush(&mut self, target: &Target, now: Instant) -> Option<T> {
        let entry = self.entries.get_mut(target)?;
        let value = entry.pending.take()?;
        entry.last_sent = now;
        Some(value)
    }

    /// Pending values whose interval has elapsed.
    pub fn due(&mut self, now: Instant) -> Vec<(Target, T)> {
        let interval = self.interval;
        let due = self
            .entries
            .iter_mut()
            .filter(|(_, entry)| now.duration_since(entry.last_sent) >= interval)
            .filter_map(|(target, entry)| {
                let value = entry.pending.take()?;
                entry.last_sent = now;
                Some((target.clone(), value))
            })
            .collect();
        self.prune(now);
        due
    }

    /// When the next pending value becomes due.
    pub fn next_due(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter(|entry| entry.pending.is_some())
            .map(|entry| entry.last_sent + self.interval)
            .min()
    }

    pub fn has_pending(&self, target: &Target) -> bool {
        self.entries
            .get(target)
            .is_some_and(|entry| entry.pending.is_some())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Forgets entries that hold nothing and no longer limit a send.
    fn prune(&mut self, now: Instant) {
        let interval = self.interval;
        self.entries.retain(|_, entry| {
            entry.pending.is_some() || now.duration_since(entry.last_sent) < interval
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct Shown<T> {
    value: T,
    changed_at: Instant,
}

/// Local values shown in place of the backend's while they are in flight.
#[derive(Debug)]
pub struct Mixer {
    throttle: Throttle<f32>,
    volumes: HashMap<Target, Shown<f32>>,
    mutes: HashMap<Target, Shown<bool>>,
    /// The default device the local values belong to.
    device_id: Option<String>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            throttle: Throttle::new(SEND_INTERVAL),
            volumes: HashMap::new(),
            mutes: HashMap::new(),
            device_id: None,
        }
    }
}

impl Mixer {
    /// A slider moved. Returns the volume to send now, if any.
    pub fn slide(&mut self, target: Target, volume: f32, now: Instant) -> Option<f32> {
        self.volumes.insert(
            target.clone(),
            Shown {
                value: volume,
                changed_at: now,
            },
        );
        self.throttle.push(target, volume, now)
    }

    /// A slider was let go. Returns the final volume if it is still held.
    pub fn release(&mut self, target: &Target, now: Instant) -> Option<f32> {
        self.throttle.flush(target, now)
    }

    /// The mute button was pressed; returns the new mute state to send.
    /// A session's mute command also carries its volume, so it supersedes
    /// any volume held for it.
    pub fn toggle_mute(&mut self, target: Target, actual_muted: bool, now: Instant) -> bool {
        let muted = !self.muted(&target, actual_muted);
        if matches!(target, Target::Session(_)) {
            self.throttle.mark_sent(target.clone(), now);
        }
        self.mutes.insert(
            target,
            Shown {
                value: muted,
                changed_at: now,
            },
        );
        muted
    }

    /// Volumes now due, and expiry of local values past their grace.
    pub fn tick(&mut self, now: Instant) -> Vec<(Target, f32)> {
        let throttle = &self.throttle;
        self.volumes.retain(|target, shown| {
            throttle.has_pending(target) || now.duration_since(shown.changed_at) < OVERRIDE_GRACE
        });
        self.mutes
            .retain(|_, shown| now.duration_since(shown.changed_at) < OVERRIDE_GRACE);
        self.throttle.due(now)
    }

    /// When [`Mixer::tick`] next has work, if ever.
    pub fn next_deadline(&self) -> Option<Instant> {
        let expiries = self
            .volumes
            .values()
            .map(|s| s.changed_at)
            .chain(self.mutes.values().map(|s| s.changed_at))
            .map(|at| at + OVERRIDE_GRACE);
        expiries.chain(self.throttle.next_due()).min()
    }

    /// Drops local values the backend has caught up with or whose control
    /// is gone. A different default device discards everything, including
    /// held volumes meant for the old one.
    pub fn on_state(&mut self, state: &AudioState) {
        let device = state.default_device.as_ref();
        let device_id = device.map(|d| d.id.clone());
        if device_id != self.device_id {
            self.device_id = device_id;
            self.volumes.clear();
            self.mutes.clear();
            self.throttle.clear();
            return;
        }
        let Some(device) = device else { return };
        let throttle = &self.throttle;
        self.volumes
            .retain(|target, shown| match control(device, target) {
                None => false,
                Some((v, _)) => {
                    (v - shown.value).abs() >= VOLUME_EPSILON || throttle.has_pending(target)
                }
            });
        self.mutes.retain(|target, shown| {
            control(device, target).is_some_and(|(_, muted)| muted != shown.value)
        });
    }

    /// The volume to show for `target`, given the backend's.
    pub fn volume(&self, target: &Target, actual: f32) -> f32 {
        self.volumes.get(target).map_or(actual, |s| s.value)
    }

    /// The mute state to show (and to send with session volumes).
    pub fn muted(&self, target: &Target, actual: bool) -> bool {
        self.mutes.get(target).map_or(actual, |s| s.value)
    }
}

#[cfg(test)]
mod tests {
    use pcvc_audio::Session;

    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn throttle_sends_first_then_spaces_out_and_keeps_last() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        let m = Target::Master;
        assert_eq!(throttle.push(m.clone(), 1, t0), Some(1));
        assert_eq!(throttle.push(m.clone(), 2, t0 + ms(10)), None);
        assert_eq!(throttle.push(m.clone(), 3, t0 + ms(20)), None);
        assert_eq!(throttle.next_due(), Some(t0 + ms(30)));
        assert!(throttle.due(t0 + ms(25)).is_empty());
        assert_eq!(throttle.due(t0 + ms(30)), [(m.clone(), 3)]);
        assert_eq!(throttle.next_due(), None);
        // Spacing restarts from the flushed send.
        assert_eq!(throttle.push(m.clone(), 4, t0 + ms(40)), None);
        assert_eq!(throttle.push(m.clone(), 5, t0 + ms(61)), Some(5));
        assert!(!throttle.has_pending(&m));
    }

    #[test]
    fn throttle_targets_are_independent() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        let a = Target::Session("a".into());
        assert!(throttle.push(Target::Master, 1, t0).is_some());
        assert_eq!(throttle.push(a.clone(), 1, t0 + ms(1)), Some(1));
        assert_eq!(throttle.push(a.clone(), 2, t0 + ms(2)), None);
        assert!(throttle.has_pending(&a));
        assert!(!throttle.has_pending(&Target::Master));
        assert_eq!(throttle.flush(&a, t0 + ms(3)), Some(2));
        assert_eq!(throttle.flush(&a, t0 + ms(4)), None);
    }

    #[test]
    fn throttle_prunes_idle_entries() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        throttle.push(Target::Master, 1, t0);
        throttle.mark_sent(Target::Session("a".into()), t0);
        throttle.push(Target::Session("b".into()), 1, t0 + ms(31));
        assert_eq!(throttle.entries.len(), 1);
    }

    #[test]
    fn mark_sent_drops_pending() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        throttle.push(Target::Master, 1, t0);
        throttle.push(Target::Master, 2, t0 + ms(5));
        throttle.mark_sent(Target::Master, t0 + ms(6));
        assert_eq!(throttle.next_due(), None);
        assert_eq!(throttle.push(Target::Master, 3, t0 + ms(7)), None);
    }

    fn state(device: &str, master: (f32, bool), sessions: &[(&str, f32, bool)]) -> AudioState {
        let mut d = Device::new(device, device, master.0);
        d.muted = master.1;
        for (id, v, muted) in sessions {
            let mut s = Session::new(format!("{device}|{id}"), *id, *v);
            s.muted = *muted;
            d = d.with_session(s);
        }
        AudioState {
            devices: Vec::new(),
            default_device: Some(d),
        }
    }

    fn mixer_on(device: &str) -> Mixer {
        let mut mixer = Mixer::default();
        mixer.on_state(&state(device, (50.0, false), &[]));
        mixer
    }

    #[test]
    fn drag_shows_local_value_until_backend_agrees() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("d");
        let m = Target::Master;
        assert_eq!(mixer.slide(m.clone(), 40.0, t0), Some(40.0));
        assert_eq!(mixer.slide(m.clone(), 45.0, t0 + ms(5)), None);
        // A stale snapshot does not move the slider back.
        mixer.on_state(&state("d", (40.0, false), &[]));
        assert_eq!(mixer.volume(&m, 40.0), 45.0);
        assert_eq!(mixer.release(&m, t0 + ms(8)), Some(45.0));
        mixer.on_state(&state("d", (45.0, false), &[]));
        assert_eq!(mixer.next_deadline(), None);
        // Remote changes show once nothing local is in flight.
        assert_eq!(mixer.volume(&m, 70.0), 70.0);
    }

    #[test]
    fn local_value_yields_to_backend_after_grace_even_without_release() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("d");
        let m = Target::Master;
        // Keyboard and wheel changes never produce a release.
        mixer.slide(m.clone(), 51.0, t0);
        mixer.on_state(&state("d", (70.0, false), &[]));
        assert_eq!(mixer.volume(&m, 70.0), 51.0);
        assert_eq!(mixer.next_deadline(), Some(t0 + OVERRIDE_GRACE));
        assert!(mixer.tick(t0 + ms(100)).is_empty());
        assert_eq!(mixer.volume(&m, 70.0), 51.0);
        mixer.tick(t0 + OVERRIDE_GRACE);
        assert_eq!(mixer.volume(&m, 70.0), 70.0);
        assert_eq!(mixer.next_deadline(), None);
    }

    #[test]
    fn held_volume_is_sent_by_tick() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("d");
        mixer.slide(Target::Master, 40.0, t0);
        mixer.slide(Target::Master, 41.0, t0 + ms(5));
        assert_eq!(mixer.next_deadline(), Some(t0 + SEND_INTERVAL));
        assert_eq!(mixer.tick(t0 + SEND_INTERVAL), [(Target::Master, 41.0)]);
    }

    #[test]
    fn mute_toggles_are_optimistic() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("d");
        let a = Target::Session("d|a".into());
        mixer.on_state(&state("d", (50.0, false), &[("a", 60.0, false)]));
        assert!(mixer.toggle_mute(a.clone(), false, t0));
        // A second click before the backend answers undoes the first.
        assert!(!mixer.toggle_mute(a.clone(), false, t0 + ms(10)));
        assert!(mixer.toggle_mute(a.clone(), false, t0 + ms(20)));
        assert!(mixer.muted(&a, false));
        mixer.on_state(&state("d", (50.0, false), &[("a", 60.0, true)]));
        assert!(!mixer.muted(&a, false));
    }

    #[test]
    fn session_mute_supersedes_held_volume_but_master_mute_does_not() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("d");
        let a = Target::Session("d|a".into());
        mixer.slide(a.clone(), 10.0, t0);
        mixer.slide(a.clone(), 11.0, t0 + ms(1));
        mixer.toggle_mute(a.clone(), false, t0 + ms(2));
        assert_eq!(mixer.release(&a, t0 + ms(3)), None);

        mixer.slide(Target::Master, 10.0, t0);
        mixer.slide(Target::Master, 11.0, t0 + ms(1));
        mixer.toggle_mute(Target::Master, false, t0 + ms(2));
        assert_eq!(mixer.release(&Target::Master, t0 + ms(3)), Some(11.0));
    }

    #[test]
    fn device_switch_discards_local_values() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("speakers");
        mixer.slide(Target::Master, 80.0, t0);
        mixer.slide(Target::Master, 81.0, t0 + ms(1));
        mixer.on_state(&state("headphones", (20.0, false), &[]));
        assert_eq!(mixer.volume(&Target::Master, 20.0), 20.0);
        assert_eq!(mixer.release(&Target::Master, t0 + ms(2)), None);
        assert_eq!(mixer.next_deadline(), None);
    }

    #[test]
    fn local_values_of_vanished_sessions_are_dropped() {
        let t0 = Instant::now();
        let mut mixer = mixer_on("d");
        let a = Target::Session("d|a".into());
        mixer.slide(a.clone(), 10.0, t0);
        mixer.on_state(&state("d", (50.0, false), &[("a", 90.0, false)]));
        assert_eq!(mixer.volume(&a, 90.0), 10.0);
        mixer.on_state(&state("d", (50.0, false), &[("b", 90.0, false)]));
        assert_eq!(mixer.volume(&a, 90.0), 90.0);
    }
}
