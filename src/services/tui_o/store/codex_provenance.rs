//! Codex episode provenance in its own channel file. Only a validated snapshot is Present; a
//! missing, damaged or foreign file never reads as one without spans or denies.

use serde::{Deserialize, Serialize};

use super::{ChannelStore, durable};
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::{
    CodexEpisodeDeny, CodexEpisodeSpan, ExecutionProofRef, TurnEpisodeRef,
};

#[cfg(test)]
mod mutation;
#[cfg(test)]
pub(crate) mod submit_floor;

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

/// A snapshot only this module's validation constructs; an origin is read from nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedCodexProvenance(
    CodexProvenance,
    #[cfg(test)] Option<submit_floor::Schema2>,
);

impl ValidatedCodexProvenance {
    pub(crate) fn spans(&self) -> &[CodexEpisodeSpan] {
        &self.0.spans
    }

    #[cfg(test)]
    pub(crate) fn raw(&self) -> &CodexProvenance {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn checked(channel: u64, read: CodexProvenance) -> Option<Self> {
        let identity = read.schema == SCHEMA && read.channel == channel;
        (identity && valid(channel, &read)).then_some(Self(read, None))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Sidecar {
    Missing,
    Present(ValidatedCodexProvenance),
    Unavailable(&'static str),
}

#[cfg(test)]
thread_local! {
    /// Sidecar reads and mutations on this thread, so a test can prove production made none.
    pub(crate) static SIDECAR_IO: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

impl ChannelStore {
    pub(crate) fn codex_provenance(&self) -> Sidecar {
        #[cfg(test)]
        SIDECAR_IO.with(|io| io.set((io.get().0 + 1, io.get().1)));
        #[cfg(test)]
        if let Some(read) = submit_floor::read(self) {
            return read;
        }
        self.read_codex_schema1()
    }

    pub(crate) fn read_codex_schema1(&self) -> Sidecar {
        let channel = self.init.channel;
        match durable::read_json::<CodexProvenance>(&self.dir.join(CODEX_PROVENANCE_FILE)) {
            Ok(None) => Sidecar::Missing,
            Ok(Some(read)) if read.schema != SCHEMA || read.channel != channel => {
                Sidecar::Unavailable("codex_provenance_identity")
            }
            Ok(Some(read)) if !valid(channel, &read) => {
                Sidecar::Unavailable("codex_provenance_conflict")
            }
            Ok(Some(read)) => Sidecar::Present(ValidatedCodexProvenance(
                read,
                #[cfg(test)]
                None,
            )),
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

/// The episode names this channel and an owner, and the execution its whole proof identity.
fn whole(channel: u64, episode: &TurnEpisodeRef, execution: &ExecutionProofRef) -> bool {
    episode.channel_id == channel
        && episode.user_message_id != 0
        && episode.request_owner_id != 0
        && !episode.turn_nonce.trim().is_empty()
        && !execution.owner_runtime_root.trim().is_empty()
        && !execution.tmux_session.trim().is_empty()
        && !execution.execution_nonce.trim().is_empty()
        && !execution.source.session_id.trim().is_empty()
        && !execution.source.path.as_os_str().is_empty()
}

/// Spans and denies name whole identities of this channel; spans never share an episode, split
/// an execution or overlap on one file, and a deny never names another execution of a stored span.
fn valid(channel: u64, next: &CodexProvenance) -> bool {
    let denies = next.denies.iter().all(|deny| {
        let foreign = next.spans.iter().any(|span| {
            let same = span.episode.user_message_id == deny.episode.user_message_id
                && span.episode.turn_nonce == deny.episode.turn_nonce;
            same && span.execution != deny.execution
        });
        whole(channel, &deny.episode, &deny.execution) && !foreign
    });
    denies
        && next.spans.iter().enumerate().all(|(index, span)| {
            let (episode, execution) = (&span.episode, &span.execution);
            let whole = whole(channel, episode, execution)
                && span.delivery_channel_id == channel
                && span.offset_authority_channel_id == channel
                && !episode.native_turn_id.trim().is_empty()
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
                            && (overlaps
                                || episode.native_turn_id == other.episode.native_turn_id)))
                })
        })
}

#[cfg(all(test, unix))]
#[path = "codex_provenance_tests.rs"]
mod tests;
