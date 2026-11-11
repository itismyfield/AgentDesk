#![cfg(unix)]

use crate::db::intake_outbox::IntakeOutboxRow;

/// Owned evidence issued inside the worker scope and consumed before effects.
pub(crate) struct BeforeEffects(IntakeOutboxRow);

impl BeforeEffects {
    pub(crate) fn row(&self) -> &IntakeOutboxRow {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(row: &IntakeOutboxRow) -> Self {
        Self(row.clone())
    }
}

#[path = "services/cluster/intake_worker/consent_scope.rs"]
pub(crate) mod issuer;
