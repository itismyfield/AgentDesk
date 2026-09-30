//! Stranded draft recovery before a warm Claude TUI follow-up: the executor sends
//! every key and decides a typed outcome; only confirmed tmux is ever retired.

use std::time::Duration;

use super::followup_support::{
    ClaudeTuiStrandedPromptDraftState, claude_tui_unknown_transcript_draft_recreate_allowed,
};
use crate::services::claude_tui::host_input::{
    self, HostInputOutcome, InputRefusal, InputRun, InputTarget, KeyGroups, LegacyTmuxGate,
    MutationGate, StopCause,
};
use crate::services::claude_tui::input::{
    PromptReadinessSnapshot, claude_prompt_draft_backspace_budget_from_tail,
    prompt_readiness_snapshot,
};
use crate::services::provider::CancelToken;
use crate::services::session_host::HostKey;

const DRAFT_CLEAR_ATTEMPTS: usize = 2;
const DRAFT_KEY_SETTLE: Duration = Duration::from_millis(120);

/// The host a warm follow-up drives and the gate admitting each mutation.
pub(crate) struct FollowupHost<'a> {
    pub(crate) target: InputTarget,
    pub(crate) gate: &'a dyn MutationGate,
}

impl FollowupHost<'static> {
    /// Production entry, decided before any IO: no Herdr selector exists, so
    /// the follow-up drives the legacy tmux session of that name.
    pub(crate) fn legacy_tmux(session_name: &str) -> Self {
        Self {
            target: InputTarget::legacy_tmux(session_name),
            gate: &LegacyTmuxGate,
        }
    }
}

impl FollowupHost<'_> {
    pub(crate) fn session(&self) -> Result<&str, InputRefusal> {
        match &self.target {
            InputTarget::Tmux(session) => Ok(session),
            InputTarget::Refused(refusal) => Err(*refusal),
        }
    }

    /// Retires a confirmed tmux session the gate still admits; the result is
    /// `LegacyRecreateEligible` only when the session was handed off.
    pub(crate) fn retire(&self, reason_code: &str, reason: &str) -> HostInputOutcome {
        let admitted = self
            .session()
            .and_then(|session| self.gate.admit(session).map(|()| session));
        let session = match admitted {
            Ok(session) => session,
            Err(refusal) => return HostInputOutcome::Refused(refusal),
        };
        host_input::legacy_retire(session, reason_code, reason);
        HostInputOutcome::LegacyRecreateEligible
    }
}

/// The error a follow-up returns when the executor stops it: never a readiness
/// timeout, so nothing requeues it.
pub(crate) fn stopped_error(outcome: &HostInputOutcome) -> String {
    format!("claude tui follow-up stopped by the host input executor: {outcome:?}")
}

#[derive(Clone, Copy)]
pub(crate) enum DraftClear {
    /// `C-e C-u`, `Escape`, `C-e C-u`, then backspaces, for an idle transcript.
    Strong,
    /// `C-e C-u`, then backspaces, when the transcript state is unknown.
    Gentle,
}

pub(crate) struct ClearedDraft {
    pub(crate) run: InputRun,
    /// The last pane read, before or after the keys.
    pub(crate) snapshot: PromptReadinessSnapshot,
}

/// Clears a stranded draft with the legacy key plans, reading the pane first.
pub(crate) fn clear_draft(
    host: &FollowupHost,
    session: &str,
    clear: DraftClear,
    cancel_token: Option<&CancelToken>,
) -> ClearedDraft {
    let mut keys = KeyGroups::new(session, host.gate, cancel_token);
    let mut snapshot = prompt_readiness_snapshot(session);
    let run = clear_attempts(&mut keys, clear, &mut snapshot)
        .err()
        .unwrap_or(InputRun::Applied);
    ClearedDraft { run, snapshot }
}

fn draft_gone(snapshot: &PromptReadinessSnapshot) -> bool {
    !snapshot.prompt_draft_detected || !snapshot.tmux_pane_alive
}

fn clear_attempts(
    keys: &mut KeyGroups,
    clear: DraftClear,
    snapshot: &mut PromptReadinessSnapshot,
) -> Result<(), InputRun> {
    const CLEAR_LINE: &[HostKey] = &[HostKey::CtrlE, HostKey::CtrlU];
    let (groups, name, cancel_after_settle): (&[&[HostKey]], &str, bool) = match clear {
        DraftClear::Strong => (
            &[CLEAR_LINE, &[HostKey::Escape], CLEAR_LINE],
            "clear-draft",
            true,
        ),
        DraftClear::Gentle => (&[CLEAR_LINE], "gentle-clear-draft", false),
    };
    if draft_gone(snapshot) {
        return Ok(());
    }
    for attempt in 1..=DRAFT_CLEAR_ATTEMPTS {
        keys.check_cancel()?;
        for group in groups {
            keys.send(group, name)?;
            std::thread::sleep(DRAFT_KEY_SETTLE);
            if cancel_after_settle {
                keys.check_cancel()?;
            }
            *snapshot = prompt_readiness_snapshot(keys.session);
            if draft_gone(snapshot) {
                return Ok(());
            }
        }
        clear_with_backspaces(keys, snapshot)?;
        if draft_gone(snapshot) {
            return Ok(());
        }
        tracing::warn!(
            tmux_session_name = keys.session,
            attempt,
            clear = name,
            pane_tail = %snapshot.pane_tail,
            "claude_tui stranded prompt draft still present after clear attempt"
        );
    }
    Ok(())
}

fn clear_with_backspaces(
    keys: &mut KeyGroups,
    snapshot: &mut PromptReadinessSnapshot,
) -> Result<(), InputRun> {
    let Some(mut remaining) = claude_prompt_draft_backspace_budget_from_tail(&snapshot.pane_tail)
    else {
        return Ok(());
    };
    keys.send(&[HostKey::CtrlE], "draft-clear-cursor-end")?;
    while remaining > 0 {
        keys.check_cancel()?;
        let batch = remaining.min(32);
        keys.send(&vec![HostKey::Backspace; batch], "draft-clear-backspace")?;
        remaining -= batch;
    }
    std::thread::sleep(DRAFT_KEY_SETTLE);
    *snapshot = prompt_readiness_snapshot(keys.session);
    Ok(())
}

/// Typed result of a draft clear on confirmed tmux: recreation only under the legacy
/// stranded-draft policy, never after a gate refusal, a cancel or a cleared draft.
pub(crate) fn draft_outcome(
    state: ClaudeTuiStrandedPromptDraftState,
    before: &PromptReadinessSnapshot,
    cleared: &ClearedDraft,
) -> HostInputOutcome {
    let recreate = |snapshot: &PromptReadinessSnapshot| match state {
        ClaudeTuiStrandedPromptDraftState::IdleTranscript => true,
        ClaudeTuiStrandedPromptDraftState::UnknownTranscript => {
            claude_tui_unknown_transcript_draft_recreate_allowed(snapshot)
        }
    };
    let after = &cleared.snapshot;
    match &cleared.run {
        InputRun::Applied if after.tmux_pane_alive && !after.prompt_draft_detected => {
            HostInputOutcome::Cleared
        }
        InputRun::Applied if recreate(after) => HostInputOutcome::LegacyRecreateEligible,
        InputRun::Applied if after.tmux_pane_alive => HostInputOutcome::PersistentDraft,
        InputRun::Applied => HostInputOutcome::UnknownTranscript,
        InputRun::Indeterminate {
            cause: StopCause::Send(_),
            ..
        } if recreate(before) => HostInputOutcome::LegacyRecreateEligible,
        stopped => HostInputOutcome::from_stopped_run(stopped)
            .unwrap_or(HostInputOutcome::UnknownTranscript),
    }
}

/// Audit code and reason a recreate-eligible clear records, as before.
pub(crate) fn recreate_reason(cleared: &ClearedDraft) -> (&'static str, String) {
    match &cleared.run {
        InputRun::Indeterminate {
            cause: StopCause::Send(error),
            ..
        } => (
            "stranded_prompt_draft_clear_failed_recreate",
            format!("claude tui stranded prompt draft clear failed: {error}"),
        ),
        _ if cleared.snapshot.tmux_pane_alive => (
            "stranded_prompt_draft_recreate",
            "stranded claude tui prompt draft persisted after clear attempts".to_string(),
        ),
        _ => (
            "stranded_prompt_draft_recreate",
            "claude tui pane died while clearing stranded prompt draft".to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "host_draft_tests.rs"]
mod tests;
