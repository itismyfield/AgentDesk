//! Codex episode provenance in its own channel file. Only a validated snapshot is Present; a
//! missing, damaged or foreign file never reads as one without spans or denies.

use serde::{Deserialize, Serialize};

use super::{ChannelStore, durable};
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeDeny, CodexEpisodeSpan,
};

mod mutation;

pub(crate) const CODEX_PROVENANCE_FILE: &str = "codex_provenance";
const SCHEMA: u32 = 1;

/// A later submit-floor record comes with a new schema; this reader refuses unknown fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexProvenance {
    pub schema: u32,
    pub channel: u64,
    pub spans: Vec<CodexEpisodeSpan>,
    pub denies: Vec<CodexEpisodeDeny>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Sidecar {
    Missing,
    Present(CodexProvenance),
    Unavailable(&'static str),
}

impl ChannelStore {
    pub(crate) fn codex_provenance(&self) -> Sidecar {
        let channel = self.init.channel;
        match durable::read_json::<CodexProvenance>(&self.dir.join(CODEX_PROVENANCE_FILE)) {
            Ok(None) => Sidecar::Missing,
            Ok(Some(read)) if read.schema != SCHEMA || read.channel != channel => {
                Sidecar::Unavailable("codex_provenance_identity")
            }
            Ok(Some(read)) if !valid(channel, &read) => {
                Sidecar::Unavailable("codex_provenance_conflict")
            }
            Ok(Some(read)) => Sidecar::Present(read),
            Err(_) => Sidecar::Unavailable("codex_provenance_unreadable"),
        }
    }
}

/// Stored spans keep their identity and close at most once; denies only grow.
fn retained(current: &CodexProvenance, next: &CodexProvenance) -> bool {
    let kept = current.spans.iter().all(|previous| {
        let mut matches = (next.spans.iter()).filter(|span| {
            span.execution == previous.execution && span.episode == previous.episode
        });
        let (Some(kept), None) = (matches.next(), matches.next()) else {
            return false;
        };
        let mut expected = previous.clone();
        expected.end = expected.end.or(kept.end);
        *kept == expected
    });
    kept && current.denies.iter().all(|deny| next.denies.contains(deny))
}

/// Every span names this channel and a whole identity; no two share an episode, put one
/// execution on two files, or overlap or repeat a native turn on one file.
fn valid(channel: u64, next: &CodexProvenance) -> bool {
    next.spans.iter().enumerate().all(|(index, span)| {
        let (episode, execution) = (&span.episode, &span.execution);
        let whole = episode.channel_id == channel
            && span.delivery_channel_id == channel
            && span.offset_authority_channel_id == channel
            && episode.user_message_id != 0
            && episode.request_owner_id != 0
            && !episode.turn_nonce.trim().is_empty()
            && !episode.native_turn_id.trim().is_empty()
            && !execution.owner_runtime_root.trim().is_empty()
            && !execution.tmux_session.trim().is_empty()
            && !execution.execution_nonce.trim().is_empty()
            && !execution.source.session_id.trim().is_empty()
            && !execution.source.path.as_os_str().is_empty()
            && span.end.is_none_or(|end| end > span.start);
        whole
            && next.spans[..index].iter().all(|other| {
                let same_source = same_file(&execution.source, &other.execution.source);
                let same_episode = episode.channel_id == other.episode.channel_id
                    && episode.user_message_id == other.episode.user_message_id
                    && episode.turn_nonce == other.episode.turn_nonce;
                let same_execution = execution.owner_runtime_root
                    == other.execution.owner_runtime_root
                    && execution.tmux_session == other.execution.tmux_session
                    && execution.execution_nonce == other.execution.execution_nonce
                    && execution.proof_seq == other.execution.proof_seq;
                let overlaps = span.start < other.end.unwrap_or(u64::MAX)
                    && other.start < span.end.unwrap_or(u64::MAX);
                !(same_episode
                    || (same_execution && !same_source)
                    || (same_source
                        && (overlaps || episode.native_turn_id == other.episode.native_turn_id)))
            })
    })
}

#[cfg(all(test, unix))]
#[path = "codex_provenance_tests.rs"]
mod tests;
