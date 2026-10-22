//! Source-aware entries keep the existing observation fold and publication path.

use chrono::{DateTime, Utc};
#[cfg(unix)]
use std::path::Path;

use super::{HostedSourcePin, capture_hook};
#[cfg(unix)]
use crate::services::tui_prompt_dedupe::RecordEvidence;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::{
    self as dedupe, ClaudePromptId, PromptObservation, handoff,
    observation::{PromptObservationEffect, observe_inner},
};

pub(crate) fn observe_hook_at(
    provider: &str,
    logical: &str,
    prompt: &str,
    (prompt_id, native_turn): (Option<&str>, Option<&str>),
    at: DateTime<Utc>,
    payload: &serde_json::Value,
    context: Option<&BindingContext>,
) -> PromptObservation {
    let pin = dedupe::owner_channel_for_tmux_session(logical)
        .and_then(|channel| capture_hook(provider, logical, channel, payload, context));
    let evidence = if native_turn.is_some() {
        handoff::Evidence::Live
    } else {
        handoff::Evidence::Unattributed
    };
    observe_inner(
        provider,
        logical,
        &[prompt.to_owned()],
        (None, prompt_id.map(ClaudePromptId::HookSubmit), native_turn),
        PromptObservationEffect::NotifyAndLease,
        at,
        (evidence, pin),
    )
}

#[cfg(unix)]
pub(crate) fn observe_claude_record_at(
    logical: &str,
    prompt: &str,
    (entry, prompt_id): (Option<&str>, Option<&str>),
    at: DateTime<Utc>,
    pin: Option<HostedSourcePin>,
) -> PromptObservation {
    observe_inner(
        "claude",
        logical,
        &[prompt.to_owned()],
        (entry, prompt_id.map(ClaudePromptId::TranscriptRow), None),
        PromptObservationEffect::NotifyAndLease,
        at,
        (handoff::Evidence::Unattributed, pin),
    )
}

#[cfg(unix)]
pub(crate) fn observe_codex_record_at(
    logical: &str,
    (path, start, record): (&Path, u64, &RecordEvidence),
    (prompt, entry, inferred): (&str, Option<&str>, Option<&str>),
    at: DateTime<Utc>,
    pin: Option<HostedSourcePin>,
) -> PromptObservation {
    let turn = record.turn_id.as_deref().or(inferred);
    if !handoff::codex_record_admission(logical, path, start, prompt, (entry, turn), record) {
        return PromptObservation::Ignored;
    }
    observe_inner(
        "codex",
        logical,
        &[prompt.to_owned()],
        (entry, None, turn),
        PromptObservationEffect::NotifyAndLease,
        at,
        (
            handoff::Evidence::Restored {
                record,
                path,
                start,
                entry,
            },
            pin,
        ),
    )
}
