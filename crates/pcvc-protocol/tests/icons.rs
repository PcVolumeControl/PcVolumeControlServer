use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use pcvc_protocol::{
    ClientUpdate, Command, DeviceState, FullState, ICON_SIZE_DEFAULT, ICON_SIZE_MAX, ICON_SIZE_MIN,
    IconId, IconMessage, IconReply, MAX_ICON_IDS_PER_REQUEST, MAX_ICON_PNG_BYTES, PROTOCOL_VERSION,
    ProtocolError, SessionState, encode_full_state, encode_icon_reply, parse_client_line,
    plan_commands,
};
use serde_json::{Value, json};

const README_FULL_STATE: &str = include_str!("fixtures/readme_full_state.json");
const SPEAKERS: &str = "0f4090a9-dee2-4563-ba29-0ad6b93d9e22";
const HEADSET: &str = "c98e4030-926b-4e62-8c83-1c529574cc51";

fn parse(line: &str) -> ClientUpdate {
    parse_client_line(line).unwrap().unwrap()
}

fn icon_request_line(request: Value) -> String {
    json!({ "protocolVersion": 8, "iconRequest": request }).to_string()
}

/// Updates as the Swift and Kotlin clients write them.
fn client_update_lines() -> Vec<String> {
    let escaped_eq = format!("{}u003d", '\\');
    vec![
        format!(r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{HEADSET}"}}}}"#),
        format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterMuted":false,"masterVolume":42.5}}}}"#
        ),
        format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterVolume":42.0,"masterMuted":true}}}}"#
        ),
        r#"{"protocolVersion":8,"defaultDevice":{"sessions":[{"name":"Google Chrome","id":"a\/b","volume":29,"muted":true}],"deviceId":"x"}}"#.to_owned(),
        format!(
            r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"x","sessions":[{{"id":"a{escaped_eq}b","volume":29.5,"muted":false}}]}}}}"#
        ),
    ]
}

fn current() -> FullState {
    FullState {
        protocol_version: PROTOCOL_VERSION,
        application_version: "test".into(),
        device_ids: BTreeMap::from([(SPEAKERS.into(), "Speakers".into())]),
        default_device: DeviceState {
            device_id: SPEAKERS.into(),
            name: "Speakers".into(),
            master_volume: 50.0,
            master_muted: false,
            sessions: vec![SessionState {
                name: "Chrome".into(),
                id: "chrome".into(),
                volume: 35.0,
                muted: false,
                icon_id: None,
                detail: None,
            }],
        },
    }
}

// Ids.

#[test]
fn icon_id_accepts_lowercase_alphanumerics_up_to_64() {
    for id in ["a", "0", "abc123", &"f".repeat(64)] {
        assert_eq!(IconId::new(id).unwrap().as_str(), id);
    }
}

#[test]
fn icon_id_rejects_everything_else() {
    for id in [
        "",
        &"f".repeat(65),
        "ABC",
        "a-b",
        "a b",
        "a_b",
        "a.png",
        "\u{e9}",
        "a\n",
    ] {
        assert!(IconId::new(id).is_err(), "{id:?}");
    }
}

#[test]
fn icon_id_serde_validates() {
    let id: IconId = serde_json::from_str(r#""abc""#).unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), r#""abc""#);
    assert!(serde_json::from_str::<IconId>(r#""A""#).is_err());
}

// Full state.

#[test]
fn icon_id_is_omitted_when_absent() {
    let state: FullState = serde_json::from_str(README_FULL_STATE).unwrap();
    assert!(
        state
            .default_device
            .sessions
            .iter()
            .all(|s| s.icon_id.is_none())
    );
    let line = encode_full_state(&state);
    assert!(!line.contains("iconId"), "{line}");
    assert!(!line.contains("features"), "{line}");
}

#[test]
fn full_state_carries_icon_ids() {
    let mut state = current();
    state.default_device.sessions[0].icon_id = Some(IconId::new("0a1b").unwrap());
    let line = encode_full_state(&state);
    assert!(
        line.starts_with(r#"{"protocolVersion":8,"applicationVersion":"test","deviceIds""#),
        "{line}"
    );
    assert!(
        line.contains(r#""volume":35.0,"muted":false,"iconId":"0a1b"}"#),
        "{line}"
    );
    assert_eq!(serde_json::from_str::<FullState>(&line).unwrap(), state);
}

// Requests.

#[test]
fn icon_requests_do_not_change_updates() {
    for line in client_update_lines() {
        let update = parse(&line);
        assert_eq!(update.icon_request, None, "{line}");
        let reencoded = serde_json::to_value(&update).unwrap();
        assert!(reencoded.get("iconRequest").is_none(), "{line}");

        // The same message with an iconRequest added plans the same commands.
        let mut with_icons: Value = serde_json::from_str(&line).unwrap();
        with_icons["iconRequest"] = json!({ "ids": ["abc"] });
        let both = parse(&with_icons.to_string());
        assert_eq!(both.default_device, update.default_device, "{line}");
        assert_eq!(
            plan_commands(Some(&current()), &both),
            plan_commands(Some(&current()), &update),
            "{line}"
        );
    }
}

#[test]
fn parses_icon_request_only() {
    let u = parse(&icon_request_line(
        json!({ "ids": ["abc", "def"], "size": 96 }),
    ));
    assert_eq!(u.default_device, None);
    let r = u.icon_request.unwrap();
    assert_eq!(r.ids, ["abc", "def"]);
    assert_eq!(r.size_px(), 96);
    assert!(
        plan_commands(
            Some(&current()),
            &parse(&icon_request_line(json!({ "ids": [] })))
        )
        .is_empty()
    );
}

#[test]
fn parses_icon_request_with_an_update() {
    let line = format!(
        r#"{{"protocolVersion":8,"defaultDevice":{{"deviceId":"{SPEAKERS}","masterVolume":10}},"iconRequest":{{"ids":["abc"]}}}}"#
    );
    let u = parse(&line);
    assert_eq!(u.icon_request.as_ref().unwrap().ids, ["abc"]);
    assert_eq!(
        u.icon_request.as_ref().unwrap().size_px(),
        ICON_SIZE_DEFAULT
    );
    assert_eq!(
        plan_commands(Some(&current()), &u),
        vec![Command::SetMaster {
            volume: Some(10.0),
            muted: None
        }]
    );
}

#[test]
fn null_icon_request_is_absent() {
    let u = parse(r#"{"protocolVersion":8,"iconRequest":null}"#);
    assert_eq!(u.icon_request, None);
}

#[test]
fn invalid_looking_ids_parse() {
    let u = parse(&icon_request_line(
        json!({ "ids": ["", "UPPER", "../etc/passwd", "x".repeat(200)] }),
    ));
    assert_eq!(u.icon_request.unwrap().ids.len(), 4);
}

#[test]
fn sizes_are_clamped_and_rounded() {
    for (size, expected) in [
        (json!(64), 64),
        (json!(64.0), 64),
        (json!(63.6), 64),
        (json!(15), ICON_SIZE_MIN),
        (json!(0), ICON_SIZE_MIN),
        (json!(-5), ICON_SIZE_MIN),
        (json!(257), ICON_SIZE_MAX),
        (json!(1e300), ICON_SIZE_MAX),
        (json!(u64::MAX), ICON_SIZE_MAX),
        (json!(i64::MIN), ICON_SIZE_MIN),
        (Value::Null, ICON_SIZE_DEFAULT),
    ] {
        let u = parse(&icon_request_line(json!({ "ids": ["a"], "size": size })));
        assert_eq!(u.icon_request.unwrap().size_px(), expected, "{size}");
    }
}

#[test]
fn duplicate_ids_are_removed_keeping_first_order() {
    let u = parse(&icon_request_line(
        json!({ "ids": ["b", "a", "b", "c", "a"] }),
    ));
    assert_eq!(u.icon_request.unwrap().unique_ids(), ["b", "a", "c"]);
}

#[test]
fn max_ids_is_accepted_and_one_more_is_rejected() {
    let ids: Vec<String> = (0..=MAX_ICON_IDS_PER_REQUEST)
        .map(|i| i.to_string())
        .collect();
    let u = parse(&icon_request_line(
        json!({ "ids": ids[..MAX_ICON_IDS_PER_REQUEST] }),
    ));
    assert_eq!(u.icon_request.unwrap().ids.len(), 32);

    match parse_client_line(&icon_request_line(json!({ "ids": ids }))) {
        Err(ProtocolError::TooManyIconIds { count: 33 }) => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn version_is_checked_before_the_id_limit() {
    let ids: Vec<String> = (0..40).map(|i| i.to_string()).collect();
    let line = json!({ "protocolVersion": 7, "iconRequest": { "ids": ids } }).to_string();
    assert!(matches!(
        parse_client_line(&line),
        Err(ProtocolError::VersionMismatch { .. })
    ));
}

#[test]
fn rejects_malformed_icon_requests() {
    for request in [
        json!([["a"], 64]),
        json!("abc"),
        json!(true),
        json!({}),
        json!({ "ids": "abc" }),
        json!({ "ids": [1] }),
        json!({ "ids": [null] }),
        json!({ "ids": [["a"]] }),
        json!({ "ids": null }),
        json!({ "ids": ["a"], "size": "64" }),
        json!({ "ids": ["a"], "size": [64] }),
    ] {
        assert!(
            matches!(
                parse_client_line(&icon_request_line(request.clone())),
                Err(ProtocolError::Malformed(_))
            ),
            "{request}"
        );
    }
}

// Replies.

#[test]
fn oversized_image_is_not_found() {
    let png = vec![0u8; MAX_ICON_PNG_BYTES];
    assert!(matches!(
        IconReply::image("a", 256, &png),
        IconReply::Image { .. }
    ));
    let png = vec![0u8; MAX_ICON_PNG_BYTES + 1];
    assert_eq!(IconReply::image("a", 256, &png), IconReply::not_found("a"));
}

#[test]
fn base64_matches_rfc_4648_vectors() {
    for (input, expected) in [
        (&b""[..], ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
        // Standard alphabet, not URL-safe.
        (&[0xfb, 0xff], "+/8="),
    ] {
        let IconReply::Image { data, .. } = IconReply::image("a", 1, input) else {
            unreachable!()
        };
        assert_eq!(data, expected);
    }
}

#[test]
fn image_reply_encoding_is_stable() {
    let png = b"\x89PNG\r\n\x1a\n\x00\x01\x02";
    let line = encode_icon_reply(IconReply::image("abc", 64, png));
    assert_eq!(
        line,
        "{\"protocolVersion\":8,\"icon\":{\"id\":\"abc\",\"size\":64,\"mime\":\"image/png\",\"data\":\"iVBORw0KGgoAAQI=\"}}\n"
    );
    let message: IconMessage = serde_json::from_str(&line).unwrap();
    let IconReply::Image { data, .. } = &message.icon else {
        panic!("{message:?}")
    };
    assert_eq!(BASE64.decode(data).unwrap(), png);
}

#[test]
fn not_found_reply_encoding_is_stable() {
    let line = encode_icon_reply(IconReply::not_found("abc"));
    assert_eq!(
        line,
        "{\"protocolVersion\":8,\"icon\":{\"id\":\"abc\",\"error\":\"notFound\"}}\n"
    );
    let message: IconMessage = serde_json::from_str(&line).unwrap();
    assert_eq!(message.icon, IconReply::not_found("abc"));
}

#[test]
fn reply_echoes_odd_ids_as_valid_json() {
    let id = "a\"b\\c\n\u{e9}";
    let line = encode_icon_reply(IconReply::not_found(id));
    assert_eq!(line.matches('\n').count(), 1);
    let message: IconMessage = serde_json::from_str(&line).unwrap();
    assert_eq!(message.icon.id(), id);
}

#[test]
fn icon_lines_have_no_default_device() {
    for reply in [IconReply::image("a", 16, b"x"), IconReply::not_found("a")] {
        let value: Value = serde_json::from_str(&encode_icon_reply(reply)).unwrap();
        assert!(value.get("defaultDevice").is_none());
        assert!(value.get("icon").is_some());
        assert!(serde_json::from_value::<FullState>(value).is_err());
    }
}
