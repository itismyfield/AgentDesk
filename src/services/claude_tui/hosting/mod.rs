mod followup_support;
mod warm_followup;

pub(crate) use followup_support::emit_claude_tui_zero_harvest;
#[cfg(test)]
pub(crate) use followup_support::{
    ClaudeTuiStrandedPromptDraftState, claude_tui_followup_busy_before_submit_from_snapshot,
    claude_tui_followup_stranded_prompt_draft_state,
    claude_tui_unknown_transcript_draft_recreate_allowed, claude_tui_warm_followup_submit_plan,
};
#[cfg(test)]
pub(crate) use warm_followup::{ClaudeTuiDraftRecoveryOutcome, ClaudeTuiRecreateState};
pub(crate) use warm_followup::{ClaudeTuiWarmFollowupOutcome, try_claude_tui_warm_followup};
