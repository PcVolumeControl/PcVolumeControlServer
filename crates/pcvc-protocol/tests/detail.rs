use pcvc_protocol::{DETAIL_MAX_CHARS, FullState, encode_full_state, session_detail};
use serde_json::{Value, json};

const README_FULL_STATE: &str = include_str!("fixtures/readme_full_state.json");

#[test]
fn keeps_useful_text() {
    assert_eq!(
        session_detail("Lo-fi beats - YouTube", "Firefox").as_deref(),
        Some("Lo-fi beats - YouTube")
    );
    assert_eq!(
        session_detail("Caf\u{e9} \u{1f3b5}", "Firefox").as_deref(),
        Some("Caf\u{e9} \u{1f3b5}")
    );
}

#[test]
fn makes_one_trimmed_line_without_control_characters() {
    assert_eq!(
        session_detail("  Line one\r\n\tline\u{2028}two  ", "x").as_deref(),
        Some("Line one line two")
    );
    assert_eq!(
        session_detail("a\u{0}b\u{7}c\u{1b}[0m \u{85} d\u{7f}", "x").as_deref(),
        Some("abc[0m d")
    );
}

#[test]
fn drops_empty_text_and_the_name() {
    for raw in ["", "   ", "\n\t", "\u{0}\u{1}"] {
        assert_eq!(session_detail(raw, "Firefox"), None, "{raw:?}");
    }
    assert_eq!(session_detail("firefox", "Firefox"), None);
    assert_eq!(session_detail(" FIREFOX\n", "Firefox "), None);
    assert_eq!(session_detail("Mus\u{c9}E", "mus\u{e9}e"), None);
    assert_eq!(session_detail("Spotify  Free", "Spotify\tFree"), None);
}

#[test]
fn truncates_on_a_char_boundary() {
    let exact = "\u{e9}".repeat(DETAIL_MAX_CHARS);
    assert_eq!(session_detail(&exact, "x").as_deref(), Some(exact.as_str()));

    let long = "\u{1f3b5}".repeat(DETAIL_MAX_CHARS + 1);
    let cut = session_detail(&long, "x").unwrap();
    assert_eq!(cut.chars().count(), DETAIL_MAX_CHARS);
    assert!(cut.ends_with("\u{1f3b5}\u{2026}"), "{cut}");
    assert_eq!(session_detail(&cut, "x").as_deref(), Some(cut.as_str()));

    // No space before the ellipsis.
    let spaced = format!("{} {}", "a".repeat(DETAIL_MAX_CHARS - 2), "b".repeat(10));
    let cut = session_detail(&spaced, "x").unwrap();
    assert_eq!(cut, format!("{}\u{2026}", "a".repeat(DETAIL_MAX_CHARS - 2)));
}

#[test]
fn wire_shape() {
    let mut value: Value = serde_json::from_str(README_FULL_STATE).unwrap();
    value["features"] = json!(["icons", "detail"]);
    value["defaultDevice"]["sessions"][0]["detail"] = json!("Inbox (3) - Gmail");
    let state: FullState = serde_json::from_value(value).unwrap();
    let session = &state.default_device.sessions[0];
    assert_eq!(session.detail.as_deref(), Some("Inbox (3) - Gmail"));
    let line = encode_full_state(&state);
    assert!(
        line.contains(r#""volume":77.0,"muted":false,"detail":"Inbox (3) - Gmail"}"#),
        "{line}"
    );
    assert!(line.contains(r#""features":["icons","detail"]"#), "{line}");

    let mut plain = state.clone();
    plain.features.clear();
    plain.default_device.sessions[0].detail = None;
    assert!(!encode_full_state(&plain).contains("detail"));
}

#[test]
fn null_detail_is_absent() {
    let mut value: Value = serde_json::from_str(README_FULL_STATE).unwrap();
    value["defaultDevice"]["sessions"][0]["detail"] = Value::Null;
    let state: FullState = serde_json::from_value(value).unwrap();
    assert_eq!(state.default_device.sessions[0].detail, None);
}
