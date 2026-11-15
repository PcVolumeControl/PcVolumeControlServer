//! Pure mapping from PulseAudio introspection data to [`AudioState`], and the
//! diff that turns two states into [`AudioEvent`]s.
//!
//! # Sessions
//!
//! A session is a group of user streams (sink-inputs) on one sink that would
//! be displayed identically and belong to the same application instance
//! (see [`group_streams`]). Streams group when they have
//!
//! - the same sink,
//! - the same name ([`RawSinkInput::display_name`]),
//! - the same detail ([`RawSinkInput::detail`]; two streams without one
//!   count as the same), whether or not details are sent, and
//! - the same owner: the process id when known (the client's first, then
//!   the stream's), otherwise the client. A stream with neither is never
//!   grouped.
//!
//! Firefox, for one, opens a stream per audio source in a page (a video
//! element, a Web Audio context), each named after the tab, and usually only
//! one of them carries the sound.
//!
//! The session takes its id, name, detail, app hints, volume and mute from
//! its representative, the stream with the lowest index, so a group of one
//! has the id `"{sink name}|{sink-input index}"` and a group keeps its id
//! while that stream lives (normally its oldest: PulseAudio and current
//! pipewire-pulse number streams in order). An id naming any stream of a group
//! addresses the whole group, and a volume or mute that differs from the one
//! shown is set on every stream of it (see [`StreamGroup::changes`]).
//!
//! Streams joining or leaving a group change no session unless the
//! representative changes. When it goes away, the group continues under the
//! next stream's id: one diff then reports [`AudioEvent::SessionRemoved`] for
//! the old id and [`AudioEvent::SessionAdded`] for the new one, and the state
//! published with them holds the one session.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::sync::Arc;

use libpulse_binding::volume::{ChannelVolumes, Volume};
use pcvc_protocol::{clamp_volume, session_detail};

use super::conn::uniform_volume;
use crate::{AppHint, AudioEvent, AudioState, Device, DeviceInfo, Session};

/// `PA_VOLUME_NORM`, reported as 100 %.
const NORMAL: u32 = Volume::NORMAL.0;

/// Separates the sink name from the sink-input index in a session id.
const ID_SEPARATOR: char = '|';

/// Largest difference, in percent, between a requested and the current
/// volume that [`volume_matches`] still treats as no change.
const ECHO_TOLERANCE: f32 = 0.5;

/// Name used when a stream has no usable name property.
pub(super) const UNKNOWN_NAME: &str = "Unknown";

/// Stream names, compared in lower case, that libraries use when the
/// application gives none. `audiostream` and `audiocallbackdriver` are
/// Firefox's internal names. `cpal-playback-<pid>` is matched separately.
const GENERIC_MEDIA_NAMES: [&str; 7] = [
    "playback",
    "playback stream",
    "audio stream",
    "audiostream",
    "audiocallbackdriver",
    "alsa playback",
    "simple directmedia layer",
];

/// What one introspection round returned.
#[derive(Debug, Clone, Default)]
pub(super) struct RawState {
    pub default_sink: Option<CString>,
    pub sinks: Vec<RawSink>,
    pub inputs: Vec<RawSinkInput>,
    /// Application properties of each client, by client index.
    pub clients: HashMap<u32, AppProps>,
}

/// The application properties of a stream's or a client's property list.
/// Blank values are dropped.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct AppProps {
    pub name: Option<String>,
    pub process_binary: Option<String>,
    pub process_id: Option<u32>,
    pub icon_name: Option<String>,
    pub app_id: Option<String>,
}

impl AppProps {
    /// Reads the properties through `get`, which looks up one key. Values
    /// are trimmed. PipeWire spells the icon key with a hyphen, PulseAudio
    /// with an underscore. `pipewire.sec.pid` is set by the server and is a
    /// host pid even for sandboxed clients, whose own
    /// `application.process.id` is in the sandbox's pid namespace.
    pub fn read(mut get: impl FnMut(&CStr) -> Option<String>) -> Self {
        let mut first = |keys: &[&CStr]| {
            keys.iter().find_map(|key| {
                get(key)
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            })
        };
        Self {
            name: first(&[c"application.name"]),
            process_binary: first(&[c"application.process.binary"]),
            process_id: first(&[c"pipewire.sec.pid", c"application.process.id"])
                .and_then(|pid| pid.parse().ok())
                .filter(|&pid| pid != 0),
            icon_name: first(&[c"application.icon_name", c"application.icon-name"]),
            app_id: first(&[c"application.id", c"pipewire.access.portal.app_id"]),
        }
    }

    fn is_sound_server(&self) -> bool {
        const SERVERS: [&str; 3] = ["pipewire", "pipewire-pulse", "pulseaudio"];
        self.process_binary
            .as_deref()
            .is_some_and(|b| SERVERS.contains(&b))
    }
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
    pub app: AppProps,
    pub media_name: Option<String>,
    pub media_title: Option<String>,
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

    /// The stream's `application.name`, then its client's, then
    /// `media.name`, then the stream's and the client's
    /// `application.process.binary`, then [`UNKNOWN_NAME`]. Blank values are
    /// skipped. Some clients (cpal, for one) set the application properties
    /// only on the client, and `media.name` is then a generic stream label.
    /// A sound server process's own name is skipped: the streams it creates
    /// (filter chains, echo cancellers) are told apart by `media.name`.
    pub fn display_name(&self, client: Option<&AppProps>) -> String {
        let client_name = client
            .filter(|c| !c.is_sound_server())
            .and_then(|c| c.name.as_ref());
        let client_binary = client.and_then(|c| c.process_binary.as_ref());
        [
            self.app.name.as_ref(),
            client_name,
            self.media_name.as_ref(),
            self.app.process_binary.as_ref(),
            client_binary,
        ]
        .into_iter()
        .filter_map(|n| n.map(|n| n.trim()))
        .find(|n| !n.is_empty())
        .unwrap_or(UNKNOWN_NAME)
        .to_owned()
    }

    /// What tells this stream apart from others of its application: the
    /// first of `media.name` and `media.title` that is not blank, not
    /// `name` and not a generic stream name. Firefox, for one, names each
    /// stream after its tab.
    pub fn detail(&self, name: &str) -> Option<String> {
        [&self.media_name, &self.media_title]
            .into_iter()
            .flatten()
            .filter_map(|raw| session_detail(raw, name))
            .find(|detail| !is_generic_media_name(detail))
    }

    /// The stream's application properties, each missing one taken from its
    /// client.
    pub fn app_hint(&self, client: Option<&AppProps>) -> AppHint {
        let pick = |own: &Option<String>, of_client: fn(&AppProps) -> &Option<String>| {
            own.as_deref()
                .or_else(|| client.and_then(|c| of_client(c).as_deref()))
                .map(Arc::from)
        };
        AppHint {
            icon_name: pick(&self.app.icon_name, |c| &c.icon_name),
            process_binary: pick(&self.app.process_binary, |c| &c.process_binary),
            process_id: self
                .app
                .process_id
                .or_else(|| client.and_then(|c| c.process_id)),
            app_id: pick(&self.app.app_id, |c| &c.app_id),
        }
    }

    /// Percent volume; a stream without volume control reports 100.
    pub fn percent(&self) -> f32 {
        if self.has_volume && !self.volume.is_empty() {
            volume_to_percent(&self.volume)
        } else {
            100.0
        }
    }

    /// The application instance the stream belongs to, if it is known. The
    /// client's pid comes first: on PipeWire it is the server's
    /// `pipewire.sec.pid`, a host pid, where the stream's own may be one in
    /// a sandbox's pid namespace.
    fn owner(&self, client: Option<&AppProps>) -> Option<Owner> {
        client
            .and_then(|c| c.process_id)
            .or(self.app.process_id)
            .map(Owner::Process)
            .or(self.client.map(Owner::Client))
    }

    /// A volume with every channel at `value`, if the stream's volume can be
    /// set.
    fn settable_volume(&self, value: u32) -> Option<ChannelVolumes> {
        uniform_volume(self.volume.len(), value).filter(|_| self.has_volume && self.volume_writable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Owner {
    Process(u32),
    Client(u32),
}

fn client_of<'a>(
    clients: &'a HashMap<u32, AppProps>,
    input: &RawSinkInput,
) -> Option<&'a AppProps> {
    input.client.and_then(|c| clients.get(&c))
}

/// Streams shown as one session.
#[derive(Debug)]
pub(super) struct StreamGroup<'a> {
    pub name: String,
    pub detail: Option<String>,
    /// By index, so the representative is first. Never empty.
    pub members: Vec<&'a RawSinkInput>,
}

/// What setting a session changes on one of its streams.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StreamChange {
    pub index: u32,
    /// The raw value of every channel, and the volume to send.
    pub volume: Option<(u32, ChannelVolumes)>,
    pub muted: Option<bool>,
}

impl StreamGroup<'_> {
    pub fn representative(&self) -> &RawSinkInput {
        self.members[0]
    }

    /// The changes that set the group to `volume` and `muted`, leaving out
    /// streams with nothing to change.
    ///
    /// Each value is first judged against what the session shows, the
    /// representative's state, because clients echo it for sessions the user
    /// did not touch: a volume that matches it (see [`volume_matches`]), or
    /// the mute it has, changes no stream, even where other streams differ.
    /// A new value goes to every stream that does not have it already,
    /// except that a stream whose volume cannot be set keeps its volume. A
    /// new volume that no stream can take refuses the request (`None`), as
    /// for a single stream.
    pub fn changes(&self, volume: f32, muted: bool) -> Option<Vec<StreamChange>> {
        let rep = self.representative();
        let target = (!volume_matches(volume, rep.percent())).then(|| percent_to_volume(volume));
        let muted = (muted != rep.muted).then_some(muted);
        if let Some(target) = target
            && self
                .members
                .iter()
                .all(|i| i.settable_volume(target).is_none())
        {
            return None;
        }
        let changes = self
            .members
            .iter()
            .map(|input| StreamChange {
                index: input.index,
                volume: target
                    .filter(|_| !volume_matches(volume, input.percent()))
                    .and_then(|t| Some((t, input.settable_volume(t)?))),
                muted: muted.filter(|&m| m != input.muted),
            })
            .filter(|c| c.volume.is_some() || c.muted.is_some())
            .collect();
        Some(changes)
    }
}

/// Splits `streams` into the sessions they are shown as (see the module
/// docs), in the order of each group's first stream in `streams`.
pub(super) fn group_streams<'a>(
    streams: impl IntoIterator<Item = &'a RawSinkInput>,
    clients: &HashMap<u32, AppProps>,
) -> Vec<StreamGroup<'a>> {
    let mut groups: Vec<StreamGroup<'a>> = Vec::new();
    let mut by_key: HashMap<_, usize> = HashMap::new();
    for input in streams {
        let client = client_of(clients, input);
        let name = input.display_name(client);
        let detail = input.detail(&name);
        let key = input
            .owner(client)
            .map(|owner| (input.sink, name.clone(), detail.clone(), owner));
        if let Some(key) = key {
            match by_key.entry(key) {
                Entry::Occupied(at) => {
                    groups[*at.get()].members.push(input);
                    continue;
                }
                Entry::Vacant(slot) => {
                    slot.insert(groups.len());
                }
            }
        }
        groups.push(StreamGroup {
            name,
            detail,
            members: vec![input],
        });
    }
    for group in &mut groups {
        group.members.sort_by_key(|i| i.index);
    }
    groups
}

fn is_generic_media_name(name: &str) -> bool {
    let name = name.to_lowercase();
    GENERIC_MEDIA_NAMES.contains(&name.as_str())
        || name
            .strip_prefix("cpal-playback-")
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
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

    /// The sessions on `sink`.
    pub fn groups_on(&self, sink: &RawSink) -> Vec<StreamGroup<'_>> {
        group_streams(self.sessions_on(sink), &self.clients)
    }

    /// The session on the default sink with a stream that `id` names. Any
    /// of its streams will do, not only the representative, so a client
    /// holding the id of a stream that has since joined a group still
    /// reaches it.
    pub fn default_group(&self, id: &str) -> Option<StreamGroup<'_>> {
        let (sink_id, index) = parse_session_id(id)?;
        let sink = self.default_sink()?;
        if sink.id() != sink_id {
            return None;
        }
        self.groups_on(sink)
            .into_iter()
            .find(|g| g.members.iter().any(|i| i.index == index))
    }

    /// The state, with session details only if `details` is set.
    pub fn to_audio_state(&self, details: bool) -> AudioState {
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
                    .groups_on(sink)
                    .into_iter()
                    .map(|group| {
                        let rep = group.representative();
                        Session {
                            id: session_id(&id, rep.index),
                            volume: rep.percent(),
                            muted: rep.muted,
                            app: rep.app_hint(client_of(&self.clients, rep)),
                            name: group.name,
                            detail: group.detail.filter(|_| details),
                        }
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
            Some(old) => events.extend(session.change_from(old)),
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
            app: AppProps {
                name: Some(app.to_owned()),
                ..AppProps::default()
            },
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
                    app: AppProps::default(),
                    media_name: Some("Playback".into()),
                    ..input(14, 1, "")
                },
            ],
            clients: HashMap::new(),
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
        assert_eq!(i.display_name(None), UNKNOWN_NAME);
        let mut client = AppProps {
            process_binary: Some("cpal-app".into()),
            ..AppProps::default()
        };
        assert_eq!(i.display_name(Some(&client)), "cpal-app");
        i.app.process_binary = Some("mpv".into());
        assert_eq!(i.display_name(Some(&client)), "mpv");
        i.media_name = Some("  ".into());
        assert_eq!(i.display_name(None), "mpv");
        i.media_name = Some("cpal-playback-42".into());
        assert_eq!(i.display_name(Some(&client)), "cpal-playback-42");
        client.name = Some(" ".into());
        assert_eq!(i.display_name(Some(&client)), "cpal-playback-42");
        client.name = Some("flaxative".into());
        assert_eq!(i.display_name(Some(&client)), "flaxative");
        let daemon = AppProps {
            name: Some("pipewire".into()),
            process_binary: Some("pipewire".into()),
            ..AppProps::default()
        };
        assert_eq!(i.display_name(Some(&daemon)), "cpal-playback-42");
        i.app.name = Some("Firefox".into());
        assert_eq!(i.display_name(Some(&client)), "Firefox");
        i.app.name = Some("Caf\u{e9} \u{fffd}".into());
        assert_eq!(i.display_name(None), "Caf\u{e9} \u{fffd}");
    }

    #[test]
    fn detail_skips_blank_generic_and_name() {
        let firefox = |media_name: Option<&str>, media_title: Option<&str>| RawSinkInput {
            media_name: media_name.map(str::to_owned),
            media_title: media_title.map(str::to_owned),
            ..input(1, 1, "Firefox")
        };
        let detail = |i: &RawSinkInput| i.detail(&i.display_name(None));

        let tab = "The Best Reactions in Always Sunny - YouTube";
        assert_eq!(detail(&firefox(Some(tab), None)).as_deref(), Some(tab));
        assert_eq!(
            detail(&firefox(Some("Two\nlines "), None)).as_deref(),
            Some("Two lines")
        );
        assert_eq!(detail(&firefox(None, None)), None);
        assert_eq!(detail(&firefox(Some("  "), None)), None);
        assert_eq!(detail(&firefox(Some("FIREFOX"), None)), None);
        assert_eq!(
            detail(&firefox(Some("Firefox"), Some("Song - Artist"))).as_deref(),
            Some("Song - Artist")
        );
        assert_eq!(detail(&firefox(Some("AudioStream"), Some("Firefox"))), None);
        for generic in [
            "Playback",
            "playback",
            "Playback Stream",
            "AudioStream",
            "audio stream",
            "AudioCallbackDriver",
            "ALSA Playback",
            "Simple DirectMedia Layer",
            "cpal-playback-660845",
            " Audio\tStream ",
        ] {
            assert_eq!(detail(&firefox(Some(generic), None)), None, "{generic:?}");
        }
        for specific in ["cpal-playback-", "cpal-playback-x1", "Playback 2", "Audio"] {
            assert_eq!(
                detail(&firefox(Some(specific), None)).as_deref(),
                Some(specific),
                "{specific:?}"
            );
        }

        // A name taken from `media.name` is not repeated as the detail.
        let unnamed = RawSinkInput {
            app: AppProps::default(),
            media_name: Some("Lecture 3.mkv".into()),
            ..input(2, 1, "")
        };
        assert_eq!(unnamed.display_name(None), "Lecture 3.mkv");
        assert_eq!(detail(&unnamed), None);
    }

    #[test]
    fn sessions_carry_details_and_their_changes_are_reported() {
        let tab = |index: u32, title: &str| RawSinkInput {
            media_name: Some(title.to_owned()),
            ..input(index, 1, "Firefox")
        };
        let mut r = raw();
        r.inputs.push(tab(20, "Inbox - Mail"));
        r.inputs.push(tab(21, "Radio - Player"));
        let base = r.to_audio_state(true);
        let details: Vec<_> = base
            .default_device
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .map(|s| (s.name.as_str(), s.detail.as_deref()))
            .collect();
        assert_eq!(
            details,
            [
                ("Firefox", None),
                ("Playback", None),
                ("Firefox", Some("Inbox - Mail")),
                ("Firefox", Some("Radio - Player")),
            ]
        );

        // Without details nothing is filled in, so a new title changes
        // nothing.
        let plain = r.to_audio_state(false);
        let sessions = &plain.default_device.as_ref().unwrap().sessions;
        assert!(sessions.iter().all(|s| s.detail.is_none()));

        r.input_mut(20).unwrap().media_name = Some("Inbox (1) - Mail".into());
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
            vec![AudioEvent::SessionDetailChanged {
                id: "speakers|20".into()
            }]
        );
        assert!(diff(&plain, &r.to_audio_state(false)).is_empty());

        r.input_mut(20).unwrap().muted = true;
        r.input_mut(21).unwrap().media_name = Some("Radio - Next".into());
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
            vec![
                AudioEvent::SessionChanged {
                    id: "speakers|20".into()
                },
                AudioEvent::SessionDetailChanged {
                    id: "speakers|21".into()
                },
            ]
        );
    }

    fn props(pairs: &[(&str, &str)]) -> AppProps {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        AppProps::read(|key| map.get(key.to_str().unwrap()).map(|v| (*v).to_owned()))
    }

    #[test]
    fn reads_app_props() {
        assert_eq!(props(&[]), AppProps::default());
        let p = props(&[
            ("application.name", "Steam"),
            ("application.process.binary", "steam"),
            ("application.process.id", " 430542 "),
            ("application.icon-name", "steam"),
            ("application.id", "org.valvesoftware.steam"),
            ("media.name", "ignored"),
        ]);
        assert_eq!(
            p,
            AppProps {
                name: Some("Steam".into()),
                process_binary: Some("steam".into()),
                process_id: Some(430542),
                icon_name: Some("steam".into()),
                app_id: Some("org.valvesoftware.steam".into()),
            }
        );
        // The PulseAudio spelling wins; blank and unparsable values are dropped.
        let p = props(&[
            ("application.icon_name", "pulse-icon"),
            ("application.icon-name", "pw-icon"),
            ("application.name", "  "),
            ("application.process.id", "x"),
            ("pipewire.access.portal.app_id", "org.example.App"),
        ]);
        assert_eq!(p.icon_name.as_deref(), Some("pulse-icon"));
        assert_eq!((p.name, p.process_id), (None, None));
        assert_eq!(p.app_id.as_deref(), Some("org.example.App"));
        assert_eq!(props(&[("application.process.id", "0")]).process_id, None);
        let p = props(&[
            ("application.process.id", "2"),
            ("pipewire.sec.pid", "9000"),
            ("application.process.binary", " steam\n"),
        ]);
        assert_eq!(p.process_id, Some(9000));
        assert_eq!(p.process_binary.as_deref(), Some("steam"));
    }

    #[test]
    fn app_hint_falls_back_to_client() {
        let client = props(&[
            ("application.name", "flaxative"),
            ("application.process.binary", "flaxative"),
            ("application.process.id", "660845"),
            ("application.icon-name", "client-icon"),
        ]);
        let mut i = RawSinkInput::default();
        assert!(i.app_hint(None).is_empty());
        let hint = i.app_hint(Some(&client));
        assert_eq!(hint.process_binary.as_deref(), Some("flaxative"));
        assert_eq!(hint.process_id, Some(660845));
        assert_eq!(hint.icon_name.as_deref(), Some("client-icon"));
        assert_eq!(hint.app_id, None);
        i.app = props(&[
            ("application.icon_name", "own-icon"),
            ("application.process.id", "7"),
        ]);
        let hint = i.app_hint(Some(&client));
        assert_eq!(hint.icon_name.as_deref(), Some("own-icon"));
        assert_eq!(hint.process_id, Some(7));
        assert_eq!(hint.process_binary.as_deref(), Some("flaxative"));
    }

    #[test]
    fn sessions_use_client_props() {
        let mut raw = raw();
        raw.inputs.push(RawSinkInput {
            client: Some(116),
            app: AppProps::default(),
            media_name: Some("cpal-playback-660845".into()),
            ..input(15, 1, "")
        });
        raw.clients.insert(
            116,
            props(&[
                ("application.name", "flaxative"),
                ("application.process.binary", "flaxative"),
            ]),
        );
        let device = raw.to_audio_state(true).default_device.unwrap();
        let session = device
            .sessions
            .iter()
            .find(|s| s.id == "speakers|15")
            .unwrap();
        assert_eq!(session.name, "flaxative");
        assert_eq!(session.app.process_binary.as_deref(), Some("flaxative"));
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
        let state = raw().to_audio_state(true);
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
        let state = raw.to_audio_state(true);
        assert_eq!(state.devices.len(), 2);
        assert!(state.default_device.is_none());
        raw.default_sink = None;
        assert!(raw.to_audio_state(true).default_device.is_none());
        assert!(RawState::default().to_audio_state(true) == AudioState::default());
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
        let state = raw.to_audio_state(true);
        assert_eq!(state.devices.len(), 3);
        assert_eq!(state.devices[2].id, "bad\u{fffd}name");
        assert_eq!(raw.sink_by_id("bad\u{fffd}name").unwrap().index, 3);
    }

    #[test]
    fn default_group_lookup() {
        let mut raw = raw();
        raw.inputs.push(radio(31));
        raw.inputs.push(radio(30));
        let found = |id: &str| {
            raw.default_group(id)
                .map(|g| g.members.iter().map(|i| i.index).collect::<Vec<_>>())
        };
        assert_eq!(found("speakers|10"), Some(vec![10]));
        assert_eq!(found("speakers|30"), Some(vec![30, 31]));
        assert_eq!(
            found("speakers|31"),
            Some(vec![30, 31]),
            "not the representative"
        );
        assert_eq!(found("speakers|12"), None, "event stream");
        assert_eq!(found("speakers|13"), None, "module stream");
        assert_eq!(found("hdmi|11"), None, "not default");
        assert_eq!(found("hdmi|31"), None, "not default");
        assert_eq!(found("speakers|11"), None, "wrong sink");
        assert_eq!(found("speakers|99"), None);
        assert_eq!(found("nonsense"), None);
    }

    /// A Firefox stream of process 500 playing a tab titled "Radio".
    fn radio(index: u32) -> RawSinkInput {
        stream(index, 1, Some(500), Some(9), Some("Radio"))
    }

    fn stream(
        index: u32,
        sink: u32,
        pid: Option<u32>,
        client: Option<u32>,
        title: Option<&str>,
    ) -> RawSinkInput {
        RawSinkInput {
            client,
            media_name: title.map(str::to_owned),
            app: AppProps {
                name: Some("Firefox".into()),
                process_id: pid,
                ..AppProps::default()
            },
            ..input(index, sink, "Firefox")
        }
    }

    fn grouped(streams: &[RawSinkInput], clients: &HashMap<u32, AppProps>) -> Vec<Vec<u32>> {
        group_streams(streams, clients)
            .iter()
            .map(|g| g.members.iter().map(|i| i.index).collect())
            .collect()
    }

    #[test]
    fn groups_streams_that_look_the_same() {
        let none = HashMap::new();
        let group = |streams: &[RawSinkInput]| grouped(streams, &none);
        let tab = "The Best Reactions in Always Sunny - YouTube";
        let firefox = |index, title| stream(index, 1, Some(22766), Some(91), Some(title));

        // Firefox opens a stream per audio source in a page.
        let streams = [
            firefox(961, "Ukraine's Starlink Workaround - YouTube"),
            firefox(6622, tab),
            firefox(6663, tab),
        ];
        assert_eq!(group(&streams), [vec![961], vec![6622, 6663]]);
        let groups = group_streams(&streams, &none);
        assert_eq!(
            (groups[1].name.as_str(), groups[1].detail.as_deref()),
            ("Firefox", Some(tab))
        );

        // Same title, different processes.
        let other = stream(2, 1, Some(4242), Some(91), Some(tab));
        assert_eq!(group(&[firefox(1, tab), other]), [vec![1], vec![2]]);

        // Same process, different titles.
        assert_eq!(
            group(&[firefox(1, tab), firefox(2, "Radio")]),
            [vec![1], vec![2]]
        );

        // Same process, neither has a detail.
        let plain = |index, title| stream(index, 1, Some(7), Some(3), title);
        assert_eq!(
            group(&[
                plain(1, None),
                plain(2, Some("AudioStream")),
                plain(3, None)
            ]),
            [vec![1, 2, 3]]
        );
        assert_eq!(
            group(&[plain(1, None), plain(2, Some("Radio"))]),
            [vec![1], vec![2]]
        );

        // Different sinks.
        assert_eq!(
            group(&[
                firefox(1, tab),
                stream(2, 2, Some(22766), Some(91), Some(tab))
            ]),
            [vec![1], vec![2]]
        );

        // Different names.
        let mpv = RawSinkInput {
            app: AppProps {
                name: Some("mpv".into()),
                process_id: Some(22766),
                ..AppProps::default()
            },
            ..firefox(2, tab)
        };
        assert_eq!(group(&[firefox(1, tab), mpv]), [vec![1], vec![2]]);

        // Without a process id the client is the owner.
        let by_client = |index, client| stream(index, 1, None, client, Some(tab));
        assert_eq!(
            group(&[
                by_client(1, Some(5)),
                by_client(2, Some(5)),
                by_client(3, Some(6))
            ]),
            [vec![1, 2], vec![3]]
        );

        // A stream with no owner at all is never grouped.
        assert_eq!(
            group(&[by_client(1, None), by_client(2, None)]),
            [vec![1], vec![2]]
        );

        // Members are ordered by index, groups by their first stream.
        assert_eq!(
            group(&[firefox(9, tab), firefox(3, "Radio"), firefox(4, tab)]),
            [vec![4, 9], vec![3]]
        );
    }

    #[test]
    fn grouping_uses_client_props() {
        // The pid and the name may come from the client alone.
        let unnamed = |index, client| RawSinkInput {
            app: AppProps::default(),
            media_name: Some("cpal-playback-1".into()),
            ..stream(index, 1, None, Some(client), None)
        };
        let app = |pid: &str| {
            props(&[
                ("application.name", "flaxative"),
                ("application.process.id", pid),
            ])
        };
        let clients = HashMap::from([(5, app("100")), (6, app("100")), (7, app("200"))]);
        assert_eq!(
            grouped(&[unnamed(1, 5), unnamed(2, 6), unnamed(3, 7)], &clients),
            [vec![1, 2], vec![3]]
        );

        // The client's pid wins over the stream's, which a sandboxed client
        // reports from its own pid namespace.
        let clients = HashMap::from([(5, app("100")), (6, app("200"))]);
        let sandboxed = |index, client| stream(index, 1, Some(2), Some(client), Some("Radio"));
        assert_eq!(
            grouped(
                &[sandboxed(1, 5), sandboxed(2, 6), sandboxed(3, 5)],
                &clients
            ),
            [vec![1, 3], vec![2]]
        );
    }

    #[test]
    fn a_group_is_shown_as_its_representative() {
        let mut r = raw();
        r.inputs.push(RawSinkInput {
            volume: vec![NORMAL / 4; 2],
            ..radio(31)
        });
        r.inputs.push(RawSinkInput {
            muted: true,
            volume: vec![NORMAL; 2],
            app: AppProps {
                icon_name: Some("firefox".into()),
                ..radio(30).app
            },
            ..radio(30)
        });
        let sessions = r.to_audio_state(true).default_device.unwrap().sessions;
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["speakers|10", "speakers|14", "speakers|30"]);
        let session = &sessions[2];
        assert_eq!(
            (session.name.as_str(), session.detail.as_deref()),
            ("Firefox", Some("Radio"))
        );
        assert_eq!((session.volume, session.muted), (100.0, true));
        assert_eq!(session.app.icon_name.as_deref(), Some("firefox"));
        assert_eq!(session.app.process_id, Some(500));

        // Grouping does not depend on whether details are sent.
        r.inputs
            .push(stream(32, 1, Some(500), Some(9), Some("Mail")));
        let sessions = r.to_audio_state(false).default_device.unwrap().sessions;
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["speakers|10", "speakers|14", "speakers|30", "speakers|32"]
        );
        assert!(sessions.iter().all(|s| s.detail.is_none()));
    }

    #[test]
    fn setting_a_group_sets_every_stream() {
        let streams = [
            RawSinkInput {
                muted: true,
                volume: vec![NORMAL; 2],
                ..radio(30)
            },
            RawSinkInput {
                volume: vec![NORMAL / 4; 2],
                ..radio(31)
            },
            RawSinkInput {
                has_volume: false,
                volume: vec![],
                ..radio(32)
            },
            RawSinkInput {
                volume: vec![NORMAL / 2; 2],
                ..radio(33)
            },
            RawSinkInput {
                volume_writable: false,
                ..radio(34)
            },
        ];
        let groups = group_streams(&streams, &HashMap::new());
        assert_eq!(groups.len(), 1);
        let summary = |changes: Vec<StreamChange>| {
            changes
                .into_iter()
                .map(|c| (c.index, c.volume.map(|(raw, _)| raw), c.muted))
                .collect::<Vec<_>>()
        };
        // The session shows 30's state; echoing it changes nothing, although
        // the other streams differ.
        assert_eq!(summary(groups[0].changes(100.0, true).unwrap()), []);
        assert_eq!(summary(groups[0].changes(99.7, true).unwrap()), []);

        // A new volume goes to every stream that can take it and does not
        // have it: 32 and 34 cannot, 33 is within the echo tolerance.
        let target = percent_to_volume(50.3);
        assert_eq!(
            summary(groups[0].changes(50.3, true).unwrap()),
            [(30, Some(target), None), (31, Some(target), None)]
        );
        let (raw, cv) = groups[0].changes(20.0, true).unwrap()[0].volume.unwrap();
        assert_eq!(raw, percent_to_volume(20.0));
        assert_eq!(cv.get(), [Volume(raw); 2]);

        // A new mute goes to every stream without it; the shown volume is
        // left alone everywhere.
        assert_eq!(
            summary(groups[0].changes(100.0, false).unwrap()),
            [(30, None, Some(false))]
        );
        let unmuted = group_streams(&streams[1..], &HashMap::new());
        assert_eq!(
            summary(unmuted[0].changes(25.0, true).unwrap()),
            [
                (31, None, Some(true)),
                (32, None, Some(true)),
                (33, None, Some(true)),
                (34, None, Some(true)),
            ]
        );

        // Refused, mute included, when no stream can take a new volume.
        let unsettable = group_streams([&streams[2], &streams[4]], &HashMap::new());
        assert_eq!(unsettable[0].changes(10.0, true), None);
        assert_eq!(
            summary(unsettable[0].changes(100.0, true).unwrap()),
            [(32, None, Some(true)), (34, None, Some(true))]
        );
        // One that can is enough, even if it already has the volume.
        let mixed = group_streams(&streams[2..], &HashMap::new());
        assert_eq!(summary(mixed[0].changes(50.0, false).unwrap()), []);
        assert_eq!(
            summary(mixed[0].changes(10.0, false).unwrap()),
            [(33, Some(percent_to_volume(10.0)), None)]
        );
    }

    #[test]
    fn diff_follows_groups() {
        let mut r = raw();
        r.inputs.push(radio(30));
        r.inputs.push(radio(31));
        let base = r.to_audio_state(true);

        // A stream joining or leaving the group, whatever its volume.
        let mut joined = r.clone();
        joined.inputs.push(RawSinkInput {
            volume: vec![NORMAL / 8; 2],
            ..radio(32)
        });
        assert!(diff(&base, &joined.to_audio_state(true)).is_empty());
        let mut left = r.clone();
        left.inputs.retain(|i| i.index != 31);
        assert!(diff(&base, &left.to_audio_state(true)).is_empty());

        // The representative leaving: the group continues as one row under
        // the next stream's id.
        let mut moved = r.clone();
        moved.inputs.retain(|i| i.index != 30);
        let after = moved.to_audio_state(true);
        assert_eq!(
            diff(&base, &after),
            vec![
                AudioEvent::SessionRemoved {
                    id: "speakers|30".into()
                },
                AudioEvent::SessionAdded {
                    id: "speakers|31".into()
                },
            ]
        );
        let radios: Vec<&str> = after
            .default_device
            .as_ref()
            .unwrap()
            .sessions
            .iter()
            .filter(|s| s.detail.as_deref() == Some("Radio"))
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(radios, ["speakers|31"]);

        // The representative's volume is the group's.
        let mut louder = r.clone();
        louder.input_mut(31).unwrap().volume.fill(NORMAL / 8);
        assert!(diff(&base, &louder.to_audio_state(true)).is_empty());
        louder.input_mut(30).unwrap().volume.fill(NORMAL / 8);
        assert_eq!(
            diff(&base, &louder.to_audio_state(true)),
            vec![AudioEvent::SessionChanged {
                id: "speakers|30".into()
            }]
        );
    }

    #[test]
    fn diff_reports_each_kind_of_change() {
        let base = raw().to_audio_state(true);
        assert!(diff(&base, &base).is_empty());

        let mut r = raw();
        r.sinks[0].volume = vec![NORMAL / 2, NORMAL / 2];
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
            vec![AudioEvent::MasterChanged]
        );

        let mut r = raw();
        r.sinks[0].muted = true;
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
            vec![AudioEvent::MasterChanged]
        );

        let mut r = raw();
        r.inputs[0].muted = true;
        r.inputs.retain(|i| i.index != 14);
        r.inputs.push(input(20, 1, "mpv"));
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
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
            diff(&base, &r.to_audio_state(true)),
            vec![AudioEvent::DefaultDeviceChanged]
        );

        let mut r = raw();
        r.sinks[1].description = Some("HDMI".into());
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
            vec![AudioEvent::DevicesChanged]
        );

        let mut r = raw();
        r.sinks[0].description = Some("Renamed".into());
        assert_eq!(
            diff(&base, &r.to_audio_state(true)),
            vec![AudioEvent::DevicesChanged, AudioEvent::MasterChanged]
        );

        let mut r = raw();
        r.default_sink = None;
        let none = r.to_audio_state(true);
        assert_eq!(diff(&base, &none), vec![AudioEvent::DefaultDeviceChanged]);
        assert_eq!(diff(&none, &base), vec![AudioEvent::DefaultDeviceChanged]);

        // Changes outside the default device are invisible.
        let mut r = raw();
        r.inputs[1].muted = true;
        r.sinks[1].volume = vec![0, 0];
        assert!(diff(&base, &r.to_audio_state(true)).is_empty());
    }
}
