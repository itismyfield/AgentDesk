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

#[cfg(test)]
pub(super) struct ReceiptAdapter<K> {
    provider: QueuedProvider,
    history: crate::services::tui_input::human_frame::history::v2::Registry,
    namespace: String,
    key: K,
    encoded: serde_json::Value,
}
#[cfg(test)]
impl QueuedProvider {
    pub(super) fn receipt_adapter<K: serde::Serialize + Clone>(
        self,
        history: &crate::services::tui_input::human_frame::history::v2::Registry,
        namespace: &str,
        key: &K,
    ) -> Result<ReceiptAdapter<K>, String> {
        let encoded = serde_json::to_value(key).map_err(|e| e.to_string())?;
        // Legacy Claude reconcile does not yet carry the full receipt identity.
        if self != Self::Codex || encoded["attempt"]["provider"] != "codex" {
            return Err("exact provider receipt unavailable".into());
        }
        history.source(namespace, &encoded)?.validate(&encoded)?;
        Ok(ReceiptAdapter {
            provider: self,
            history: history.clone(),
            namespace: namespace.into(),
            key: key.clone(),
            encoded,
        })
    }
}
#[cfg(test)]
impl<K: Clone> ReceiptAdapter<K> {
    pub(super) fn run(
        &self,
        pane: (&busy_inject::Pane, Option<&HerdrTarget>),
        target: (&str, &Path, Option<&str>),
        turn: Option<String>,
        message: MessageId,
        input: &Input,
    ) -> Result<(InjectAttempt, K), String> {
        crate::services::tui_input::human_frame::history::v2::with_attempt(
            &self.history,
            &self.namespace,
            &self.encoded,
            || {
                (
                    self.provider.run(pane, target, turn, message, input),
                    self.key.clone(),
                )
            },
        )
    }
    pub(super) fn proof(&self, bound: &Bound) -> Option<Proof> {
        let source = self.history.source(&self.namespace, &self.encoded).ok()?;
        let recorded = Recorded {
            nonce: self.encoded["attempt"]["nonce"].as_str()?.into(),
            transcript: source.transcript.display().to_string(),
            session_id: source.session_id,
            file: (source.file[0], source.file[1]),
            offset: source.offset,
        };
        if !recorded.matches(bound) || source.target["owner"] != bound.session {
            return None;
        }
        crate::services::tui_input::human_frame::history::v2::with_attempt(
            &self.history,
            &self.namespace,
            &self.encoded,
            || super::codex_inject::submitted(&bound.path, recorded.offset, &recorded.nonce),
        )
        .ok()??;
        Some(Proof {
            nonce: recorded.nonce,
            session: bound.session_id.clone(),
            offset: recorded.offset,
        })
    }
}

#[cfg(all(test, unix))]
#[path = "queued_provider/history_pg_tests.rs"]
mod history_pg_tests;
