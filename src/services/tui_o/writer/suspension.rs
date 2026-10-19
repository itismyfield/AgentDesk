//! One actor's monotone selection, shared with its strong-consumer ports.

use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::spool::source_key;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Default)]
struct Scope {
    nonce: Option<String>,
    keys: HashSet<String>,
}

#[derive(Clone, Default)]
pub(crate) struct Suspension(Arc<Mutex<Scope>>);

impl std::fmt::Debug for Suspension {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Suspension")
            .field(
                "count",
                &self
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .keys
                    .len(),
            )
            .finish_non_exhaustive()
    }
}

impl Suspension {
    pub(crate) fn contains(&self, source: &SourceId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys
            .contains(&source_key(source))
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys
            .is_empty()
    }
    pub(crate) fn nonce(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .nonce
            .clone()
    }
    pub(super) fn extend(&self, nonce: String, sources: impl IntoIterator<Item = SourceId>) {
        let mut scope = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        scope
            .keys
            .extend(sources.into_iter().map(|source| source_key(&source)));
        scope.nonce = Some(nonce);
    }
}
