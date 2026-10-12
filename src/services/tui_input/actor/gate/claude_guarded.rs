//! Ownership evidence for the dormant Claude guarded reader.

use crate::services::claude_tui::input::TuiInputAction;
use crate::services::codex_tui::input::strip_ansi_escape_sequences;

pub(crate) struct DraftEvidence<'a> {
    frame: &'a str,
    payload: &'a [TuiInputAction],
    pre_empty: bool,
    exclusive_writer: bool,
}

impl<'a> DraftEvidence<'a> {
    /// Derive PRE and ownership from the locked, detached pane and current target gate.
    pub(crate) fn observe(
        frame: &'a str,
        payload: &'a [TuiInputAction],
        permit: &'a crate::services::claude_tui::composer_lock::ComposerPermit<'_>,
        pane: &crate::services::tui_input::submission_tmux::guarded::GuardedPane,
        gate: &dyn crate::services::claude_tui::host_input::MutationGate,
    ) -> Result<Self, crate::services::claude_tui::host_input::InputRefusal> {
        use crate::services::claude_tui::host_input::InputRefusal;
        use crate::services::tui_input::submission::Refusal;
        if !pane.session_matches(permit.session()) {
            return Err(InputRefusal::IdentityMismatch);
        }
        gate.admit(permit.session())?;
        let before = pane
            .capture_draft()
            .ok_or(InputRefusal::Composer(Refusal::CaptureUnavailable))?;
        let pre_empty = super::judge_pane(
            crate::services::tui_o::shadow::ShadowProvider::Claude,
            &before,
        ) == super::PaneVerdict::Ready;
        if !pre_empty {
            return Err(InputRefusal::Composer(Refusal::NotReady));
        }
        Ok(Self {
            frame,
            payload,
            pre_empty,
            exclusive_writer: true,
        })
    }
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
