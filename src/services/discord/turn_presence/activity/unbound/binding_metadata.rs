//! Durable names can locate an absent target without granting permission to consume its source.

use super::*;
use crate::services::tmux_common;
use crate::services::tui_prompt_dedupe::{binding_context, binding_events as p5};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Snapshot {
    records: Vec<p5::BindingEvent>,
    name: Option<Name>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Name {
    seq: u64,
    context: binding_context::BindingContext,
}

impl Snapshot {
    pub(super) fn supplement(&self, candidates: &mut Candidates) -> Result<(), String> {
        let Some(name) = &self.name else {
            return Ok(());
        };
        if candidates.provider.as_str() != name.context.provider
            || name.context.channel_id != Some(candidates.channel)
        {
            return Err("binding metadata candidate scope mismatch".into());
        }
        if candidates
            .names
            .iter()
            .any(|candidate| candidate.name != name.context.tmux_session)
        {
            return Err("conflicting binding metadata names".into());
        }
        if !candidates.names.iter().any(|candidate| {
            candidate.name == name.context.tmux_session
                && candidate.origin == Origin::Binding(name.seq)
        }) {
            candidates.names.push(Candidate {
                name: name.context.tmux_session.clone(),
                origin: Origin::Binding(name.seq),
            });
        }
        Ok(())
    }
}

pub(super) fn collect(
    channel: u64,
    provider: &ProviderKind,
    delivery: &[BindingEvent],
) -> Result<Snapshot, String> {
    if *provider != ProviderKind::Codex {
        return Ok(Snapshot::default());
    }
    let records = p5::records_strict(channel)
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("binding metadata history: {error:?}"))?;
    if records.iter().any(|event| {
        event.channel_id != channel
            || event.provider != provider.as_str()
            || event.tmux_session.trim().is_empty()
    }) {
        return Err("foreign or blank binding metadata".into());
    }
    let mut snapshot = Snapshot {
        records,
        name: None,
    };
    let Some(event) = snapshot
        .records
        .iter()
        .rev()
        .find(|event| !matches!(event.new, p5::BindingTarget::Rejected { .. }))
    else {
        return Ok(snapshot);
    };
    let source = match &event.new {
        p5::BindingTarget::Source(source) | p5::BindingTarget::Resolved { source, .. } => source,
        _ => return Ok(snapshot),
    };
    let mapped = delivery
        .iter()
        .find(|mapped| mapped.seq == event.seq)
        .ok_or("binding metadata changed during observation")?;
    if mapped.channel_id != event.channel_id
        || mapped.provider != ShadowProvider::Codex
        || mapped.tmux_session != event.tmux_session
        || mapped.execution_nonce != event.execution_nonce.as_deref().unwrap_or_default()
    {
        return Err("binding metadata delivery identity mismatch".into());
    }
    if !matches!(mapped.record, BindingRecord::Rejected { .. }) {
        return Ok(snapshot);
    }
    let nonce = event
        .execution_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty())
        .ok_or("binding metadata has no execution nonce")?;
    let context = p5::codex::context_for_nonce(channel, &event.tmux_session, Some(nonce))
        .map_err(|error| error.to_string())?
        .ok_or("binding metadata ownership context missing")?;
    if context.schema != 1
        || context.provider != provider.as_str()
        || context.channel_id != Some(channel)
        || context.tmux_session != event.tmux_session
        || context.execution_nonce != nonce
        || context.source_policy.as_deref() != Some("verified")
        || context.owner_runtime_root != tmux_common::current_tmux_owner_marker()
        || !context
            .host
            .as_deref()
            .is_some_and(|host| !host.trim().is_empty())
        || context.host != binding_context::stable_host_identity()
    {
        return Err("binding metadata ownership identity mismatch".into());
    }
    let latest = p5::codex::latest_ownership(channel, &event.tmux_session)
        .map_err(|error| error.to_string())?
        .ok_or("binding metadata latest ownership missing")?;
    if latest.1 != nonce
        || snapshot
            .records
            .last()
            .is_none_or(|last| latest.0 > last.seq)
    {
        return Err("binding metadata ownership changed".into());
    }
    let proof = p5::codex::proof_at_seq(&context, event.seq)
        .map_err(|error| error.to_string())?
        .ok_or("binding metadata source has no ownership proof")?;
    let fold = p5::codex::read_ownership(&context).map_err(|error| error.to_string())?;
    if proof.source != *source
        || proof.ownership.context != context
        || fold.conflicted
        || !fold.pending.is_empty()
        || fold
            .verified
            .as_ref()
            .is_none_or(|current| current != &proof)
    {
        return Err("binding metadata source identity conflicts".into());
    }
    #[cfg(test)]
    if std::env::var("AGENTDESK_N2B_TEST_MUTATION").is_ok_and(|mutation| {
        matches!(
            mutation.as_str(),
            "joint-metadata-off" | "joint-metadata-missing"
        )
    }) {
        return Ok(snapshot);
    }
    snapshot.name = Some(Name {
        seq: event.seq,
        context,
    });
    Ok(snapshot)
}

#[cfg(test)]
#[path = "binding_metadata_tests.rs"]
mod tests;
