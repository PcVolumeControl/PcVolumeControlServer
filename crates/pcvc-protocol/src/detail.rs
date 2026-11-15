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
/// Whitespace runs (line breaks and tabs included) become one space, other
/// control characters are dropped and the ends are trimmed. Text equal to the
/// name, ignoring case and the same whitespace rules, is dropped. Text longer
/// than [`DETAIL_MAX_CHARS`] is cut on a character boundary and ends with
/// `…`, still within the limit.
pub fn session_detail(raw: &str, name: &str) -> Option<String> {
    let detail = single_line(raw);
    if detail.is_empty() || detail.to_lowercase() == single_line(name).to_lowercase() {
        return None;
    }
    Some(truncate(detail))
}

fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else if !c.is_control() {
            if space {
                out.push(' ');
                space = false;
            }
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
    cut.truncate(cut.trim_end().len());
    cut.push('\u{2026}');
    cut
}
