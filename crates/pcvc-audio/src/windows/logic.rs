//! The Windows backend's decisions that need no Core Audio, kept apart so they
//! are tested on every platform.

use std::time::Duration;

use pcvc_protocol::clamp_volume;

use crate::{AudioEvent, AudioState, Session};

/// Name reported for a session with neither a display name nor a readable
/// process image.
pub(crate) const UNKNOWN_SESSION_NAME: &str = "Unknown";

/// The protocol id of an endpoint: the GUID of an `IMMDevice` id of the form
/// `{0.0.0.00000000}.{GUID}`, lowercased and without braces, which is what the
/// C# host reported. Any other shape is returned verbatim.
pub(crate) fn device_id_from_endpoint_id(endpoint_id: &str) -> String {
    endpoint_id
        .rsplit_once(".{")
        .and_then(|(prefix, rest)| {
            let guid = rest.strip_suffix('}')?;
            (prefix.starts_with('{') && is_guid(guid)).then(|| guid.to_ascii_lowercase())
        })
        .unwrap_or_else(|| endpoint_id.to_owned())
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` with hex digits.
fn is_guid(s: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == GROUPS.len()
        && parts
            .iter()
            .zip(GROUPS)
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A Core Audio scalar (0.0 to 1.0) as a protocol percentage, rounded to two
/// decimals so that values set from a percentage read back unchanged.
pub(crate) fn scalar_to_percent(scalar: f32) -> f32 {
    let percent = clamp_volume(scalar * 100.0);
    (percent * 100.0).round() / 100.0
}

/// A protocol percentage, clamped to 0..=100, as a Core Audio scalar.
pub(crate) fn percent_to_scalar(percent: f32) -> f32 {
    clamp_volume(percent) / 100.0
}

/// Whether an `HRESULT` is exactly `S_OK`. Predicates such as
/// `IAudioSessionControl2::IsSystemSoundsSession` answer "no" with `S_FALSE`,
/// which is also a success code.
pub(crate) fn is_s_ok(hresult: i32) -> bool {
    hresult == 0
}

/// `s` trimmed, or `None` if that leaves nothing.
pub(crate) fn non_blank(s: &str) -> Option<String> {
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The usable form of a session display name. A name starting with `@` is an
/// indirect resource string (`@%SystemRoot%\...dll,-202`) and is passed to
/// `resolve_indirect`; if that fails the session has no usable display name.
pub(crate) fn resolve_display_name(
    raw: &str,
    resolve_indirect: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let name = non_blank(raw)?;
    if name.starts_with('@') {
        resolve_indirect(&name).as_deref().and_then(non_blank)
    } else {
        Some(name)
    }
}

/// The file stem of a Windows image path (`C:\Program Files\App\app.exe` gives
/// `app`), matching .NET's `Process.ProcessName`. Both separators are accepted
/// so this behaves the same on every host.
pub(crate) fn process_stem(path: &str) -> Option<String> {
    let file = path.rsplit(['\\', '/']).next()?;
    let stem = match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file,
    };
    non_blank(stem)
}

/// Display name if there is one, else the process name, else "Unknown".
pub(crate) fn session_name(display: Option<&str>, process: Option<&str>) -> String {
    display
        .or(process)
        .unwrap_or(UNKNOWN_SESSION_NAME)
        .to_owned()
}

/// Friendly name of an endpoint, falling back to its protocol id.
pub(crate) fn device_name(friendly: Option<&str>, device_id: &str) -> String {
    friendly
        .and_then(non_blank)
        .unwrap_or_else(|| device_id.to_owned())
}

/// Delay before retrying after `failures` consecutive failed refreshes:
/// 500 ms, doubling up to 30 s.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    const FIRST: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(30);
    let doublings = failures.saturating_sub(1).min(16);
    (FIRST * (1 << doublings)).min(MAX)
}

/// What a Core Audio notification says needs re-reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// An endpoint appeared, disappeared, changed state or was renamed.
    Devices,
    /// The default render endpoint changed.
    Default,
    /// Master volume or mute of the default endpoint.
    Master,
    /// A session was created, expired or disconnected.
    SessionList,
    /// A session's volume, mute or display name.
    SessionValues,
}

/// The parts of the cached state that must be re-read, accumulated across a
/// burst of notifications.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Dirty {
    pub devices: bool,
    pub default: bool,
    pub master: bool,
    pub session_list: bool,
    pub session_values: bool,
}

impl Dirty {
    pub fn all() -> Self {
        Self {
            devices: true,
            default: true,
            master: true,
            session_list: true,
            session_values: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn mark(&mut self, change: Change) {
        match change {
            Change::Devices => self.devices = true,
            Change::Default => self.default = true,
            Change::Master => self.master = true,
            Change::SessionList => self.session_list = true,
            Change::SessionValues => self.session_values = true,
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.devices |= other.devices;
        self.default |= other.default;
        self.master |= other.master;
        self.session_list |= other.session_list;
        self.session_values |= other.session_values;
    }
}

/// Where a session control came from during a session-list refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionSource {
    /// `OnSessionCreated`: definitely a new session.
    Created {
        /// The known session with the same id has this very control.
        same_control: bool,
    },
    /// The session enumerator, which also lists inactive sessions of apps
    /// that have exited.
    Enumerated {
        /// Its id belongs to a session dropped because its process exited.
        exited_before: bool,
        /// It is playing.
        active: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionAction {
    Skip,
    Add,
    /// Re-open the known session with this control: the app re-created its
    /// stream under the same id and the old control is about to expire.
    Replace,
}

/// What to do with a session control found during a session-list refresh,
/// given whether a session with its id is already `known`.
pub(crate) fn session_action(known: bool, source: SessionSource) -> SessionAction {
    match source {
        SessionSource::Created { same_control } => match (known, same_control) {
            (false, _) => SessionAction::Add,
            (true, false) => SessionAction::Replace,
            (true, true) => SessionAction::Skip,
        },
        SessionSource::Enumerated { .. } if known => SessionAction::Skip,
        // An exited app's leftover session only comes back once something
        // plays in it again.
        SessionSource::Enumerated {
            exited_before: true,
            active: false,
        } => SessionAction::Skip,
        SessionSource::Enumerated { .. } => SessionAction::Add,
    }
}

/// Stable-sorts `items` into the order their ids had in `previous`, with ids
/// not in `previous` after the others in their current order. Keeps the
/// phones' session list from reordering when a device's sessions are re-read
/// from scratch.
pub(crate) fn sort_like<T>(items: &mut [T], id_of: impl Fn(&T) -> &str, previous: &[String]) {
    items.sort_by_key(|item| {
        previous
            .iter()
            .position(|id| id == id_of(item))
            .unwrap_or(usize::MAX)
    });
}

/// False only for ids that are certainly capture endpoints
/// (`{0.0.1.00000000}.{GUID}`). The flow digit is not documented, so any
/// other shape counts as possibly render.
pub(crate) fn may_be_render_endpoint(endpoint_id: &str) -> bool {
    !endpoint_id.starts_with("{0.0.1.")
}

/// The events that describe the change from `old` to `new`; empty only when
/// they are equal.
pub(crate) fn diff_states(old: &AudioState, new: &AudioState) -> Vec<AudioEvent> {
    let mut events = Vec::new();
    if old.devices != new.devices {
        events.push(AudioEvent::DevicesChanged);
    }
    match (&old.default_device, &new.default_device) {
        (Some(a), Some(b)) if a.id == b.id => {
            if (a.volume, a.muted) != (b.volume, b.muted) {
                events.push(AudioEvent::MasterChanged);
            }
            diff_sessions(&a.sessions, &b.sessions, &mut events);
        }
        (None, None) => {}
        _ => events.push(AudioEvent::DefaultDeviceChanged),
    }
    // A change the cases above do not name, such as the default device being
    // renamed or sessions being reordered.
    if events.is_empty() && old != new {
        events.push(AudioEvent::DevicesChanged);
    }
    events
}

fn diff_sessions(old: &[Session], new: &[Session], events: &mut Vec<AudioEvent>) {
    for s in old {
        if !new.iter().any(|n| n.id == s.id) {
            events.push(AudioEvent::SessionRemoved { id: s.id.clone() });
        }
    }
    for s in new {
        match old.iter().find(|o| o.id == s.id) {
            None => events.push(AudioEvent::SessionAdded { id: s.id.clone() }),
            Some(o) if o != s => events.push(AudioEvent::SessionChanged { id: s.id.clone() }),
            Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Device, DeviceInfo};

    const GUID: &str = "0f4090a9-dee2-4563-ba29-0ad6b93d9e22";

    #[test]
    fn device_id_is_bare_lowercase_guid() {
        assert_eq!(
            device_id_from_endpoint_id("{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}"),
            GUID
        );
        assert_eq!(
            device_id_from_endpoint_id("{0.0.0.00000000}.{0F4090A9-DEE2-4563-BA29-0AD6B93D9E22}"),
            GUID
        );
        // Capture endpoints use flow 1.
        assert_eq!(
            device_id_from_endpoint_id("{0.0.1.00000000}.{c98e4030-926b-4e62-8c83-1c529574cc51}"),
            "c98e4030-926b-4e62-8c83-1c529574cc51"
        );
    }

    #[test]
    fn unexpected_endpoint_ids_are_kept_verbatim() {
        for id in [
            "",
            "SWD\\MMDEVAPI\\x",
            "{0.0.0.00000000}.{not-a-guid}",
            "{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22",
            "{0.0.0.00000000}.{0f4090a9dee245634ba290ad6b93d9e2200}",
            ".{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}",
        ] {
            assert_eq!(device_id_from_endpoint_id(id), id);
        }
    }

    #[test]
    fn volume_conversion_round_trips_and_clamps() {
        for percent in [0.0, 1.0, 29.0, 33.33, 64.5, 99.99, 100.0] {
            assert_eq!(scalar_to_percent(percent_to_scalar(percent)), percent);
        }
        assert_eq!(scalar_to_percent(0.35), 35.0);
        assert_eq!(scalar_to_percent(1.5), 100.0);
        assert_eq!(scalar_to_percent(-0.1), 0.0);
        assert_eq!(scalar_to_percent(f32::NAN), 0.0);
        assert_eq!(percent_to_scalar(150.0), 1.0);
        assert_eq!(percent_to_scalar(-5.0), 0.0);
        assert_eq!(percent_to_scalar(f32::NAN), 0.0);
    }

    #[test]
    fn only_s_ok_counts_as_yes() {
        assert!(is_s_ok(0));
        // S_FALSE
        assert!(!is_s_ok(1));
        // E_FAIL
        assert!(!is_s_ok(0x8000_4005_u32 as i32));
    }

    #[test]
    fn display_names() {
        let unreachable = |_: &str| -> Option<String> { panic!("not indirect") };
        assert_eq!(
            resolve_display_name("  Spotify ", unreachable).as_deref(),
            Some("Spotify")
        );
        assert_eq!(resolve_display_name("", unreachable), None);
        assert_eq!(resolve_display_name(" \t", unreachable), None);

        let indirect = "@%SystemRoot%\\System32\\AudioSrv.Dll,-202";
        let resolved = resolve_display_name(indirect, |s| {
            assert_eq!(s, indirect);
            Some("System Sounds".into())
        });
        assert_eq!(resolved.as_deref(), Some("System Sounds"));
        assert_eq!(resolve_display_name(indirect, |_| None), None);
        assert_eq!(resolve_display_name(indirect, |_| Some("  ".into())), None);
    }

    #[test]
    fn process_stems() {
        assert_eq!(
            process_stem("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe").as_deref(),
            Some("chrome")
        );
        assert_eq!(
            process_stem("\\Device\\HarddiskVolume2\\a.b\\Discord.Canary.exe").as_deref(),
            Some("Discord.Canary")
        );
        assert_eq!(process_stem("C:/x/noext").as_deref(), Some("noext"));
        assert_eq!(process_stem("C:\\x\\.hidden").as_deref(), Some(".hidden"));
        assert_eq!(process_stem("C:\\x\\"), None);
        assert_eq!(process_stem(""), None);
    }

    #[test]
    fn name_fallbacks() {
        assert_eq!(session_name(Some("Firefox"), Some("firefox")), "Firefox");
        assert_eq!(session_name(None, Some("firefox")), "firefox");
        assert_eq!(session_name(None, None), UNKNOWN_SESSION_NAME);
        assert_eq!(
            device_name(Some("Speakers (Realtek)"), GUID),
            "Speakers (Realtek)"
        );
        assert_eq!(device_name(Some(" "), GUID), GUID);
        assert_eq!(device_name(None, GUID), GUID);
    }

    #[test]
    fn retry_backs_off() {
        let ms = |n| retry_delay(n).as_millis();
        assert_eq!([ms(0), ms(1), ms(2), ms(3)], [500, 500, 1000, 2000]);
        assert_eq!(ms(7), 30_000);
        assert_eq!(ms(u32::MAX), 30_000);
    }

    #[test]
    fn dirty_flags() {
        let mut dirty = Dirty::default();
        assert!(dirty.is_empty());
        dirty.mark(Change::Master);
        dirty.mark(Change::Master);
        assert_eq!(
            dirty,
            Dirty {
                master: true,
                ..Dirty::default()
            }
        );
        let mut other = Dirty::default();
        for change in [
            Change::Devices,
            Change::Default,
            Change::SessionList,
            Change::SessionValues,
        ] {
            other.mark(change);
        }
        dirty.merge(other);
        assert_eq!(dirty, Dirty::all());
    }

    #[test]
    fn session_actions() {
        use SessionAction::*;
        let created = |same_control| SessionSource::Created { same_control };
        let enumerated = |exited_before, active| SessionSource::Enumerated {
            exited_before,
            active,
        };
        assert_eq!(session_action(false, created(false)), Add);
        assert_eq!(session_action(true, created(false)), Replace);
        assert_eq!(session_action(true, created(true)), Skip);
        for exited_before in [false, true] {
            for active in [false, true] {
                assert_eq!(
                    session_action(true, enumerated(exited_before, active)),
                    Skip
                );
            }
        }
        assert_eq!(session_action(false, enumerated(false, false)), Add);
        assert_eq!(session_action(false, enumerated(false, true)), Add);
        assert_eq!(session_action(false, enumerated(true, false)), Skip);
        assert_eq!(session_action(false, enumerated(true, true)), Add);
    }

    #[test]
    fn sort_like_restores_previous_order() {
        let previous: Vec<String> = ["c", "a", "b"].map(String::from).to_vec();
        let mut items = ["x", "a", "b", "y", "c"];
        sort_like(&mut items, |s| s, &previous);
        assert_eq!(items, ["c", "a", "b", "x", "y"]);

        let mut items = ["b", "a"];
        sort_like(&mut items, |s| s, &[]);
        assert_eq!(items, ["b", "a"]);
    }

    #[test]
    fn capture_endpoints_are_recognised() {
        assert!(!may_be_render_endpoint(
            "{0.0.1.00000000}.{c98e4030-926b-4e62-8c83-1c529574cc51}"
        ));
        assert!(may_be_render_endpoint(
            "{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}"
        ));
        assert!(may_be_render_endpoint("SWD\\MMDEVAPI\\x"));
        assert!(may_be_render_endpoint(""));
    }

    fn state(default: Option<Device>) -> AudioState {
        AudioState {
            devices: vec![DeviceInfo {
                id: GUID.into(),
                name: "Speakers".into(),
            }],
            default_device: default,
        }
    }

    fn speakers() -> Device {
        Device::new(GUID, "Speakers", 50.0)
            .with_session(Session::new(format!("{GUID}|a"), "A", 10.0))
            .with_session(Session::new(format!("{GUID}|b"), "B", 20.0))
    }

    #[test]
    fn diff_equal_states_is_empty() {
        let s = state(Some(speakers()));
        assert!(diff_states(&s, &s.clone()).is_empty());
        assert!(diff_states(&AudioState::default(), &AudioState::default()).is_empty());
    }

    #[test]
    fn diff_reports_master_and_session_changes() {
        let old = state(Some(speakers()));
        let mut device = speakers();
        device.muted = true;
        device.sessions[1].volume = 21.0;
        device.sessions.remove(0);
        device
            .sessions
            .push(Session::new(format!("{GUID}|c"), "C", 30.0));
        let new = state(Some(device));
        assert_eq!(
            diff_states(&old, &new),
            vec![
                AudioEvent::MasterChanged,
                AudioEvent::SessionRemoved {
                    id: format!("{GUID}|a")
                },
                AudioEvent::SessionChanged {
                    id: format!("{GUID}|b")
                },
                AudioEvent::SessionAdded {
                    id: format!("{GUID}|c")
                },
            ]
        );
    }

    #[test]
    fn diff_reports_device_and_default_changes() {
        let old = state(Some(speakers()));
        let mut new = old.clone();
        new.devices.push(DeviceInfo {
            id: "other".into(),
            name: "Other".into(),
        });
        assert_eq!(diff_states(&old, &new), vec![AudioEvent::DevicesChanged]);

        let other = Device::new("other", "Other", 50.0);
        let switched = AudioState {
            default_device: Some(other),
            ..new.clone()
        };
        assert_eq!(
            diff_states(&new, &switched),
            vec![AudioEvent::DefaultDeviceChanged]
        );
        assert_eq!(
            diff_states(&state(None), &old),
            vec![AudioEvent::DefaultDeviceChanged]
        );
        assert_eq!(
            diff_states(&old, &state(None)),
            vec![AudioEvent::DefaultDeviceChanged]
        );
    }

    #[test]
    fn diff_never_misses_a_change() {
        let old = state(Some(speakers()));
        let mut renamed = old.clone();
        renamed.default_device.as_mut().unwrap().name = "Renamed".into();
        assert_eq!(
            diff_states(&old, &renamed),
            vec![AudioEvent::DevicesChanged]
        );

        let mut reordered = old.clone();
        reordered
            .default_device
            .as_mut()
            .unwrap()
            .sessions
            .reverse();
        assert_eq!(
            diff_states(&old, &reordered),
            vec![AudioEvent::DevicesChanged]
        );
    }
}
