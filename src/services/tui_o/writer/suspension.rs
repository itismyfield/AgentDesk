//! A proven selection survives same-incarnation restarts and is shared with strong consumers.

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
    pub(crate) fn restore_current(
        &self,
        store: &crate::services::tui_o::store::ChannelStore,
    ) -> Result<bool, crate::services::tui_o::store::StoreError> {
        crate::services::tui_prompt_dedupe::clear_relaunch::with_current_scope_nonce(
            store.init().channel,
            |current| {
                let Some(nonce) = current else {
                    return Ok(false);
                };
                let Some(record) = store.fresh_selection()? else {
                    return Ok(false);
                };
                let matches = record.nonce == nonce;
                #[cfg(test)]
                let matches = matches
                    || crate::services::claude_tui::source_verify::n2b_mutant(
                        "b1b-selection-nonce",
                    );
                if !matches {
                    return Ok(false);
                }
                let mut scope = self.0.lock().unwrap_or_else(PoisonError::into_inner);
                scope.keys.extend(record.keys);
                scope.nonce = Some(nonce.to_owned());
                Ok(true)
            },
        )
    }

    pub(super) fn install(
        &self,
        store: &mut crate::services::tui_o::store::ChannelStore,
        nonce: String,
        sources: Vec<SourceId>,
    ) -> Result<(), crate::services::tui_o::store::StoreError> {
        let mut scope = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut keys = scope.keys.clone();
        keys.extend(sources.iter().map(source_key));
        // Publish the proven selection before exposing it to this actor or its ports.
        store.write_fresh_selection(nonce.clone(), keys.iter().cloned().collect())?;
        scope.keys = keys;
        scope.nonce = Some(nonce);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn extend(&self, nonce: String, sources: impl IntoIterator<Item = SourceId>) {
        let mut scope = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        scope
            .keys
            .extend(sources.into_iter().map(|source| source_key(&source)));
        scope.nonce = Some(nonce);
    }
}
