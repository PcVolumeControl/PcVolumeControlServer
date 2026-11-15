use std::collections::BTreeMap;

use pcvc_protocol::{
    ClientUpdate, Command, DeviceState, FullState, MAX_LINE_LEN, PROTOCOL_VERSION, ProtocolError,
    SessionState, clamp_volume, encode_full_state, parse_client_line, plan_commands,
};
use serde_json::Value;

const README_FULL_STATE: &str = include_str!("fixtures/readme_full_state.json");

const SPEAKERS: &str = "0f4090a9-dee2-4563-ba29-0ad6b93d9e22";
const HEADSET: &str = "c98e4030-926b-4e62-8c83-1c529574cc51";
const CHROME: &str = "{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}|\\Device\\HarddiskVolume2\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe%b{00000000-0000-0000-0000-000000000000}";
const STEAM: &str = "{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}|\\Device\\HarddiskVolume2\\Program Files (x86)\\Steam\\Steam.exe%b{00000000-0000-0000-0000-000000000000}";

fn readme_state() -> FullState {
    serde_json::from_str(README_FULL_STATE).unwrap()
}

fn parse(line: &str) -> ClientUpdate {
    parse_client_line(line).unwrap().unwrap()
}

#[test]
fn readme_full_state_round_trips() {
    let state = readme_state();
    assert_eq!(state.protocol_version, 7);
    assert_eq!(state.device_ids.len(), 3);
    assert_eq!(state.default_device.device_id, SPEAKERS);
    assert_eq!(state.default_device.master_volume, 80.0);
    assert!(!state.default_device.master_muted);
    assert_eq!(state.default_device.sessions.len(), 4);
    assert_eq!(state.default_device.sessions[3].name, "Google Chrome");
    assert_eq!(state.default_device.sessions[3].id, CHROME);

    let line = encode_full_state(&state);
    assert!(line.ends_with('\n'));
    assert_eq!(line.matches('\n').count(), 1, "one state per line");

    let original: Value = serde_json::from_str(README_FULL_STATE).unwrap();
    let encoded: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(original, encoded);
}

#[test]
fn full_state_ignores_unknown_fields() {
    let mut value: Value = serde_json::from_str(README_FULL_STATE).unwrap();
    value["futureField"] = Value::Bool(true);
    value["defaultDevice"]["sessions"][0]["icon"] = Value::String("x".into());
    let state: FullState = serde_json::from_value(value).unwrap();
    assert_eq!(state, readme_state());
}

#[test]
fn full_state_encodes_non_ascii_as_utf8() {
    let mut state = readme_state();
    state.default_device.sessions[0].name = "Musique \u{e9}t\u{e9} \u{1f3b5}".into();
    let line = encode_full_state(&state);
    assert!(line.contains("Musique \u{e9}t\u{e9} \u{1f3b5}"));
}

// Client messages exactly as the shipped clients serialise them.

// Swift and Kotlin emit byte-identical default device updates.
#[test]
fn parses_default_device_update() {
    let u = parse(&format!(
        r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#
    ));
    let d = u.default_device.unwrap();
    assert_eq!(d.device_id.as_deref(), Some(HEADSET));
    assert_eq!(d.master_volume, None);
    assert_eq!(d.master_muted, None);
    assert_eq!(d.sessions, None);
}

#[test]
fn parses_swift_master_update_with_both_fields() {
    let u = parse(&format!(
        r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterMuted":false,"masterVolume":42.5}}}}"#
    ));
    let d = u.default_device.unwrap();
    assert_eq!(d.master_muted, Some(false));
    assert_eq!(d.master_volume, Some(42.5));
}

#[test]
fn parses_kotlin_master_update() {
    let u = parse(&format!(
        r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterVolume":42.0,"masterMuted":true}}}}"#
    ));
    let d = u.default_device.unwrap();
    assert_eq!(d.master_muted, Some(true));
    assert_eq!(d.master_volume, Some(42.0));
}

#[test]
fn parses_swift_session_update_with_name_and_escaped_slashes() {
    // Swift's JSONEncoder writes integral doubles without a fraction and
    // escapes '/' as "\/".
    let u = parse(
        r#"{"protocolVersion":7,"defaultDevice":{"sessions":[{"name":"Google Chrome","id":"a\/b","volume":29,"muted":true}],"deviceId":"x"}}"#,
    );
    let d = u.default_device.unwrap();
    let s = &d.sessions.unwrap()[0];
    assert_eq!(s.name.as_deref(), Some("Google Chrome"));
    assert_eq!(s.id, "a/b");
    assert_eq!(s.volume, 29.0);
    assert!(s.muted);
}

#[test]
fn parses_kotlin_session_update_without_name() {
    // Gson omits nulls and HTML-escapes '=' as a unicode escape sequence.
    let escaped_eq = format!("{}u003d", '\\');
    let u = parse(&format!(
        r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"x","sessions":[{{"id":"a{escaped_eq}b","volume":29.5,"muted":false}}]}}}}"#
    ));
    let s = &u.default_device.unwrap().sessions.unwrap()[0];
    assert_eq!(s.name, None);
    assert_eq!(s.id, "a=b");
    assert_eq!(s.volume, 29.5);
}

#[test]
fn accepts_crlf_and_ignores_unknown_fields() {
    let u = parse(
        "{\"protocolVersion\":7,\"applicationVersion\":\"x\",\"extra\":1,\"defaultDevice\":{\"deviceId\":\"x\",\"what\":[]}}\r\n",
    );
    assert_eq!(u.default_device.unwrap().device_id.as_deref(), Some("x"));
}

#[test]
fn blank_line_is_not_a_message() {
    assert!(parse_client_line("").unwrap().is_none());
    assert!(parse_client_line("  \r\n").unwrap().is_none());
}

#[test]
fn rejects_wrong_version() {
    let err =
        parse_client_line(r#"{"protocolVersion":6,"defaultDevice":{"deviceId":"x"}}"#).unwrap_err();
    match err {
        ProtocolError::VersionMismatch { received: Some(v) } => assert_eq!(v.as_i64(), Some(6)),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn rejects_fractional_version() {
    let err = parse_client_line(r#"{"protocolVersion":7.5}"#).unwrap_err();
    assert!(matches!(
        err,
        ProtocolError::VersionMismatch { received: Some(_) }
    ));
}

#[test]
fn rejects_stale_version_field() {
    let err = parse_client_line(r#"{"defaultDevice":{"deviceId":"x"},"version":7}"#).unwrap_err();
    assert!(matches!(
        err,
        ProtocolError::VersionMismatch { received: None }
    ));
}

#[test]
fn rejects_malformed() {
    for line in [
        "garbage",
        "{",
        "[]",
        "[7]",
        // Positional (array) encodings of nested structs.
        r#"{"protocolVersion":7,"defaultDevice":["x",null,50.0,true,null]}"#,
        r#"{"protocolVersion":7,"defaultDevice":{"deviceId":"x","sessions":[[null,"a",5,true]]}}"#,
        "null",
        r#"{"protocolVersion":"7"}"#,
        // Session entries require both volume and muted.
        r#"{"protocolVersion":7,"defaultDevice":{"deviceId":"x","sessions":[{"id":"a","volume":1}]}}"#,
        r#"{"protocolVersion":7,"defaultDevice":{"deviceId":"x","sessions":[{"id":"a","muted":true}]}}"#,
    ] {
        assert!(
            matches!(parse_client_line(line), Err(ProtocolError::Malformed(_))),
            "{line}"
        );
    }
}

#[test]
fn accepts_integral_float_version() {
    let u = parse(r#"{"protocolVersion":7.0,"defaultDevice":{"deviceId":"x"}}"#);
    assert!(u.default_device.is_some());
}

#[test]
fn null_default_device_is_accepted() {
    let u = parse(r#"{"protocolVersion":7,"defaultDevice":null}"#);
    assert!(u.default_device.is_none());
}

#[test]
fn rejects_overlong_line() {
    let line = format!(
        r#"{{"protocolVersion":7,"applicationVersion":"{}"}}"#,
        "x".repeat(MAX_LINE_LEN)
    );
    assert!(matches!(
        parse_client_line(&line),
        Err(ProtocolError::LineTooLong { .. })
    ));
}

#[test]
fn version_constant_matches_wire() {
    assert_eq!(PROTOCOL_VERSION, 7);
}

// Precedence.

fn current() -> FullState {
    FullState {
        protocol_version: PROTOCOL_VERSION,
        application_version: "test".into(),
        device_ids: BTreeMap::from([
            (SPEAKERS.into(), "Speakers".into()),
            (HEADSET.into(), "Headset".into()),
        ]),
        default_device: DeviceState {
            device_id: SPEAKERS.into(),
            name: "Speakers".into(),
            master_volume: 50.0,
            master_muted: false,
            sessions: vec![
                SessionState {
                    name: "Steam".into(),
                    id: STEAM.into(),
                    volume: 80.0,
                    muted: false,
                },
                SessionState {
                    name: "Chrome".into(),
                    id: CHROME.into(),
                    volume: 35.0,
                    muted: false,
                },
            ],
        },
    }
}

fn update(json: Value) -> ClientUpdate {
    serde_json::from_value(json).unwrap()
}

#[test]
fn no_default_device_is_noop() {
    let u = update(serde_json::json!({ "protocolVersion": 7 }));
    assert!(plan_commands(&current(), &u).is_empty());
}

#[test]
fn missing_device_id_is_noop() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": { "masterVolume": 10.0 }
    }));
    assert!(plan_commands(&current(), &u).is_empty());
}

#[test]
fn device_switch_wins_over_everything_else() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": HEADSET,
            "masterVolume": 10.0,
            "masterMuted": true,
            "sessions": [{ "id": CHROME, "volume": 1.0, "muted": true }]
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetDefaultDevice {
            device_id: HEADSET.into()
        }]
    );
}

#[test]
fn switch_to_unknown_device_is_left_to_the_backend() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": "nope",
            "sessions": [{ "id": CHROME, "volume": 1.0, "muted": true }]
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetDefaultDevice {
            device_id: "nope".into()
        }]
    );
}

#[test]
fn master_wins_over_sessions() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": SPEAKERS,
            "masterVolume": 10.0,
            "sessions": [{ "id": CHROME, "volume": 1.0, "muted": true }]
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetMaster {
            volume: Some(10.0),
            muted: None
        }]
    );
}

#[test]
fn master_mute_only() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": { "deviceId": SPEAKERS, "masterMuted": true }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetMaster {
            volume: None,
            muted: Some(true)
        }]
    );
}

#[test]
fn swift_master_update_sets_both() {
    let u = parse(&format!(
        r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterMuted":true,"masterVolume":64}}}}"#
    ));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetMaster {
            volume: Some(64.0),
            muted: Some(true)
        }]
    );
}

#[test]
fn sessions_apply_volume_and_mute_for_live_ids_only() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": SPEAKERS,
            "sessions": [
                { "id": "gone", "volume": 5.0, "muted": true },
                { "name": "Chrome", "id": CHROME, "volume": 29.0, "muted": true },
                { "id": STEAM, "volume": 12.0, "muted": false }
            ]
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![
            Command::SetSession {
                session_id: STEAM.into(),
                volume: 12.0,
                muted: false
            },
            Command::SetSession {
                session_id: CHROME.into(),
                volume: 29.0,
                muted: true
            },
        ]
    );
}

#[test]
fn duplicate_session_entries_use_the_first() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": SPEAKERS,
            "sessions": [
                { "id": CHROME, "volume": 1.0, "muted": false },
                { "id": CHROME, "volume": 2.0, "muted": true }
            ]
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetSession {
            session_id: CHROME.into(),
            volume: 1.0,
            muted: false
        }]
    );
}

#[test]
fn same_device_without_changes_is_noop() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": { "deviceId": SPEAKERS, "sessions": [] }
    }));
    assert!(plan_commands(&current(), &u).is_empty());
}

// Clamping.

#[test]
fn clamps_volumes() {
    assert_eq!(clamp_volume(-5.0), 0.0);
    assert_eq!(clamp_volume(150.0), 100.0);
    assert_eq!(clamp_volume(f32::INFINITY), 100.0);
    assert_eq!(clamp_volume(f32::NEG_INFINITY), 0.0);
    assert_eq!(clamp_volume(f32::NAN), 0.0);
    assert_eq!(clamp_volume(42.5), 42.5);
}

#[test]
fn planned_volumes_are_clamped() {
    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": SPEAKERS,
            "masterVolume": 250.0
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetMaster {
            volume: Some(100.0),
            muted: None
        }]
    );

    let u = update(serde_json::json!({
        "protocolVersion": 7,
        "defaultDevice": {
            "deviceId": SPEAKERS,
            "sessions": [{ "id": CHROME, "volume": -3.0, "muted": false }]
        }
    }));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetSession {
            session_id: CHROME.into(),
            volume: 0.0,
            muted: false
        }]
    );
}

#[test]
fn huge_volume_parses_and_clamps() {
    let u = parse(&format!(
        r#"{{"protocolVersion":7,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterVolume":1e300}}}}"#
    ));
    assert_eq!(
        plan_commands(&current(), &u),
        vec![Command::SetMaster {
            volume: Some(100.0),
            muted: None
        }]
    );
}
