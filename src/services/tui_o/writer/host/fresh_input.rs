//! Placement may precede restoration only for a real, owned and running writer.

use super::*;

pub(crate) fn initial_fresh_ready(channel: u64) -> bool {
    let readiness = process_readiness();
    let live = locked(&readiness.live);
    live.get(&channel).is_some_and(|live| {
        live.submission.is_some()
            && owned_now(channel, &live.gate)
            && !*live.stopping.borrow()
            && live.resumed.has_changed().is_ok()
    })
}

pub(super) fn hold_metadata(store: &ChannelStore) -> Option<&'static str> {
    if !crate::services::tui_prompt_dedupe::clear_relaunch::fresh_scope_eligible(
        store.init().channel,
    ) {
        return None;
    }
    if store.cursors().next().is_some() && !matches!(store.binding_checkpoint(), Ok(Some(_))) {
        return Some("store_metadata_unready");
    }
    if store.ledger().unresolved().is_some()
        || super::super::local_retry::pending(store.ledger()).is_some()
        || store.ledger().resume_pieces().next().is_some()
    {
        return Some("unresolved_work");
    }
    None
}

#[cfg(test)]
#[path = "fresh_input_tests.rs"]
mod tests;
