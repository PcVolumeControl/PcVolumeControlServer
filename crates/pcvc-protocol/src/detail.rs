//! The session detail extension: a short secondary label that tells apart
//! sessions of the same application.

/// Feature name advertised in [`crate::FullState::features`] and in the mDNS
/// `features` TXT record when sessions may carry a detail.
pub const FEATURE_DETAIL: &str = "detail";

/// Longest detail sent, in characters (Unicode scalar values).
pub const DETAIL_MAX_CHARS: usize = 120;

/// `raw` as the detail of a session named `name`, or `None` if nothing useful
/// is left.
///
/// Whitespace runs (line breaks and tabs included) become one space; other
/// control characters and the [`is_invisible`] ones are dropped; zero-width
/// joiners and non-joiners are kept only between two visible characters;
/// the ends are trimmed. Text equal to the name, both cleaned this way,
/// joiners removed and lower-cased (no Unicode normalisation), is dropped.
/// Text longer than [`DETAIL_MAX_CHARS`] is cut on a character boundary and
/// ends with `…`, still within the limit.
pub fn session_detail(raw: &str, name: &str) -> Option<String> {
    let detail = single_line(raw);
    let key = |text: &str| text.replace(is_joiner, "").to_lowercase();
    if detail.is_empty() || key(&detail) == key(&single_line(name)) {
        return None;
    }
    Some(truncate(detail))
}

/// Zero-width, soft hyphen and bidirectional formatting characters:
/// invisible on their own, and an unterminated override or isolate would
/// reorder whatever a client draws after the detail.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// Zero-width non-joiner and joiner, which shape emoji sequences and some
/// scripts.
fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}')
}

fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    let mut joiners = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            joiners.clear();
        } else if is_joiner(c) {
            if !out.is_empty() && !space {
                joiners.push(c);
            }
        } else if !c.is_control() && !is_invisible(c) {
            if space {
                out.push(' ');
                space = false;
            }
            out.push_str(&joiners);
            joiners.clear();
            out.push(c);
        }
    }
    out
}

fn truncate(text: String) -> String {
    if text.chars().nth(DETAIL_MAX_CHARS).is_none() {
        return text;
    }
    let mut cut: String = text.chars().take(DETAIL_MAX_CHARS - 1).collect();
    cut.truncate(
        cut.trim_end_matches(|c: char| c.is_whitespace() || is_joiner(c))
            .len(),
    );
    cut.push('\u{2026}');
    cut
}
