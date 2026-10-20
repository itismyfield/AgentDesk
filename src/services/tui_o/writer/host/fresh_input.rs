//! Placement may precede restoration only for a real, owned and running writer.

use super::*;

pub(crate) fn initial_fresh_refusal(channel: u64) -> Option<&'static str> {
    let readiness = process_readiness();
    let live = locked(&readiness.live);
    let Some(live) = live.get(&channel) else {
        return Some("host_missing");
    };
    let owned = owned_now(channel, &live.gate);
    #[cfg(test)]
    let owned = owned || crate::services::claude_tui::source_verify::n2b_mutant("b1b-host");
    if !owned {
        return Some("ownership_not_owned");
    }
    if *live.stopping.borrow() {
        return Some("stopping");
    }
    if live.resumed.has_changed().is_err() {
        return Some("actor_closed");
    }
    if live.submission.is_none() {
        return Some("writer_sources_unready");
    }
    None
}

pub(super) fn hold_metadata(store: &ChannelStore) -> Option<&'static str> {
    #[cfg(test)]
    if crate::services::claude_tui::source_verify::n2b_mutant("b1b-meta") {
        return None;
    }
    if !crate::services::tui_prompt_dedupe::clear_relaunch::fresh_scope_eligible(
        store.init().channel,
    ) {
        return None;
    }
    let selection = super::super::suspension::Suspension::default();
    let restored = match selection.restore_current(store) {
        Ok(restored) => restored,
        Err(_) => return Some("store_selection_unready"),
    };
    let ordinary = super::super::deliver::source_delivery_allowed(
        store.init().channel,
        store
            .cursors()
            .filter(|cursor| {
                let suspended = selection.contains(&cursor.source);
                #[cfg(test)]
                let suspended = suspended
                    && !crate::services::claude_tui::source_verify::n2b_mutant(
                        "b1b-boot-scope-subtract",
                    );
                !cursor.retired && !suspended
            })
            .map(|cursor| cursor.source.clone()),
    );
    #[cfg(test)]
    let ordinary =
        ordinary && !crate::services::claude_tui::source_verify::n2b_mutant("b1b-boot-ordinary");
    if ordinary {
        return None;
    }
    if restored {
        return Some("store_selection_conflict");
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
