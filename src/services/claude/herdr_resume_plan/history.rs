//! Reconcile a launch selector only through verified transitions of the same execution.
use super::*;
use crate::services::tui_prompt_dedupe::binding_events::{BindingCause, StrictBindingRecord};

pub(super) fn reconcile(
    context: &BindingContext,
    records: &[StrictBindingRecord],
) -> Result<Option<SourceId>, HoldReason> {
    let relevant: Vec<_> = records
        .iter()
        .filter(|record| {
            let event = &record.event;
            event.provider == "claude"
                && event.tmux_session == context.tmux_session
                && event.channel_id == context.channel_id.unwrap_or_default()
                && !matches!(event.new, BindingTarget::Rejected { .. })
        })
        .collect();
    let Some(expected) = context.expected_native_session_id.as_deref() else {
        return Ok(None);
    };
    let target = |event: &BindingEvent| match &event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
            Some(source.clone())
        }
        _ => None,
    };
    let Some(latest) = relevant.last().and_then(|record| target(&record.event)) else {
        return Ok(None);
    };
    if latest.session_id == expected {
        return Ok(None);
    }
    let refuse = || {
        HoldReason::Conflict(
            "launch selector has no verified same-execution transition chain".into(),
        )
    };
    let first = relevant
        .iter()
        .position(|record| {
            record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
        })
        .ok_or_else(refuse)?;
    if relevant[first..].iter().any(|record| {
        record.event.execution_nonce.as_deref() != Some(context.execution_nonce.as_str())
    }) {
        return Err(refuse());
    }
    if target(&relevant[first].event).is_none_or(|source| source.session_id != expected) {
        return Err(refuse());
    }
    let mut current = target(&relevant[first].event).ok_or_else(refuse)?;
    let mut pending: Option<&BindingEvent> = None;
    for record in &relevant[first + 1..] {
        let event = &record.event;
        if event.execution_nonce.as_deref() != Some(context.execution_nonce.as_str()) {
            return Err(refuse());
        }
        match &event.new {
            BindingTarget::Pending { .. } => {
                if event.old.as_ref() != Some(&current) {
                    return Err(refuse());
                }
                pending = Some(event);
            }
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                if source == &current {
                    continue;
                }
                let transition = matches!(
                    event.cause,
                    BindingCause::Clear | BindingCause::Compact | BindingCause::Continuation
                ) && matches!(
                    event.evidence.hook_event.as_deref(),
                    Some("session_start" | "user_prompt_submit")
                ) || event.cause == BindingCause::Unknown
                    && event.evidence.hook_event.as_deref() == Some("user_prompt_submit");
                if !record.verified || !transition || event.old.as_ref() != Some(&current) {
                    return Err(refuse());
                }
                if let BindingTarget::Resolved { pending_seq, .. } = event.new {
                    let prior = pending.ok_or_else(refuse)?;
                    let BindingTarget::Pending {
                        payload_session_id,
                        payload_transcript_path,
                    } = &prior.new
                    else {
                        return Err(refuse());
                    };
                    if prior.seq != pending_seq
                        || prior.seq >= event.seq
                        || prior.cause != event.cause
                        || prior.old.as_ref() != Some(&current)
                        || payload_session_id != &source.session_id
                        || payload_transcript_path.as_deref() != source.path.to_str()
                    {
                        return Err(refuse());
                    }
                }
                current = source.clone();
                pending = None;
            }
            BindingTarget::Rejected { .. } => unreachable!(),
        }
    }
    if current != latest || pending.is_some() {
        return Err(refuse());
    }
    Ok(Some(current))
}
