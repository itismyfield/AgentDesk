//! Execution-end evidence for one historical source.

use std::collections::BTreeSet;

use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::spool::ExecutionEnd;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingRecord, BindingTarget};

pub(crate) struct Execution<'a> {
    pub channel: u64,
    pub logical_key: &'a str,
    pub nonce: &'a str,
    pub host: &'a str,
    pub ended_panes: &'a BTreeSet<String>,
}

fn unique(events: &[BindingEvent], seq: u64) -> Result<&BindingEvent, &'static str> {
    let mut matching = events.iter().filter(|event| event.seq == seq);
    let event = matching.next().ok_or("binding_event_missing")?;
    if matching.next().is_some() {
        return Err("binding_event_unknown");
    }
    Ok(event)
}

fn belongs(event: &BindingEvent, execution: &Execution<'_>, pane: &str) -> bool {
    event.channel_id == execution.channel
        && event.provider == ShadowProvider::Claude
        && event.tmux_session == pane
}

fn binder<'a>(
    events: &'a [BindingEvent],
    old: &SourceId,
    hop_seq: u64,
    execution: &Execution<'_>,
    pane: &str,
) -> Result<&'a BindingEvent, &'static str> {
    let mut bindings = Vec::new();
    for event in events.iter().filter(|event| event.seq < hop_seq) {
        let binding = match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(source),
                ..
            } if source == old => event,
            BindingRecord::Resolved {
                resolves_seq,
                source,
            } if source == old => {
                let pending = unique(events, *resolves_seq)?;
                let resolutions = events
                    .iter()
                    .filter(|row| {
                        matches!(&row.record,
                    BindingRecord::Resolved { resolves_seq: seq, .. } if seq == resolves_seq)
                    })
                    .count();
                if resolutions != 1
                    || pending.seq >= event.seq
                    || !matches!(
                        pending.record,
                        BindingRecord::Bound {
                            new: BindingTarget::Pending { .. },
                            ..
                        }
                    )
                {
                    return Err("binding_event_unknown");
                }
                pending
            }
            _ => continue,
        };
        if !belongs(event, execution, pane) || !belongs(binding, execution, pane) {
            return Err("binding_event_unknown");
        }
        bindings.push(binding);
    }
    // Resolution names the source; its Pending bind owns the execution and sequence.
    let last = bindings
        .iter()
        .map(|event| event.seq)
        .max()
        .ok_or("binding_event_missing")?;
    if bindings.iter().filter(|event| event.seq == last).count() != 1 {
        return Err("binding_event_unknown");
    }
    unique(events, last)
}

pub(crate) fn for_hop(
    execution: &Execution<'_>,
    hop_seq: u64,
    pane: &str,
    events: &[BindingEvent],
) -> Result<ExecutionEnd, &'static str> {
    let event = unique(events, hop_seq)?;
    let BindingRecord::Bound { old: Some(old), .. } = &event.record else {
        return Err("binding_event_unknown");
    };
    if !belongs(event, execution, pane) {
        return Err("binding_event_unknown");
    }
    let basis = if pane == execution.logical_key {
        let binding = binder(events, old, hop_seq, execution, pane)?;
        let nonce = binding.execution_nonce.as_str();
        #[cfg(test)]
        let nonce = if std::env::var("AGENTDESK_OTRANSITION_MUTANT").is_ok_and(|m| m == "nonce_hop")
        {
            event.execution_nonce.as_str()
        } else {
            nonce
        };
        if execution.nonce.trim().is_empty() || nonce.trim().is_empty() {
            return Err("execution_nonce_unknown");
        }
        let different = nonce != execution.nonce;
        #[cfg(test)]
        let different = match std::env::var("AGENTDESK_OTRANSITION_MUTANT").as_deref() {
            Ok("nonce_same") => false,
            Ok("nonce_different") => true,
            _ => different,
        };
        if !different {
            return Err("current_execution");
        }
        format!(
            "source bound at {} superseded by Bound execution {}",
            binding.seq, execution.nonce
        )
    } else {
        if !execution.ended_panes.contains(pane) {
            return Err("historical_execution_not_ended");
        }
        "known historical host reports execution dead or absent".into()
    };
    if execution.host.trim().is_empty() {
        return Err("local_node_unknown");
    }
    Ok(ExecutionEnd {
        host: execution.host.to_owned(),
        pane: pane.to_owned(),
        first_seq: hop_seq,
        last_seq: hop_seq,
        basis,
    })
}
