//! Source-specific retention claims; Complete observation alone never grants retirement.
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::spool::{
    ExecutionEnd, RetentionBasisV1, RetentionClaimV1, source_key,
};
use crate::services::tui_o::writer::codex_snapshot::{Observation, Snapshot};
use crate::services::tui_o::writer::transition_cleanup::scope::codex::identity;
use crate::services::tui_prompt_dedupe::binding_context;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingEvent, BindingTarget,
    codex::{SettledCompanions, observe_channel},
};

pub(crate) fn for_hop(
    observed: &Snapshot,
    key: &str,
    old: &SourceId,
    seq: u64,
    pane: &str,
) -> Result<RetentionClaimV1, &'static str> {
    let log = observe_channel(observed.target.channel, 65_536)
        .map_err(|_| "codex_retention_log_unavailable")?;
    if log.tail != observed.tail || log.digest != observed.log_digest {
        return Err("codex_retention_log_changed");
    }
    claim(
        observed,
        key,
        old,
        seq,
        pane,
        &log.events,
        &log.settled_companions,
    )
}

fn row(events: &[BindingEvent], seq: u64) -> Result<&BindingEvent, &'static str> {
    let mut matching = events.iter().filter(|event| event.seq == seq);
    let event = matching.next().ok_or("codex_retention_binding_missing")?;
    if matching.next().is_some() {
        return Err("codex_retention_binding_ambiguous");
    }
    Ok(event)
}

fn resolved<'a>(
    events: &'a [BindingEvent],
    event: &'a BindingEvent,
) -> Result<(&'a SourceId, &'a BindingEvent), &'static str> {
    match &event.new {
        BindingTarget::Source(source) => Ok((source, event)),
        BindingTarget::Pending {
            payload_session_id,
            payload_transcript_path,
        } => {
            let mut matches = events.iter().filter_map(|later| match &later.new {
                BindingTarget::Resolved {
                    pending_seq,
                    source,
                } if *pending_seq == event.seq => Some((source, later)),
                _ => None,
            });
            let (source, later) = matches.next().ok_or("codex_retention_pending")?;
            if matches.next().is_some()
                || later.seq <= event.seq
                || later.tmux_session != event.tmux_session
                || later.execution_nonce != event.execution_nonce
                || source.session_id != *payload_session_id
                || payload_transcript_path
                    .as_ref()
                    .is_some_and(|path| std::path::Path::new(path) != source.path)
            {
                return Err("codex_retention_resolution_ambiguous");
            }
            Ok((source, later))
        }
        _ => Err("codex_retention_not_a_binding"),
    }
}

fn claim(
    observed: &Snapshot,
    key: &str,
    old: &SourceId,
    seq: u64,
    pane: &str,
    events: &[BindingEvent],
    settled: &SettledCompanions,
) -> Result<RetentionClaimV1, &'static str> {
    let target = &observed.target;
    if key != source_key(old) || observed.held.iter().any(|held| overlaps(old, held)) {
        return Err("codex_retention_source_held");
    }
    if events.len() as u64 != observed.tail
        || events.iter().enumerate().any(|(index, event)| {
            event.seq != index as u64 + 1
                || event.channel_id != target.channel
                || event.provider != "codex"
        })
    {
        return Err("codex_retention_log_incomplete");
    }
    let hop = row(events, seq)?;
    if hop.old.as_ref() != Some(old) || hop.tmux_session != pane {
        return Err("codex_retention_edge_mismatch");
    }
    let (successor, _) = resolved(events, hop)?;
    if successor == old {
        return Err("codex_retention_edge_mismatch");
    }
    let mut bindings = Vec::new();
    for event in events {
        let companion = settled.contains(event.seq);
        #[cfg(test)]
        let companion = companion && !mutant("companion_unwired");
        if companion {
            continue;
        }
        if matches!(
            event.new,
            BindingTarget::Source(_) | BindingTarget::Pending { .. }
        ) {
            let (source, binder) = resolved(events, event)?;
            if source == old {
                if event.tmux_session != pane {
                    return Err("codex_retention_binding_ambiguous");
                }
                bindings.push((event, binder));
            }
        }
    }
    let (binding, binder) = bindings
        .into_iter()
        .max_by_key(|(binding, _)| binding.seq)
        .ok_or("codex_retention_binding_missing")?;
    let nonce = binding
        .execution_nonce
        .as_deref()
        .filter(|nonce| !nonce.trim().is_empty())
        .ok_or("codex_retention_nonce_unknown")?;
    if binder.seq >= seq {
        return Err("codex_retention_source_revisited");
    }
    let (basis, detail) = if pane == target.tmux {
        if nonce == target.nonce {
            return Err("codex_retention_current_execution");
        }
        (
            RetentionBasisV1::SamePane,
            format!(
                "last binder {} superseded by current nonce {}",
                binder.seq, target.nonce
            ),
        )
    } else {
        let digest = historical(observed, pane, nonce)?;
        (
            RetentionBasisV1::LocalHistorical {
                context_digest: digest,
            },
            "exact local historical execution is dead or absent".into(),
        )
    };
    let (binding_seq, binder_nonce) = (binding.seq, nonce.to_owned());
    #[cfg(test)]
    let binding_seq = if mutant("binder_seq") {
        binder.seq
    } else {
        binding_seq
    };
    #[cfg(test)]
    let binder_nonce = if mutant("hop_nonce") {
        hop.execution_nonce.clone().unwrap_or_default()
    } else {
        binder_nonce
    };
    Ok(RetentionClaimV1 {
        version: 1,
        provider: ShadowProvider::Codex,
        channel: target.channel,
        old: old.clone(),
        hop_seq: seq,
        binding_seq,
        binder_seq: binder.seq,
        binder_nonce,
        successor: successor.clone(),
        observation: identity(&Observation::Complete(Box::new(observed.clone())))
            .map_err(|_| "codex_retention_observation_unknown")?,
        end: ExecutionEnd {
            host: target.host.clone(),
            pane: pane.to_owned(),
            first_seq: seq,
            last_seq: seq,
            basis: detail,
        },
        basis,
    })
}

fn overlaps(a: &SourceId, b: &SourceId) -> bool {
    a.session_id == b.session_id || a.path == b.path || (a.dev, a.ino) == (b.dev, b.ino)
}

pub(crate) fn historical(
    observed: &Snapshot,
    pane: &str,
    nonce: &str,
) -> Result<String, &'static str> {
    let old = binding_context::execution_context("codex", nonce)
        .map_err(|_| "codex_retention_old_context_unavailable")?;
    let mismatch = old.execution_nonce != nonce
        || old.tmux_session != pane
        || old.channel_id != Some(observed.target.channel)
        || old.host.as_deref() != Some(observed.target.host.as_str())
        || old.owner_runtime_root != observed.target.owner_runtime_root
        || !matches!(old.source_policy.as_deref(), Some("shadow" | "verified"));
    #[cfg(test)]
    let mismatch = mismatch && !mutant("historical_scope");
    if mismatch {
        return Err("codex_retention_old_context_not_local");
    }
    use crate::services::session_host::HostLiveness;
    let liveness = crate::services::discord::host_liveness::historical_execution_liveness(pane)
        .ok_or("codex_retention_historical_not_local")?;
    if liveness != HostLiveness::DeadOrAbsent {
        return Err("codex_retention_historical_not_ended");
    }
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&old).map_err(|_| "codex_retention_old_context_unavailable")?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
pub(crate) fn mutant(name: &str) -> bool {
    std::env::var("AGENTDESK_BA_MUTANT").is_ok_and(|value| value == name)
}

#[cfg(all(test, unix))]
#[path = "codex_retention_tests.rs"]
mod tests;
