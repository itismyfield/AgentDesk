//! Read-modify-replace of the sidecar, only through the channel actor's one store.

use super::super::{ChannelStore, StoreError, durable};
use super::submit_floor::{DurableClosedEpisode, Schema2, SubmitFloor};
use super::{CODEX_PROVENANCE_FILE, CodexEpisodeDeny, CodexEpisodeSpan, CodexProvenance, SCHEMA};
use super::{Sidecar, retained, valid};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommitStage {
    Renamed,
    ParentSynced,
    ReadBack,
}

type CommitHook = Box<dyn FnMut(CommitStage) -> std::io::Result<()>>;
thread_local! {
    pub(super) static COMMIT_HOOK: std::cell::RefCell<Option<CommitHook>> = const { std::cell::RefCell::new(None) };
}

fn stage(stage: CommitStage) -> std::io::Result<()> {
    COMMIT_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(stage),
        None => Ok(()),
    })
}

fn sync_parent(path: &std::path::Path) -> std::io::Result<()> {
    stage(CommitStage::Renamed)?;
    crate::services::discord::runtime_store::fsync_parent_dir(path)
}

impl ChannelStore {
    pub(crate) fn prepare_codex_floor(&mut self, floor: SubmitFloor) -> Result<(), StoreError> {
        let current = self.codex_provenance();
        if let Sidecar::Present(read) = &current
            && read.1.is_none()
        {
            let raw =
                durable::read_json::<serde_json::Value>(&self.dir.join(CODEX_PROVENANCE_FILE))?
                    .ok_or_else(|| StoreError::Rejected("Codex provenance changed".into()))?;
            if !super::submit_floor::known_fields(&raw, &serde_json::to_value(&read.0)?) {
                return Err(StoreError::Rejected(
                    "unknown Codex provenance field".into(),
                ));
            }
        }
        let create = matches!(current, Sidecar::Missing);
        let mut next = match current {
            Sidecar::Missing => Schema2 {
                schema: 2,
                channel: self.init.channel,
                spans: Vec::new(),
                denies: Vec::new(),
                floors: Vec::new(),
            },
            Sidecar::Present(read) => read.1.unwrap_or(Schema2 {
                schema: 2,
                channel: read.0.channel,
                spans: read.0.spans,
                denies: read.0.denies,
                floors: Vec::new(),
            }),
            Sidecar::Unavailable(_) => {
                return Err(StoreError::Rejected("Codex provenance unavailable".into()));
            }
        };
        match next.floors.iter_mut().find(|old| floor.same_episode(old)) {
            Some(old) if *old == floor => {}
            Some(old) if floor.retains(old) => *old = floor,
            Some(_) => return Err(StoreError::Rejected("conflicting submit floor".into())),
            None => next.floors.push(floor),
        }
        self.publish_codex_floor(next, create).map(|_| ())
    }

    pub(crate) fn close_codex_episode(
        &mut self,
        span: CodexEpisodeSpan,
    ) -> Result<DurableClosedEpisode, StoreError> {
        let Sidecar::Present(read) = self.codex_provenance() else {
            return Err(StoreError::Rejected("Codex provenance unavailable".into()));
        };
        let mut next = read
            .1
            .ok_or_else(|| StoreError::Rejected("native floor unavailable".into()))?;
        if next.denies.iter().any(|deny| {
            deny.execution == span.execution
                && (
                    deny.episode.channel_id,
                    deny.episode.user_message_id,
                    &deny.episode.turn_nonce,
                ) == (
                    span.episode.channel_id,
                    span.episode.user_message_id,
                    &span.episode.turn_nonce,
                )
        }) {
            return Err(StoreError::Rejected("episode denied".into()));
        }
        let floor = next
            .floors
            .iter()
            .find_map(|floor| match floor {
                SubmitFloor::NativeFloor(floor) if floor.names(&span) => Some(floor),
                _ => None,
            })
            .ok_or_else(|| StoreError::Rejected("native floor mismatch".into()))?;
        let verify = || -> Result<(), StoreError> {
            use crate::services::codex_tui::rollout_tail::provenance::{
                NativeTurnAnchor, terminal_end,
            };
            use crate::services::tui_o::shadow::capture::SourceCapture;
            use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource};
            let mut capture = SourceCapture::reopen(
                floor.canonical_source.clone(),
                floor.through,
                &floor.prefix_hash,
            )?;
            let end = span
                .end
                .ok_or_else(|| StoreError::Rejected("native terminal unavailable".into()))?;
            let anchor = NativeTurnAnchor {
                native_turn_id: span.episode.native_turn_id.clone(),
                start: span.start,
            };
            let terminal = terminal_end(&span.execution.source, &anchor, end);
            if !capture.verify_prefix()?
                || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
                || terminal != Ok(Some(end))
            {
                return Err(StoreError::Rejected(
                    "native terminal or prefix changed".into(),
                ));
            }
            Ok(())
        };
        verify()?;
        match next
            .spans
            .iter_mut()
            .find(|old| old.execution == span.execution && old.episode == span.episode)
        {
            Some(old) if *old == span => {}
            Some(old) => *old = span.clone(),
            None => next.spans.push(span.clone()),
        }
        self.publish_codex_floor(next, false)?;
        Ok(DurableClosedEpisode::new(span))
    }

    fn publish_codex_floor(
        &mut self,
        next: Schema2,
        create: bool,
    ) -> Result<super::ValidatedCodexProvenance, StoreError> {
        if !next.valid(self.init.channel) {
            return Err(StoreError::Rejected("conflicting submit provenance".into()));
        }
        match self.codex_provenance() {
            Sidecar::Present(current) => {
                if !retained(&current.0, &next.base())
                    || current.1.as_ref().is_some_and(|current| {
                        current
                            .floors
                            .iter()
                            .any(|old| !next.floors.iter().any(|floor| floor.retains(old)))
                    })
                {
                    return Err(StoreError::Rejected("submit provenance lost".into()));
                }
            }
            Sidecar::Missing if create => {}
            _ => return Err(StoreError::Rejected("submit provenance changed".into())),
        }
        let bytes = serde_json::to_vec(&next)?;
        let path = self.dir.join(CODEX_PROVENANCE_FILE);
        let temp = self.dir.join(format!(
            ".codex_provenance.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        super::SIDECAR_IO.with(|io| io.set((io.get().0, io.get().1 + 1)));
        self.mutate(|store| {
            durable::create_once(&temp, &bytes)?;
            std::fs::rename(&temp, &path)?;
            sync_parent(&path)?;
            stage(CommitStage::ParentSynced)?;
            stage(CommitStage::ReadBack)?;
            match store.codex_provenance() {
                Sidecar::Present(read) if read.1.as_ref() == Some(&next) => Ok(read),
                _ => Err(std::io::Error::other("Codex provenance read-back failed").into()),
            }
        })
    }

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
        super::SIDECAR_IO.with(|io| io.set((io.get().0, io.get().1 + 1)));
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
        let schema2 = current.1;
        let current = current.0;
        let mut next = current.clone();
        change(&mut next);
        if !retained(&current, &next) || !valid(self.init.channel, &next) {
            return Err(StoreError::Rejected(
                "conflicting Codex episode provenance".into(),
            ));
        }
        if let Some(mut document) = schema2 {
            (document.spans, document.denies) = (next.spans, next.denies);
            return self.publish_codex_floor(document, false).map(|_| ());
        }
        let bytes = serde_json::to_vec(&next)?;
        let path = self.dir.join(CODEX_PROVENANCE_FILE);
        super::SIDECAR_IO.with(|io| io.set((io.get().0, io.get().1 + 1)));
        self.mutate(|_| Ok(durable::replace(&path, &bytes)?))
    }
}
