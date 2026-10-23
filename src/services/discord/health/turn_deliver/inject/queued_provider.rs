//! Provider effects and native receipt selection for the durable queued-head owner.
use std::path::Path;

use super::queued_reconcile::{self, Bound, Proof, Recorded};
use super::*;
use crate::services::turn_orchestrator::PendingQueueItem;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum QueuedProvider {
    Claude,
    Codex,
}

impl QueuedProvider {
    pub(super) fn select(provider: &ProviderKind, channel: u64) -> Option<Self> {
        match provider {
            ProviderKind::Claude => Some(Self::Claude),
            ProviderKind::Codex if codex_enabled(channel) => Some(Self::Codex),
            _ => None,
        }
    }

    pub(super) fn runtime(self) -> RuntimeHandoffKind {
        match self {
            Self::Claude => RuntimeHandoffKind::ClaudeTui,
            Self::Codex => RuntimeHandoffKind::CodexTui,
        }
    }

    pub(super) fn session(self, path: &Path, bound: Option<&str>) -> Option<String> {
        match self {
            Self::Claude => queued_reconcile::transcript_session(path, bound),
            Self::Codex => {
                let meta =
                    crate::services::codex_tui::rollout_index::read_rollout_session_meta(path)?;
                let id = meta.id.filter(|id| !id.trim().is_empty())?;
                bound.is_none_or(|bound| bound == id).then_some(id)
            }
        }
    }

    pub(super) fn run(
        self,
        pane: (&busy_inject::Pane, Option<&HerdrTarget>),
        (session, transcript, native_turn): (&str, &Path, Option<&str>),
        turn_id: Option<String>,
        message: MessageId,
        input: &Input,
    ) -> InjectAttempt {
        if let Some(host) = pane.1 {
            return self.run_herdr(host, session, transcript, turn_id, input);
        }
        match self {
            Self::Claude => super::run(pane, session, transcript, turn_id, input),
            Self::Codex => match native_turn {
                Some(turn) => run_codex(
                    pane.0,
                    (session, transcript, turn),
                    turn_id,
                    Some(message),
                    input,
                ),
                None => InjectAttempt::NotSent("turn_unknown"),
            },
        }
    }

    fn run_herdr(
        self,
        host: &HerdrTarget,
        session: &str,
        transcript: &Path,
        turn_id: Option<String>,
        input: &Input,
    ) -> InjectAttempt {
        match self {
            Self::Claude => super::run(
                (&pane(input.channel, session), Some(host)),
                session,
                transcript,
                turn_id,
                input,
            ),
            Self::Codex => InjectAttempt::NotSent("provider_unsupported"),
        }
    }

    pub(super) fn proof(
        self,
        channel: ChannelId,
        identity: &QueueIdentity,
        original: &PendingQueueItem,
        recorded: Option<Recorded>,
        bound: &Bound,
    ) -> Option<Proof> {
        match self {
            Self::Claude => {
                queued_reconcile::claude_proof(channel, identity, original, recorded, bound)
            }
            Self::Codex => {
                let recorded = recorded.filter(|attempt| attempt.matches(bound))?;
                codex_inject::submitted(&bound.path, recorded.offset, &recorded.nonce)?;
                Some(Proof {
                    nonce: recorded.nonce,
                    session: bound.session_id.clone(),
                    offset: recorded.offset,
                })
            }
        }
    }
}
