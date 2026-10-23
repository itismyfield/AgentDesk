//! Execution-end evidence for one historical hop.

use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::store::spool::ExecutionEnd;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingRecord};

pub(crate) fn for_hop(
    channel: u64,
    logical_key: &str,
    current_nonce: &str,
    hop_seq: u64,
    pane: &str,
    events: &[BindingEvent],
) -> Result<ExecutionEnd, &'static str> {
    let basis = if pane == logical_key {
        let mut matching = events.iter().filter(|event| event.seq == hop_seq);
        let event = matching.next().ok_or("binding_event_missing")?;
        if matching.next().is_some()
            || event.channel_id != channel
            || event.provider != ShadowProvider::Claude
            || event.tmux_session != pane
            || !matches!(event.record, BindingRecord::Bound { .. })
        {
            return Err("binding_event_unknown");
        }
        if current_nonce.trim().is_empty() || event.execution_nonce.trim().is_empty() {
            return Err("execution_nonce_unknown");
        }
        let different = event.execution_nonce != current_nonce;
        #[cfg(test)]
        let different = match std::env::var("AGENTDESK_OTRANSITION_MUTANT").as_deref() {
            Ok("nonce_same") => false,
            Ok("nonce_different") => true,
            _ => different,
        };
        if !different {
            return Err("current_execution");
        }
        format!("binding {hop_seq} superseded by Bound execution {current_nonce}")
    } else {
        if !crate::services::discord::host_liveness::historical_execution_ended(pane) {
            return Err("historical_execution_not_ended");
        }
        "known historical host reports execution dead or absent".into()
    };
    Ok(ExecutionEnd {
        host: crate::config::session_hosts::local_node().ok_or("local_node_unknown")?,
        pane: pane.to_owned(),
        first_seq: hop_seq,
        last_seq: hop_seq,
        basis,
    })
}
