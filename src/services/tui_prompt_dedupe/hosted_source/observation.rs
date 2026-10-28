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
    let logical = logical.trim();
    let channel = dedupe::owner_channel_for_tmux_session(logical);
    let pin =
        channel.and_then(|channel| capture_hook(provider, logical, channel, payload, context));
    let observe = |pin| {
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
    };
    if provider != "claude" {
        return observe(pin);
    }
    let order = dedupe::herdr_listing_order(logical);
    let _order = order.lock().unwrap_or_else(|error| error.into_inner());
    crate::services::tmux_common::with_tmux_source_authority(logical, |authority| {
        let required = match super::herdr_evidence(channel.unwrap_or(0), logical, None) {
            Ok(required) => {
                required
                    || pin
                        .as_ref()
                        .is_some_and(|pin| pin.hosted_admission_required)
            }
            Err(_) => return PromptObservation::Ignored,
        };
        if required {
            let Some(original) = pin.as_ref() else {
                return PromptObservation::Ignored;
            };
            if super::binding_context::observe_spawn_nonce_marker(logical)
                != super::SpawnNonceMarker::Known(original.execution_nonce.clone())
                || !dedupe::herdr_hook_admitted_under_source_authority(
                    authority,
                    &original.execution_nonce,
                )
            {
                return PromptObservation::Ignored;
            }
        }
        observe(pin)
    })
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
