//! Pure mapping from PulseAudio introspection data to [`AudioState`], and the
//! diff that turns two states into [`AudioEvent`]s.

use std::collections::HashSet;
use std::ffi::CString;

use libpulse_binding::volume::Volume;
use pcvc_protocol::clamp_volume;

use crate::{AudioEvent, AudioState, Device, DeviceInfo, Session};

/// `PA_VOLUME_NORM`, reported as 100 %.
const NORMAL: u32 = Volume::NORMAL.0;

/// Separates the sink name from the sink-input index in a session id.
const ID_SEPARATOR: char = '|';

/// Largest difference, in percent, between a requested and the current
/// volume that [`volume_matches`] still treats as no change.
const ECHO_TOLERANCE: f32 = 0.5;

/// Name used when a stream has no usable name property.
pub(super) const UNKNOWN_NAME: &str = "Unknown";

/// What one introspection round returned.
#[derive(Debug, Clone, Default)]
pub(super) struct RawState {
    pub default_sink: Option<CString>,
    pub sinks: Vec<RawSink>,
    pub inputs: Vec<RawSinkInput>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RawSink {
    pub index: u32,
    /// The sink name as the server sent it, for addressing the sink.
    pub name: CString,
    pub description: Option<String>,
    pub volume: Vec<u32>,
    pub muted: bool,
}

impl RawSink {
    /// The protocol device id: the sink name, lossily decoded.
    pub fn id(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct RawSinkInput {
    pub index: u32,
    pub sink: u32,
    /// `None` for streams created by a server module rather than a client.
    pub client: Option<u32>,
    pub volume: Vec<u32>,
    pub has_volume: bool,
    pub volume_writable: bool,
    pub muted: bool,
    pub application_name: Option<String>,
    pub media_name: Option<String>,
    pub process_binary: Option<String>,
    pub media_role: Option<String>,
}

impl RawSinkInput {
    /// Whether the stream is shown as a session.
    ///
    /// Excluded: event sounds (`media.role` "event", the counterpart of the
    /// Windows system-sounds session) and streams with no client, which are
    /// created by server modules (loopbacks, combine and tunnel sinks) to
    /// route audio between devices rather than by applications. Corked
    /// (paused) streams are kept, like inactive sessions on Windows.
    pub fn is_user_stream(&self) -> bool {
        self.client.is_some() && self.media_role.as_deref() != Some("event")
    }

    /// `application.name`, then `media.name`, then
    /// `application.process.binary`, then [`UNKNOWN_NAME`]. Blank values are
    /// skipped.
    pub fn display_name(&self) -> String {
        [
            &self.application_name,
            &self.media_name,
            &self.process_binary,
        ]
        .into_iter()
        .filter_map(|n| n.as_deref().map(str::trim))
        .find(|n| !n.is_empty())
        .unwrap_or(UNKNOWN_NAME)
        .to_owned()
    }

    /// Percent volume; a stream without volume control reports 100.
    pub fn percent(&self) -> f32 {
        if self.has_volume && !self.volume.is_empty() {
            volume_to_percent(&self.volume)
        } else {
            100.0
        }
    }
}

/// Average of the channel volumes as a percentage of `PA_VOLUME_NORM`,
/// clamped to 0..=100. No channels reads as 0.
pub(super) fn volume_to_percent(channels: &[u32]) -> f32 {
    if channels.is_empty() {
        return 0.0;
    }
    let sum: u64 = channels.iter().map(|&v| u64::from(v)).sum();
    let average = sum as f64 / channels.len() as f64;
    clamp_volume((average * 100.0 / f64::from(NORMAL)) as f32)
}

/// The raw volume for a percentage, clamped to 0..=100 (so never above
/// `PA_VOLUME_NORM`).
pub(super) fn percent_to_volume(percent: f32) -> u32 {
    let percent = f64::from(clamp_volume(percent));
    (percent * f64::from(NORMAL) / 100.0).round() as u32
}

/// Whether a requested volume is close enough to the current one (in
/// percent) to count as unchanged. Clients echo the levels they were sent,
/// possibly rounded, for sessions the user did not touch.
pub(super) fn volume_matches(requested: f32, current: f32) -> bool {
    (clamp_volume(requested) - current).abs() < ECHO_TOLERANCE
}

pub(super) fn session_id(sink_name: &str, input_index: u32) -> String {
    format!("{sink_name}{ID_SEPARATOR}{input_index}")
}

/// Splits a session id into sink name and sink-input index. The index is
/// after the last separator, so sink names containing it still parse.
pub(super) fn parse_session_id(id: &str) -> Option<(&str, u32)> {
    let (sink, index) = id.rsplit_once(ID_SEPARATOR)?;
    if sink.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((sink, index.parse().ok()?))
}

impl RawState {
    /// Sinks with a unique id, in server order. A sink whose lossily decoded
    /// name collides with an earlier one is dropped, since it could not be
    /// told apart by id.
    pub fn addressable_sinks(&self) -> impl Iterator<Item = &RawSink> {
        let mut seen = HashSet::new();
        self.sinks.iter().filter(move |s| seen.insert(s.id()))
    }

    /// The default sink, if the server has one and it is addressable.
    pub fn default_sink(&self) -> Option<&RawSink> {
        let name = self.default_sink.as_ref()?;
        self.addressable_sinks().find(|s| &s.name == name)
    }

    pub fn sink_by_id(&self, id: &str) -> Option<&RawSink> {
        self.addressable_sinks().find(|s| s.id() == id)
    }

    pub fn sink_mut(&mut self, index: u32) -> Option<&mut RawSink> {
        self.sinks.iter_mut().find(|s| s.index == index)
    }

    pub fn input_mut(&mut self, index: u32) -> Option<&mut RawSinkInput> {
        self.inputs.iter_mut().find(|i| i.index == index)
    }

    /// User streams playing to `sink`, in server order.
    pub fn sessions_on(&self, sink: &RawSink) -> impl Iterator<Item = &RawSinkInput> {
        let index = sink.index;
        self.inputs
            .iter()
            .filter(move |i| i.sink == index && i.is_user_stream())
    }

    /// The session `id` refers to, if it is currently a session on the
    /// default sink.
    pub fn default_session(&self, id: &str) -> Option<(&RawSink, &RawSinkInput)> {
        let (sink_id, index) = parse_session_id(id)?;
        let sink = self.default_sink()?;
        if sink.id() != sink_id {
            return None;
        }
        let input = self.sessions_on(sink).find(|i| i.index == index)?;
        Some((sink, input))
    }

    pub fn to_audio_state(&self) -> AudioState {
        let devices = self
            .addressable_sinks()
            .map(|s| DeviceInfo {
                id: s.id(),
                name: sink_display_name(s),
            })
            .collect();
        let default_device = self.default_sink().map(|sink| {
            let id = sink.id();
            Device {
                name: sink_display_name(sink),
                volume: volume_to_percent(&sink.volume),
                muted: sink.muted,
                sessions: self
                    .sessions_on(sink)
                    .map(|i| Session {
                        id: session_id(&id, i.index),
                        name: i.display_name(),
                        volume: i.percent(),
                        muted: i.muted,
                    })
                    .collect(),
                id,
            }
        });
        AudioState {
            devices,
            default_device,
        }
    }
}

/// The sink description, falling back to its name.
fn sink_display_name(sink: &RawSink) -> String {
    sink.description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map_or_else(|| sink.id(), str::to_owned)
}

/// The events that describe the change from `old` to `new`; empty when they
/// are equal. Session events are only produced while the default device
/// stays the same, since a new default device replaces the whole session
/// list.
pub(super) fn diff(old: &AudioState, new: &AudioState) -> Vec<AudioEvent> {
    let mut events = Vec::new();
    if old.devices != new.devices {
        events.push(AudioEvent::DevicesChanged);
    }
    let (old_dev, new_dev) = match (&old.default_device, &new.default_device) {
        (None, None) => return events,
        (Some(o), Some(n)) if o.id == n.id => (o, n),
        _ => {
            events.push(AudioEvent::DefaultDeviceChanged);
            return events;
        }
    };
    if (old_dev.name.as_str(), old_dev.volume, old_dev.muted)
        != (new_dev.name.as_str(), new_dev.volume, new_dev.muted)
    {
        events.push(AudioEvent::MasterChanged);
    }
    for session in &old_dev.sessions {
        if !new_dev.sessions.iter().any(|s| s.id == session.id) {
            events.push(AudioEvent::SessionRemoved {
                id: session.id.clone(),
            });
        }
    }
    for session in &new_dev.sessions {
        match old_dev.sessions.iter().find(|s| s.id == session.id) {
            None => events.push(AudioEvent::SessionAdded {
                id: session.id.clone(),
            }),
            Some(old) if old != session => events.push(AudioEvent::SessionChanged {
                id: session.id.clone(),
            }),
            Some(_) => {}
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(index: u32, name: &str, description: Option<&str>, volume: u32) -> RawSink {
        RawSink {
            index,
            name: CString::new(name).unwrap(),
            description: description.map(str::to_owned),
            volume: vec![volume, volume],
            muted: false,
        }
    }

    fn input(index: u32, sink: u32, app: &str) -> RawSinkInput {
        RawSinkInput {
            index,
            sink,
            client: Some(7),
            volume: vec![NORMAL / 2, NORMAL / 2],
            has_volume: true,
            volume_writable: true,
            muted: false,
            application_name: Some(app.to_owned()),
            ..Default::default()
        }
    }

    fn raw() -> RawState {
        RawState {
            default_sink: Some(CString::new("speakers").unwrap()),
            sinks: vec![
                sink(1, "speakers", Some("Speakers"), NORMAL),
                sink(2, "hdmi", Some("  "), NORMAL / 4),
            ],
            inputs: vec![
                input(10, 1, "Firefox"),
                input(11, 2, "Steam"),
                RawSinkInput {
                    media_role: Some("event".into()),
                    ..input(12, 1, "bell")
                },
                RawSinkInput {
                    client: None,
                    ..input(13, 1, "loopback")
                },
                RawSinkInput {
                    muted: true,
                    has_volume: false,
                    volume: vec![],
                    application_name: None,
                    media_name: Some("Playback".into()),
                    ..input(14, 1, "")
                },
            ],
        }
    }

    #[test]
    fn volume_to_percent_averages_and_clamps() {
        assert_eq!(volume_to_percent(&[NORMAL]), 100.0);
        assert_eq!(volume_to_percent(&[NORMAL, 0]), 50.0);
        assert_eq!(volume_to_percent(&[NORMAL / 4; 6]), 25.0);
        assert_eq!(volume_to_percent(&[NORMAL * 3 / 2, NORMAL * 3 / 2]), 100.0);
        assert_eq!(volume_to_percent(&[u32::MAX; 32]), 100.0);
        assert_eq!(volume_to_percent(&[]), 0.0);
        assert_eq!(volume_to_percent(&[0]), 0.0);
    }

    #[test]
    fn percent_to_volume_clamps_and_rounds() {
        assert_eq!(percent_to_volume(100.0), NORMAL);
        assert_eq!(percent_to_volume(250.0), NORMAL);
        assert_eq!(percent_to_volume(0.0), 0);
        assert_eq!(percent_to_volume(-3.0), 0);
        assert_eq!(percent_to_volume(f32::NAN), 0);
        assert_eq!(percent_to_volume(50.0), NORMAL / 2);
    }

    #[test]
    fn volume_round_trips() {
        for raw in [0, 1, 655, 12345, NORMAL / 3, NORMAL - 1, NORMAL] {
            assert_eq!(percent_to_volume(volume_to_percent(&[raw])), raw);
        }
        for percent in [0.0, 1.0, 33.0, 64.0, 99.5, 100.0] {
            let back = volume_to_percent(&[percent_to_volume(percent)]);
            assert!((back - percent).abs() < 0.001, "{percent} -> {back}");
        }
    }

    #[test]
    fn volume_matches_tolerates_echoes() {
        assert!(volume_matches(75.0, 75.3));
        assert!(volume_matches(49.99, 50.0));
        assert!(volume_matches(150.0, 100.0));
        assert!(!volume_matches(74.0, 75.0));
        assert!(!volume_matches(f32::NAN, 1.0));
        assert!(volume_matches(f32::NAN, 0.0));
    }

    #[test]
    fn session_id_format_and_parse() {
        let id = session_id("alsa_output.pci-0000_00_1f.3.analog-stereo", 42);
        assert_eq!(id, "alsa_output.pci-0000_00_1f.3.analog-stereo|42");
        assert_eq!(
            parse_session_id(&id),
            Some(("alsa_output.pci-0000_00_1f.3.analog-stereo", 42))
        );
        assert_eq!(parse_session_id("a|b|7"), Some(("a|b", 7)));
        for bad in [
            "",
            "|",
            "sink",
            "sink|",
            "|3",
            "sink|x",
            "sink|-1",
            "sink|+1",
            "sink|4294967296",
        ] {
            assert_eq!(parse_session_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn display_name_falls_back() {
        let mut i = RawSinkInput::default();
        assert_eq!(i.display_name(), UNKNOWN_NAME);
        i.process_binary = Some("mpv".into());
        assert_eq!(i.display_name(), "mpv");
        i.media_name = Some("  ".into());
        assert_eq!(i.display_name(), "mpv");
        i.media_name = Some("Video".into());
        assert_eq!(i.display_name(), "Video");
        i.application_name = Some("Firefox".into());
        assert_eq!(i.display_name(), "Firefox");
        i.application_name = Some("Caf\u{e9} \u{fffd}".into());
        assert_eq!(i.display_name(), "Caf\u{e9} \u{fffd}");
    }

    #[test]
    fn filters_event_and_module_streams() {
        let raw = raw();
        let speakers = raw.default_sink().unwrap();
        let shown: Vec<u32> = raw.sessions_on(speakers).map(|i| i.index).collect();
        assert_eq!(shown, vec![10, 14]);
        let corked_role = RawSinkInput {
            media_role: Some("music".into()),
            ..input(1, 1, "a")
        };
        assert!(corked_role.is_user_stream());
    }

    #[test]
    fn maps_to_audio_state() {
        let state = raw().to_audio_state();
        assert_eq!(
            state.devices,
            vec![
                DeviceInfo {
                    id: "speakers".into(),
                    name: "Speakers".into()
                },
                DeviceInfo {
                    id: "hdmi".into(),
                    name: "hdmi".into()
                },
            ]
        );
        let d = state.default_device.unwrap();
        assert_eq!(
            (d.id.as_str(), d.volume, d.muted),
            ("speakers", 100.0, false)
        );
        assert_eq!(
            d.sessions,
            vec![
                Session::new("speakers|10", "Firefox", 50.0),
                Session {
                    muted: true,
                    ..Session::new("speakers|14", "Playback", 100.0)
                },
            ]
        );
        for s in &d.sessions {
            assert!(s.id.contains(&d.id));
        }
    }

    #[test]
    fn missing_or_unknown_default_sink() {
        let mut raw = raw();
        raw.default_sink = Some(CString::new("gone").unwrap());
        let state = raw.to_audio_state();
        assert_eq!(state.devices.len(), 2);
        assert!(state.default_device.is_none());
        raw.default_sink = None;
        assert!(raw.to_audio_state().default_device.is_none());
        assert!(RawState::default().to_audio_state() == AudioState::default());
    }

    #[test]
    fn non_utf8_names_are_lossy_and_unique() {
        let mut raw = raw();
        raw.sinks.push(RawSink {
            name: CString::new(b"bad\xffname".to_vec()).unwrap(),
            ..sink(3, "", None, 0)
        });
        raw.sinks.push(RawSink {
            name: CString::new(b"bad\xfename".to_vec()).unwrap(),
            ..sink(4, "", Some("Twin"), 0)
        });
        let state = raw.to_audio_state();
        assert_eq!(state.devices.len(), 3);
        assert_eq!(state.devices[2].id, "bad\u{fffd}name");
        assert_eq!(raw.sink_by_id("bad\u{fffd}name").unwrap().index, 3);
    }

    #[test]
    fn default_session_lookup() {
        let raw = raw();
        assert_eq!(raw.default_session("speakers|10").unwrap().1.index, 10);
        assert!(raw.default_session("speakers|12").is_none(), "event stream");
        assert!(
            raw.default_session("speakers|13").is_none(),
            "module stream"
        );
        assert!(raw.default_session("hdmi|11").is_none(), "not default");
        assert!(raw.default_session("speakers|11").is_none(), "wrong sink");
        assert!(raw.default_session("speakers|99").is_none());
        assert!(raw.default_session("nonsense").is_none());
    }

    #[test]
    fn diff_reports_each_kind_of_change() {
        let base = raw().to_audio_state();
        assert!(diff(&base, &base).is_empty());

        let mut r = raw();
        r.sinks[0].volume = vec![NORMAL / 2, NORMAL / 2];
        assert_eq!(
            diff(&base, &r.to_audio_state()),
            vec![AudioEvent::MasterChanged]
        );

        let mut r = raw();
        r.sinks[0].muted = true;
        assert_eq!(
            diff(&base, &r.to_audio_state()),
            vec![AudioEvent::MasterChanged]
        );

        let mut r = raw();
        r.inputs[0].muted = true;
        r.inputs.retain(|i| i.index != 14);
        r.inputs.push(input(20, 1, "mpv"));
        assert_eq!(
            diff(&base, &r.to_audio_state()),
            vec![
                AudioEvent::SessionRemoved {
                    id: "speakers|14".into()
                },
                AudioEvent::SessionChanged {
                    id: "speakers|10".into()
                },
                AudioEvent::SessionAdded {
                    id: "speakers|20".into()
                },
            ]
        );

        let mut r = raw();
        r.default_sink = Some(CString::new("hdmi").unwrap());
        assert_eq!(
            diff(&base, &r.to_audio_state()),
            vec![AudioEvent::DefaultDeviceChanged]
        );

        let mut r = raw();
        r.sinks[1].description = Some("HDMI".into());
        assert_eq!(
            diff(&base, &r.to_audio_state()),
            vec![AudioEvent::DevicesChanged]
        );

        let mut r = raw();
        r.sinks[0].description = Some("Renamed".into());
        assert_eq!(
            diff(&base, &r.to_audio_state()),
            vec![AudioEvent::DevicesChanged, AudioEvent::MasterChanged]
        );

        let mut r = raw();
        r.default_sink = None;
        let none = r.to_audio_state();
        assert_eq!(diff(&base, &none), vec![AudioEvent::DefaultDeviceChanged]);
        assert_eq!(diff(&none, &base), vec![AudioEvent::DefaultDeviceChanged]);

        // Changes outside the default device are invisible.
        let mut r = raw();
        r.inputs[1].muted = true;
        r.sinks[1].volume = vec![0, 0];
        assert!(diff(&base, &r.to_audio_state()).is_empty());
    }
}
