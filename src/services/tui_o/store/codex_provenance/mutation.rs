//! Read-modify-replace of the sidecar, only through the channel actor's one store.

use super::super::{ChannelStore, StoreError, durable};
use super::{CODEX_PROVENANCE_FILE, CodexEpisodeDeny, CodexEpisodeSpan, CodexProvenance, SCHEMA};
use super::{Sidecar, retained, valid};

impl ChannelStore {
    /// Test seam for a new execution's empty sidecar; its real caller belongs to P4-2.
    pub(crate) fn init_codex_provenance(&mut self) -> Result<(), StoreError> {
        if self.codex_provenance() != Sidecar::Missing {
            return Err(StoreError::Rejected("codex provenance exists".into()));
        }
        let channel = self.init.channel;
        let (spans, denies) = (Vec::new(), Vec::new());
        let empty = CodexProvenance {
            schema: SCHEMA,
            channel,
            spans,
            denies,
        };
        let bytes = serde_json::to_vec(&empty)?;
        let path = self.dir.join(CODEX_PROVENANCE_FILE);
        self.mutate(|_| Ok(durable::create_once(&path, &bytes)?))
    }

    /// Publish the returned span only after its durable replace succeeds.
    pub(crate) fn persist_codex_span(
        &mut self,
        span: CodexEpisodeSpan,
    ) -> Result<CodexEpisodeSpan, StoreError> {
        self.change_codex_provenance(|next| {
            let stored = (next.spans.iter_mut()).find(|stored| {
                stored.execution == span.execution && stored.episode == span.episode
            });
            match stored {
                Some(stored) => *stored = span.clone(),
                None => next.spans.push(span.clone()),
            }
        })?;
        Ok(span)
    }

    pub(crate) fn persist_codex_deny(&mut self, deny: CodexEpisodeDeny) -> Result<(), StoreError> {
        self.change_codex_provenance(|next| {
            if !next.denies.contains(&deny) {
                next.denies.push(deny);
            }
        })
    }

    /// A conflict or an unreadable sidecar is Rejected and leaves the store writable; only a
    /// failed write marks it failed.
    pub(super) fn change_codex_provenance(
        &mut self,
        change: impl FnOnce(&mut CodexProvenance),
    ) -> Result<(), StoreError> {
        let Sidecar::Present(current) = self.codex_provenance() else {
            return Err(StoreError::Rejected("codex provenance unavailable".into()));
        };
        let mut next = current.clone();
        change(&mut next);
        if !retained(&current, &next) || !valid(self.init.channel, &next) {
            return Err(StoreError::Rejected(
                "conflicting Codex episode provenance".into(),
            ));
        }
        let bytes = serde_json::to_vec(&next)?;
        let path = self.dir.join(CODEX_PROVENANCE_FILE);
        self.mutate(|_| Ok(durable::replace(&path, &bytes)?))
    }
}
