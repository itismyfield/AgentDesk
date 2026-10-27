//! Legacy array or V1 envelope to V2. Runs only from adoption, which names the namespace and
//! publishes through the commit that compares the raw bytes it read.
use super::*;
use crate::services::turn_orchestrator::queued_head::SnapshotMeta;

/// Settle time of a legacy terminal identity; the first tick anchors its 24h window.
pub(super) const UNANCHORED: u64 = u64::MAX;

pub(super) fn legacy(items: Vec<PendingQueueItem>, meta: Option<Box<SnapshotMeta>>) -> SnapshotV2 {
    // The old record names no provider or target, so its attempt is kept as written, not rebuilt.
    let head = meta
        .as_ref()
        .and_then(|meta| meta.head_injection.as_ref())
        .filter(|head| head.phase != HeadPhase::Reserved)
        .map(|head| InputAttempt {
            identity: head.identity.clone(),
            original: items
                .first()
                .cloned()
                .unwrap_or_else(|| head.original.clone()),
            phase: HeadPhase::Unknown,
            attempt: None,
            payload: None,
            confirmation_deadline_ms: None,
            resolved_source: None,
            payload_window: None,
            resolved_target: None,
            preparation_hold: None,
            legacy_attempt: head.attempt.clone(),
        });
    SnapshotV2 {
        version: 2,
        admission_receipts: vec![],
        admission_sequence: 0,
        items,
        head_injection: head,
        retained_inputs: vec![],
        terminal_identities: meta
            .map(|meta| {
                meta.terminal_identities
                    .into_iter()
                    .map(|identity| TerminalIdentity {
                        identity,
                        settled_at_ms: UNANCHORED,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        namespace: None,
    }
}

/// Starts the retention window of terminals restored without a settle time.
pub(super) fn anchor_terminals(snapshot: &mut SnapshotV2, now: u64) {
    for terminal in &mut snapshot.terminal_identities {
        if terminal.settled_at_ms == UNANCHORED {
            terminal.settled_at_ms = now;
        }
    }
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
