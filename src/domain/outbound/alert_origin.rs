//! Invisible operational-alert provenance marker on outbound text, removed again at intake.

use std::borrow::Cow;
use std::sync::OnceLock;

const OPERATIONAL_ALERT_ORIGIN_LITERAL: &str = "[origin=operational_alert]";

fn hidden_operational_alert_origin_marker() -> &'static str {
    static MARKER: OnceLock<String> = OnceLock::new();
    MARKER.get_or_init(|| {
        OPERATIONAL_ALERT_ORIGIN_LITERAL
            .bytes()
            .flat_map(|byte| {
                (0..8).rev().map(move |shift| {
                    if (byte >> shift) & 1 == 1 {
                        '\u{200C}'
                    } else {
                        '\u{200B}'
                    }
                })
            })
            .collect()
    })
}

/// Prefix an operational alert with a provenance marker removed at intake.
/// The marker is invisible in Discord while remaining stable in message text.
pub(crate) fn prepend_operational_alert_origin(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.starts_with(hidden_operational_alert_origin_marker())
        || trimmed.starts_with(OPERATIONAL_ALERT_ORIGIN_LITERAL)
    {
        trimmed.to_string()
    } else {
        format!("{}{}", hidden_operational_alert_origin_marker(), trimmed)
    }
}

/// Keep Discord's subtext prefix before the invisible provenance marker.
pub(crate) fn prepend_inline_operational_alert_origin(text: &str) -> String {
    let trimmed = text.trim();
    if strip_operational_alert_origin(trimmed).1 {
        return trimmed.to_string();
    }
    match trimmed.strip_prefix("-# ") {
        Some(body) => format!("-# {}{body}", hidden_operational_alert_origin_marker()),
        None => prepend_operational_alert_origin(trimmed),
    }
}

pub(crate) fn strip_operational_alert_origin<'a>(text: &'a str) -> (Cow<'a, str>, bool) {
    if let Some(body) = text.strip_prefix("-# ") {
        let (body, marked) = strip_operational_alert_origin(body);
        if marked {
            return (Cow::Owned(format!("-# {body}")), true);
        }
    }
    if let Some(rest) = text.strip_prefix(hidden_operational_alert_origin_marker()) {
        return (Cow::Borrowed(rest), true);
    }

    if let Some(rest) = text.strip_prefix(OPERATIONAL_ALERT_ORIGIN_LITERAL) {
        return (Cow::Owned(rest.trim_start().to_string()), true);
    }

    (Cow::Borrowed(text), false)
}
