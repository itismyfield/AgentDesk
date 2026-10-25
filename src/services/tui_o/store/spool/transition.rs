//! The store side of an operator's transition cleanup: one cursor write that retires a source with
//! its audit, and the reads its judgment takes without turning a failed read into a default.

use std::collections::BTreeMap;

use super::{
    ChannelStore, Cursor, StoreError, TransitionCleanup, rejected, source_key, write_cursor,
};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::cleanup_witness::CleanupWitness;
use crate::services::tui_o::store::rotation::Rotation;

/// Each read as it came back; an error stays an error.
pub(crate) struct TransitionFacts {
    pub rotation: Result<Rotation, String>,
    pub checkpoint: Result<Option<u64>, String>,
    /// By spool key.
    pub witnesses: BTreeMap<String, Result<Option<CleanupWitness>, String>>,
}

impl ChannelStore {
    /// Retires `expected` with `audit` in a single cursor write, only while the cursor is still
    /// exactly `expected` and the audit verified its offset and hash.
    pub(crate) fn retire_transition(
        &mut self,
        expected: &Cursor,
        audit: TransitionCleanup,
        provider: ShadowProvider,
    ) -> Result<(), StoreError> {
        #[cfg(test)]
        if crate::services::tui_o::writer::actor::codex_retention::mutant("skip_store_write") {
            return Ok(());
        }
        self.mutate(|store| {
            let retention_valid = match provider {
                ShadowProvider::Claude => audit.retention.is_none(),
                ShadowProvider::Codex => audit
                    .retention
                    .as_ref()
                    .is_some_and(|evidence| evidence.valid(expected, &audit, store.init.channel)),
            };
            if !retention_valid {
                return Err(rejected(
                    "transition retention evidence is absent or invalid",
                ));
            }
            let key = source_key(&expected.source);
            let spool =
                (store.sources.get_mut(&key)).ok_or_else(|| rejected("source not attached"))?;
            let verified = &audit.verified;
            if spool.cursor != *expected
                || expected.retired
                || verified.offset != expected.captured_through
                || verified.prefix_hash != expected.prefix_hash
            {
                return Err(rejected("transition cleanup cursor changed"));
            }
            let cursor = Cursor {
                retired: true,
                transition_cleanup: Some(audit),
                ..expected.clone()
            };
            write_cursor(&store.dir, &key, &cursor)?;
            spool.cursor = cursor;
            Ok(())
        })
    }

    /// Reads nothing once an I/O error latched the store, as its writes are refused then too.
    pub(crate) fn read_transition_facts<'a>(
        &self,
        sources: impl IntoIterator<Item = &'a SourceId>,
    ) -> TransitionFacts {
        let latched = match self.io_failure() {
            Some(kind) => Err(format!(
                "the store refuses writes after an I/O error: {kind:?}"
            )),
            None => Ok(()),
        };
        let read = |error: StoreError| format!("{error:?}");
        let witnesses = (sources.into_iter())
            .map(|source| {
                let witness = latched
                    .clone()
                    .and_then(|()| self.recorded_witness(source).map_err(read));
                (source_key(source), witness)
            })
            .collect();
        TransitionFacts {
            rotation: latched.clone().and_then(|()| self.rotation().map_err(read)),
            checkpoint: latched.and_then(|()| self.binding_checkpoint().map_err(read)),
            witnesses,
        }
    }
}
