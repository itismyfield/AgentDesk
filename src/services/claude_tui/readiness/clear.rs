//! A current clear can prepare its first input before publishing the new transcript binding.
use super::{ledger, source, source::Source};
use crate::services::tui_prompt_dedupe;
#[cfg(unix)]
use crate::services::tui_prompt_dedupe::{binding_context, binding_events};
use chrono::{DateTime, Utc};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Evidence {
    channel: u64,
    seq: u64,
    started: DateTime<Utc>,
}

#[cfg(unix)]
pub(super) fn evidence(source: &Source) -> Option<Evidence> {
    if !source::clear_usable(source) {
        return None;
    }
    let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(&source.session);
    evidence_with(source, binding.as_ref())
}
#[cfg(unix)]
pub(super) fn evidence_under(
    source: &Source,
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
) -> Option<Evidence> {
    if !source::clear_usable_under(source, authority) {
        return None;
    }
    let binding =
        tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority);
    evidence_with(source, binding.as_ref())
}
#[cfg(unix)]
fn evidence_with(
    source: &Source,
    binding: Option<&tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<Evidence> {
    use binding_events::{BindingCause, BindingTarget};
    let nonce = source.nonce.as_deref()?;
    if source::nonce(&source.session) != Some(source.nonce.clone()) {
        return None;
    }
    let context = binding_context::execution_context("claude", nonce).ok()?;
    let channel = context.channel_id?;
    if context.provider != "claude"
        || context.execution_nonce != nonce
        || context.tmux_session != source.session
        || !context.provider_root.as_deref().is_some_and(|root| {
            super::super::source_verify::is_top_level_transcript(root, &source.path, &source.native)
        })
    {
        return None;
    }
    let started = {
        let state = ledger();
        let (at, current) = state.starts.get(&(source.session.clone(), nonce.into()))?;
        if current != source || !state.entries.get(source)?.clear_start {
            return None;
        }
        *at
    };
    let snapshot = binding_events::published_claude_history(channel, &source.session, nonce)?;
    let records = binding_events::records_strict(channel).ok()?.ok()?;
    if records.last()?.seq != snapshot.0
        || !snapshot.1.complete
        || snapshot.1.awaiting.as_ref()?.session != source.native
    {
        return None;
    }
    let pane: Vec<_> = records
        .iter()
        .filter(|event| {
            event.tmux_session == source.session
                && !matches!(event.new, BindingTarget::Rejected { .. })
        })
        .collect();
    let clear = |event: &&&binding_events::BindingEvent| {
        event.execution_nonce.as_deref() == Some(nonce)
            && event.cause == BindingCause::Clear
            && event.evidence.hook_event.as_deref() == Some("session_start")
            && matches!(event.new, BindingTarget::Pending { .. })
    };
    let waiting = pane.iter().rev().take_while(clear).count();
    let (earlier, chain) = pane.split_at(pane.len() - waiting);
    let previous = earlier
        .last()
        .filter(|event| event.execution_nonce.as_deref() == Some(nonce))?;
    let from = match &previous.new {
        BindingTarget::Source(from) | BindingTarget::Resolved { source: from, .. } => from,
        _ => return None,
    };
    if chain.iter().any(|event| event.old.as_ref() != Some(from)) {
        return None;
    }
    let before = *chain.last()?;
    if before.channel_id != channel || before.provider != "claude" {
        return None;
    }
    let BindingTarget::Pending {
        payload_session_id,
        payload_transcript_path,
    } = &before.new
    else {
        return None;
    };
    if *payload_session_id != source.native
        || payload_transcript_path.as_deref().map(Path::new) != Some(source.path.as_path())
    {
        return None;
    }
    let old = before.old.as_ref()?;
    if binding.is_some_and(|binding| {
        binding.session_id.as_deref() != Some(old.session_id.as_str())
            || Path::new(binding.relay_output_path()) != old.path
    }) || binding_events::published_claude_history(channel, &source.session, nonce).as_ref()
        != Some(&snapshot)
    {
        return None;
    }
    Some(Evidence {
        channel,
        seq: before.seq,
        started,
    })
}

pub(super) fn current(source: &Source) -> Option<Option<Evidence>> {
    let clear_start = ledger()
        .entries
        .get(source)
        .is_some_and(|entry| entry.clear_start);
    let Some(binding) = tui_prompt_dedupe::runtime_binding_for_tmux_session(&source.session) else {
        return if clear_start {
            evidence(source).map(Some)
        } else {
            Some(None)
        };
    };
    #[cfg(unix)]
    let pending = source
        .nonce
        .as_deref()
        .and_then(|nonce| binding_context::execution_context("claude", nonce).ok())
        .and_then(|ctx| Some((ctx.channel_id?, ctx.execution_nonce)))
        .and_then(|(channel, nonce)| {
            binding_events::published_claude_history(channel, &source.session, &nonce)
        })
        .is_some_and(|(_, history)| history.awaiting.is_some());
    #[cfg(not(unix))]
    let pending = false;
    if clear_start && pending && !source.path.exists() {
        return evidence(source).map(Some);
    }
    if binding
        .session_id
        .as_deref()
        .is_none_or(|native| native == source.native)
        && Path::new(binding.relay_output_path()) == source.path
    {
        Some(None)
    } else {
        evidence(source).map(Some)
    }
}

#[cfg(not(unix))]
pub(super) fn evidence(source: &Source) -> Option<Evidence> {
    let _ = source;
    None
}

pub(super) fn claim(source: &Source, expected: Option<&Evidence>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    #[cfg(unix)]
    {
        source::claim_clear(source, expected)
    }
    #[cfg(not(unix))]
    {
        let _ = (source, expected);
        false
    }
}
pub(super) fn release(source: &Source, expected: Option<&Evidence>) {
    let Some(expected) = expected else {
        return;
    };
    #[cfg(unix)]
    source::release_clear(source, expected);
    #[cfg(not(unix))]
    let _ = (source, expected);
}
impl Evidence {
    pub(super) fn same_pending(&self, channel: u64, seq: u64) -> bool {
        self.channel == channel && self.seq == seq
    }
}
