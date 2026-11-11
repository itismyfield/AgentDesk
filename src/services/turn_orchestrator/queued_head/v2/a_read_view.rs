//! Read-only marker cleanup debt shares the owner's queue identity and history.
use super::super::QueueIdentity;
use super::super::v2_types::{AdmissionBind, BindAt, SnapshotV2};

impl SnapshotV2 {
    pub(crate) fn has_control_debt(&self) -> bool {
        !self.a_control.is_empty()
    }

    pub(crate) fn control_identities(&self) -> impl Iterator<Item = &QueueIdentity> {
        self.a_control.iter().map(|control| &control.identity)
    }

    pub(crate) fn control_references_identity(&self, identity: &QueueIdentity) -> bool {
        self.control_identities().any(|old| old == identity)
    }

    pub(crate) fn control_references_bind(&self, bind: &AdmissionBind) -> bool {
        self.a_control.iter().any(|control| {
            control.front_receipt.as_deref() == Some(bind.receipt.as_str())
                || match &bind.at {
                    BindAt::Attempt { identity, attempt } => {
                        &control.identity == identity && control.attempt.as_ref() == Some(attempt)
                    }
                    BindAt::Settled { attempt, .. } => control
                        .attempt
                        .as_ref()
                        .is_some_and(|old| old.nonce == *attempt),
                    BindAt::Queued { .. } | BindAt::Opaque => false,
                }
        })
    }

    pub(crate) fn control_pinned_receipts(&self) -> Vec<String> {
        self.admission_binds
            .iter()
            .filter(|bind| self.control_references_bind(bind))
            .map(|bind| bind.receipt.clone())
            .collect()
    }

    pub(crate) fn expire_terminals(&mut self, now: u64) {
        let pinned: Vec<_> = self
            .terminal_identities
            .iter()
            .filter(|terminal| self.control_references_identity(&terminal.identity))
            .map(|terminal| terminal.identity.clone())
            .collect();
        self.terminal_identities.retain(|terminal| {
            now.saturating_sub(terminal.settled_at_ms) < 86_400_000
                || pinned.contains(&terminal.identity)
        });
    }

    pub(crate) fn prune_unpinned_binds(&mut self) {
        let pinned = self.control_pinned_receipts();
        let len = self.items.len();
        self.admission_binds.retain(|bind| {
            !matches!(bind.at, BindAt::Queued { index, .. } if index >= len)
                || pinned.contains(&bind.receipt)
        });
    }

    pub(crate) fn expire_unpinned_binds(&mut self, now: u64) {
        let pinned = self.control_pinned_receipts();
        self.admission_binds.retain(|bind| {
            !matches!(bind.at, BindAt::Settled { settled_at_ms, .. }
                if now.saturating_sub(settled_at_ms) >= 86_400_000)
                || pinned.contains(&bind.receipt)
        });
    }
}

#[cfg(test)]
#[path = "a_read_view_tests.rs"]
mod tests;
