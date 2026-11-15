//! Mixer view model: rate-limits volume commands while a slider is dragged
//! and keeps the dragged value on screen until the backend catches up.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use pcvc_audio::AudioState;
use pcvc_protocol::Command;

/// Minimum spacing between two volume commands for the same control.
pub const SEND_INTERVAL: Duration = Duration::from_millis(30);

/// How long a released slider keeps showing its own value when the backend
/// reports a different one (for example because it rounds volumes).
pub const RELEASE_GRACE: Duration = Duration::from_millis(300);

/// Volumes closer than this are considered equal.
const VOLUME_EPSILON: f32 = 0.5;

/// A volume control in the mixer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Master,
    Session(String),
}

#[derive(Debug)]
struct Entry {
    last_sent: Instant,
    pending: Option<Command>,
}

/// Per-target throttle: the first change goes out at once, later ones at
/// most once per interval, and the last value is never dropped.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    entries: HashMap<Target, Entry>,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            entries: HashMap::new(),
        }
    }

    /// Returns the command if it may be sent now; otherwise it is held as
    /// the target's pending command, replacing any earlier one.
    pub fn push(&mut self, target: Target, command: Command, now: Instant) -> Option<Command> {
        match self.entries.get_mut(&target) {
            Some(entry) if now.duration_since(entry.last_sent) < self.interval => {
                entry.pending = Some(command);
                None
            }
            _ => {
                self.mark_sent(target, now);
                Some(command)
            }
        }
    }

    /// Records a command sent outside the throttle; any pending one for the
    /// target is dropped because `command` supersedes it.
    pub fn mark_sent(&mut self, target: Target, now: Instant) {
        self.entries.insert(
            target,
            Entry {
                last_sent: now,
                pending: None,
            },
        );
    }

    /// Takes the target's pending command regardless of the interval.
    pub fn flush(&mut self, target: &Target, now: Instant) -> Option<Command> {
        let entry = self.entries.get_mut(target)?;
        let command = entry.pending.take()?;
        entry.last_sent = now;
        Some(command)
    }

    /// Pending commands whose interval has elapsed. Idle entries are pruned.
    pub fn due(&mut self, now: Instant) -> Vec<Command> {
        let interval = self.interval;
        let mut due = Vec::new();
        self.entries.retain(|_, entry| {
            let elapsed = now.duration_since(entry.last_sent) >= interval;
            if elapsed && let Some(command) = entry.pending.take() {
                entry.last_sent = now;
                due.push(command);
                return true;
            }
            entry.pending.is_some() || !elapsed
        });
        due
    }

    pub fn has_pending(&self, target: &Target) -> bool {
        self.entries
            .get(target)
            .is_some_and(|entry| entry.pending.is_some())
    }

    pub fn any_pending(&self) -> bool {
        self.entries.values().any(|entry| entry.pending.is_some())
    }
}

#[derive(Debug, Clone, Copy)]
struct Override {
    volume: f32,
    released_at: Option<Instant>,
}

#[derive(Debug)]
pub struct Mixer {
    throttle: Throttle,
    /// Values shown instead of the backend's while dragging.
    overrides: HashMap<Target, Override>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            throttle: Throttle::new(SEND_INTERVAL),
            overrides: HashMap::new(),
        }
    }
}

/// The volume command for `target`. Sessions take volume and mute together.
pub fn volume_command(target: &Target, volume: f32, session_muted: bool) -> Command {
    match target {
        Target::Master => Command::SetMaster {
            volume: Some(volume),
            muted: None,
        },
        Target::Session(id) => Command::SetSession {
            session_id: id.clone(),
            volume,
            muted: session_muted,
        },
    }
}

impl Mixer {
    /// A slider moved. Returns the command to send now, if any.
    pub fn slide(
        &mut self,
        target: Target,
        command: Command,
        volume: f32,
        now: Instant,
    ) -> Option<Command> {
        self.overrides.insert(
            target.clone(),
            Override {
                volume,
                released_at: None,
            },
        );
        self.throttle.push(target, command, now)
    }

    /// A slider was let go. Returns the final value if it is still held back.
    pub fn release(&mut self, target: &Target, now: Instant) -> Option<Command> {
        if let Some(o) = self.overrides.get_mut(target) {
            o.released_at = Some(now);
        }
        self.throttle.flush(target, now)
    }

    /// A command that must go out immediately (a mute toggle). It supersedes
    /// anything pending for the target.
    pub fn send_now(&mut self, target: Target, now: Instant) {
        self.throttle.mark_sent(target, now);
    }

    /// Periodic work while [`Mixer::needs_tick`]: due commands to send, and
    /// expiry of released overrides.
    pub fn tick(&mut self, now: Instant) -> Vec<Command> {
        self.overrides.retain(|_, o| {
            o.released_at
                .is_none_or(|at| now.duration_since(at) < RELEASE_GRACE)
        });
        self.throttle.due(now)
    }

    pub fn needs_tick(&self) -> bool {
        self.throttle.any_pending() || self.overrides.values().any(|o| o.released_at.is_some())
    }

    /// Drops overrides the backend has caught up with or whose control is
    /// gone.
    pub fn on_state(&mut self, state: &AudioState) {
        let device = state.default_device.as_ref();
        let throttle = &self.throttle;
        self.overrides.retain(|target, o| {
            let actual = match target {
                Target::Master => device.map(|d| d.volume),
                Target::Session(id) => device
                    .and_then(|d| d.sessions.iter().find(|s| &s.id == id))
                    .map(|s| s.volume),
            };
            match actual {
                None => false,
                Some(v) => (v - o.volume).abs() >= VOLUME_EPSILON || throttle.has_pending(target),
            }
        });
    }

    /// The value to show for `target`, given the backend's.
    pub fn volume(&self, target: &Target, actual: f32) -> f32 {
        self.overrides.get(target).map_or(actual, |o| o.volume)
    }
}

#[cfg(test)]
mod tests {
    use pcvc_audio::{Device, Session};

    use super::*;

    fn master(v: f32) -> Command {
        volume_command(&Target::Master, v, false)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn throttle_sends_first_then_spaces_out_and_keeps_last() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        assert_eq!(
            throttle.push(Target::Master, master(1.0), t0),
            Some(master(1.0))
        );
        assert_eq!(
            throttle.push(Target::Master, master(2.0), t0 + ms(10)),
            None
        );
        assert_eq!(
            throttle.push(Target::Master, master(3.0), t0 + ms(20)),
            None
        );
        assert!(throttle.due(t0 + ms(25)).is_empty());
        assert_eq!(throttle.due(t0 + ms(30)), [master(3.0)]);
        assert!(!throttle.any_pending());
        // Spacing restarts from the flushed send.
        assert_eq!(
            throttle.push(Target::Master, master(4.0), t0 + ms(40)),
            None
        );
        assert_eq!(
            throttle.push(Target::Master, master(5.0), t0 + ms(61)),
            Some(master(5.0))
        );
        assert!(!throttle.any_pending());
    }

    #[test]
    fn throttle_targets_are_independent() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        let a = Target::Session("a".into());
        let cmd = |v| volume_command(&a, v, false);
        assert!(throttle.push(Target::Master, master(1.0), t0).is_some());
        assert_eq!(
            throttle.push(a.clone(), cmd(1.0), t0 + ms(1)),
            Some(cmd(1.0))
        );
        assert_eq!(throttle.push(a.clone(), cmd(2.0), t0 + ms(2)), None);
        assert!(throttle.has_pending(&a));
        assert!(!throttle.has_pending(&Target::Master));
        assert_eq!(throttle.flush(&a, t0 + ms(3)), Some(cmd(2.0)));
        assert_eq!(throttle.flush(&a, t0 + ms(4)), None);
    }

    #[test]
    fn throttle_prunes_idle_entries() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        throttle.push(Target::Master, master(1.0), t0);
        throttle.due(t0 + ms(31));
        assert!(throttle.entries.is_empty());
    }

    #[test]
    fn mark_sent_drops_pending() {
        let t0 = Instant::now();
        let mut throttle = Throttle::new(ms(30));
        throttle.push(Target::Master, master(1.0), t0);
        throttle.push(Target::Master, master(2.0), t0 + ms(5));
        throttle.mark_sent(Target::Master, t0 + ms(6));
        assert!(!throttle.any_pending());
        assert_eq!(throttle.push(Target::Master, master(3.0), t0 + ms(7)), None);
    }

    fn state(master: f32, sessions: &[(&str, f32)]) -> AudioState {
        let mut device = Device::new("d", "D", master);
        for (id, v) in sessions {
            device = device.with_session(Session::new(format!("d|{id}"), *id, *v));
        }
        AudioState {
            devices: Vec::new(),
            default_device: Some(device),
        }
    }

    #[test]
    fn drag_shows_local_value_until_backend_agrees() {
        let t0 = Instant::now();
        let mut mixer = Mixer::default();
        assert!(
            mixer
                .slide(Target::Master, master(40.0), 40.0, t0)
                .is_some()
        );
        assert!(
            mixer
                .slide(Target::Master, master(45.0), 45.0, t0 + ms(5))
                .is_none()
        );
        // A stale snapshot does not move the slider back.
        mixer.on_state(&state(40.0, &[]));
        assert_eq!(mixer.volume(&Target::Master, 40.0), 45.0);
        assert_eq!(
            mixer.release(&Target::Master, t0 + ms(8)),
            Some(master(45.0))
        );
        assert!(mixer.needs_tick());
        mixer.on_state(&state(45.0, &[]));
        assert_eq!(mixer.volume(&Target::Master, 45.0), 45.0);
        assert!(!mixer.needs_tick());
        // Remote changes show once nothing is being dragged.
        assert_eq!(mixer.volume(&Target::Master, 70.0), 70.0);
    }

    #[test]
    fn matching_snapshot_keeps_override_while_pending() {
        let t0 = Instant::now();
        let mut mixer = Mixer::default();
        mixer.slide(Target::Master, master(40.0), 40.0, t0);
        mixer.slide(Target::Master, master(40.0), 40.0, t0 + ms(5));
        mixer.on_state(&state(40.0, &[]));
        assert!(mixer.needs_tick());
        assert_eq!(mixer.tick(t0 + ms(30)), [master(40.0)]);
    }

    #[test]
    fn released_override_expires_after_grace() {
        let t0 = Instant::now();
        let mut mixer = Mixer::default();
        mixer.slide(Target::Master, master(33.0), 33.0, t0);
        mixer.release(&Target::Master, t0 + ms(1));
        // The backend settles on a slightly different value.
        mixer.on_state(&state(32.0, &[]));
        assert_eq!(mixer.volume(&Target::Master, 32.0), 33.0);
        assert!(mixer.tick(t0 + ms(100)).is_empty());
        assert_eq!(mixer.volume(&Target::Master, 32.0), 33.0);
        mixer.tick(t0 + RELEASE_GRACE + ms(2));
        assert_eq!(mixer.volume(&Target::Master, 32.0), 32.0);
        assert!(!mixer.needs_tick());
    }

    #[test]
    fn overrides_of_vanished_sessions_are_dropped() {
        let t0 = Instant::now();
        let mut mixer = Mixer::default();
        let a = Target::Session("d|a".into());
        mixer.slide(a.clone(), volume_command(&a, 10.0, true), 10.0, t0);
        mixer.on_state(&state(50.0, &[("a", 90.0)]));
        assert_eq!(mixer.volume(&a, 90.0), 10.0);
        mixer.on_state(&state(50.0, &[("b", 90.0)]));
        assert_eq!(mixer.volume(&a, 90.0), 90.0);
    }

    #[test]
    fn session_commands_carry_mute() {
        let a = Target::Session("x".into());
        assert_eq!(
            volume_command(&a, 12.0, true),
            Command::SetSession {
                session_id: "x".into(),
                volume: 12.0,
                muted: true
            }
        );
    }
}
