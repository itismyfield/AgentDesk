//! Ownership evidence for the dormant Claude guarded reader.

use crate::services::claude_tui::input::TuiInputAction;
use crate::services::codex_tui::input::strip_ansi_escape_sequences;

pub(crate) struct DraftEvidence<'a> {
    pub frame: &'a str,
    pub payload: &'a [TuiInputAction],
    pub pre_empty: bool,
    pub exclusive_writer: bool,
}

pub(crate) fn claude_should_fold(frame: &str) -> bool {
    frame.encode_utf16().count() > 800 || frame.bytes().filter(|b| *b == b'\n').count() >= 3
}

pub(crate) fn own_claude_guarded_draft(capture: &str, evidence: DraftEvidence<'_>) -> bool {
    if !evidence.pre_empty || !evidence.exclusive_writer {
        return false;
    }
    let mut transmitted = String::new();
    for action in evidence.payload {
        match action {
            TuiInputAction::Literal(text) | TuiInputAction::PasteBuffer(text) => {
                transmitted.push_str(text);
            }
            _ => return false,
        }
    }
    if transmitted != evidence.frame || !super::claude_composer_not_known_empty(capture) {
        return false;
    }
    let plain = strip_ansi_escape_sequences(capture);
    let Some(rows) = super::claude_composer_rows(&plain) else {
        return false;
    };
    let body = rows.join("\n");
    if body == evidence.frame {
        return true;
    }
    // A placeholder is not a digest; only one unchanged paste under exclusive ownership admits it.
    if !matches!(evidence.payload, [TuiInputAction::PasteBuffer(text)] if text == evidence.frame)
        || !claude_should_fold(evidence.frame)
    {
        return false;
    }
    let Some(rest) = body.strip_prefix("[Pasted text #") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let suffix = &rest[digits..];
    let lf = evidence.frame.bytes().filter(|b| *b == b'\n').count();
    if lf == 0 {
        suffix == "]"
    } else {
        suffix == format!(" +{lf} lines]")
    }
}

#[cfg(test)]
mod acceptance_tests;
