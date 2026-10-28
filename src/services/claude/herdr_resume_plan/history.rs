//! Reconcile launch evidence with the writer's settled source and verified transition links.
use super::*;
use crate::services::tui_prompt_dedupe::binding_events::{
    StrictBindingRecord, pending_matches, strict_claude_history,
};

fn source_of(event: &BindingEvent) -> Option<&SourceId> {
    match &event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some(source),
        _ => None,
    }
}

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
        })
        .collect();
    let refuse = || {
        HoldReason::Conflict(
            "launch selector has no verified same-execution transition chain".into(),
        )
    };
    let Some(first) = relevant.iter().position(|record| {
        record.event.execution_nonce.as_deref() == Some(context.execution_nonce.as_str())
    }) else {
        return Ok(None);
    };
    let span = &relevant[first..];
    if span.iter().any(|record| {
        record.event.execution_nonce.as_deref() != Some(context.execution_nonce.as_str())
    }) {
        return Err(refuse());
    }
    let Some(latest) = span
        .iter()
        .rev()
        .find(|record| !matches!(record.event.new, BindingTarget::Rejected { .. }))
        .and_then(|record| source_of(&record.event))
    else {
        return Ok(None);
    };
    let stable = span
        .iter()
        .filter_map(|record| source_of(&record.event))
        .all(|source| source == latest);
    let same_launch =
        context.expected_native_session_id.as_deref() == Some(latest.session_id.as_str());
    // With one unchanged source and no Pending, replay cannot change current or awaiting.
    if same_launch
        && stable
        && !span
            .iter()
            .any(|record| matches!(record.event.new, BindingTarget::Pending { .. }))
    {
        return Ok(None);
    }
    let folded = strict_claude_history(
        span.iter().copied(),
        &context.tmux_session,
        &context.execution_nonce,
    );
    if !folded.complete || folded.awaiting.is_some() {
        return Err(refuse());
    }

    if folded
        .current
        .as_ref()
        .is_none_or(|visit| visit.session != latest.session_id)
    {
        return Err(refuse());
    }
    let Some(expected) = context.expected_native_session_id.as_deref() else {
        return Ok(None);
    };
    if latest.session_id == expected && stable {
        return Ok(None);
    }
    let anchor = span
        .iter()
        .position(|record| source_of(&record.event).is_some())
        .ok_or_else(refuse)?;
    let mut current = source_of(&span[anchor].event).cloned().ok_or_else(refuse)?;
    if current.session_id != expected {
        return Err(refuse());
    }
    let resolved_pair = |event: &BindingEvent, source: &SourceId| -> Result<(), HoldReason> {
        let BindingTarget::Resolved { pending_seq, .. } = &event.new else {
            return Ok(());
        };
        let prior = span
            .iter()
            .find(|record| record.event.seq == *pending_seq)
            .map(|record| &record.event)
            .ok_or_else(refuse)?;
        if !matches!(prior.new, BindingTarget::Pending { .. })
            || prior.seq >= event.seq
            || prior.cause != event.cause
            || !pending_matches(
                prior,
                Some(&source.session_id),
                source.path.to_str().ok_or_else(refuse)?,
            )
        {
            return Err(refuse());
        }
        Ok(())
    };
    resolved_pair(&span[anchor].event, &current)?;
    if matches!(span[anchor].event.new, BindingTarget::Resolved { .. }) && !span[anchor].verified {
        return Err(refuse());
    }
    for record in &span[anchor + 1..] {
        let event = &record.event;
        match &event.new {
            BindingTarget::Pending { .. } => {
                if event.old.as_ref() != Some(&current) {
                    return Err(refuse());
                }
            }
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                resolved_pair(event, source)?;
                if source != &current {
                    if !record.verified || event.old.as_ref() != Some(&current) {
                        return Err(refuse());
                    }
                    current = source.clone();
                }
            }
            BindingTarget::Rejected { .. } => {}
        }
    }
    if &current != latest
        || folded.current.as_ref().and_then(|visit| visit.pin.as_ref()) != Some(&current)
    {
        return Err(refuse());
    }
    Ok(Some(current))
}
