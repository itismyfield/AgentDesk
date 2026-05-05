use sha2::{Digest, Sha256};

const USER_DERIVED_PREVIEW_CHARS: usize = 240;

/// Marker appended to truncated `full_content` so transcript readers can detect
/// the truncation client-side without reading the schema.
const TRUNCATION_MARKER: &str = "...[truncated by retention policy]";

pub(super) fn apply_byte_cap(body: String, cap: Option<usize>) -> (String, bool) {
    let Some(cap) = cap else {
        return (body, false);
    };
    if body.len() <= cap {
        return (body, false);
    }
    let marker = TRUNCATION_MARKER;
    // Reserve room for the marker; if the cap is smaller than the marker
    // itself, just emit a marker-only body so the row still hashes the
    // original content but stores something legible.
    if cap <= marker.len() {
        return (marker.to_string(), true);
    }
    let budget = cap - marker.len();
    let mut end = budget;
    // Walk back to the nearest UTF-8 char boundary.
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + marker.len());
    out.push_str(&body[..end]);
    out.push_str(marker);
    (out, true)
}

pub(super) fn estimate_tokens_from_chars_i64(chars: i64) -> i64 {
    if chars <= 0 { 0 } else { chars / 4 }
}

pub(super) fn sha256_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

pub(super) fn redacted_preview(content: &str) -> Option<String> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    Some(truncate_chars(content, USER_DERIVED_PREVIEW_CHARS))
}

pub(super) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    if value.chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

pub(super) fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn normalized_opt_owned(value: Option<String>) -> Option<String> {
    normalized_opt(value.as_deref())
}

pub(super) fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
