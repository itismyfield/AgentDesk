//! Episode metadata only grows; native ranges close once without changing their identity.

#[cfg(test)]
use super::{ChannelStore, CodexEpisodeSpan};
use super::{Rotation, StoreError};
use crate::services::tui_o::shadow::capture::same_file;

fn rejected() -> StoreError {
    StoreError::Rejected("conflicting Codex episode provenance".into())
}

pub(super) fn validate(
    channel: u64,
    current: &Rotation,
    next: &Rotation,
) -> Result<(), StoreError> {
    for previous in &current.codex_spans {
        let matches: Vec<_> = next
            .codex_spans
            .iter()
            .filter(|span| span.execution == previous.execution && span.episode == previous.episode)
            .collect();
        let [kept] = matches.as_slice() else {
            return Err(rejected());
        };
        let mut expected = previous.clone();
        if expected.end.is_none() {
            expected.end = kept.end;
        }
        if **kept != expected {
            return Err(rejected());
        }
    }
    if current
        .codex_denies
        .iter()
        .any(|deny| !next.codex_denies.contains(deny))
    {
        return Err(rejected());
    }
    for (index, span) in next.codex_spans.iter().enumerate() {
        let episode = &span.episode;
        let execution = &span.execution;
        if episode.channel_id != channel
            || span.delivery_channel_id != channel
            || span.offset_authority_channel_id != channel
            || episode.user_message_id == 0
            || episode.request_owner_id == 0
            || episode.turn_nonce.trim().is_empty()
            || episode.native_turn_id.trim().is_empty()
            || execution.owner_runtime_root.trim().is_empty()
            || execution.tmux_session.trim().is_empty()
            || execution.execution_nonce.trim().is_empty()
            || execution.source.session_id.trim().is_empty()
            || execution.source.path.as_os_str().is_empty()
            || span.end.is_some_and(|end| end <= span.start)
        {
            return Err(rejected());
        }
        for other in &next.codex_spans[..index] {
            let same_source = same_file(&execution.source, &other.execution.source);
            let same_episode = episode.channel_id == other.episode.channel_id
                && episode.user_message_id == other.episode.user_message_id
                && episode.turn_nonce == other.episode.turn_nonce;
            let same_execution = execution.owner_runtime_root == other.execution.owner_runtime_root
                && execution.tmux_session == other.execution.tmux_session
                && execution.execution_nonce == other.execution.execution_nonce
                && execution.proof_seq == other.execution.proof_seq;
            let overlaps = span.start < other.end.unwrap_or(u64::MAX)
                && other.start < span.end.unwrap_or(u64::MAX);
            if same_episode
                || (same_execution && !same_source)
                || (same_source
                    && (overlaps || episode.native_turn_id == other.episode.native_turn_id))
            {
                return Err(rejected());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
impl Rotation {
    pub(crate) fn valid_codex_provenance(&self, channel: u64) -> bool {
        validate(channel, self, self).is_ok()
    }
}

#[cfg(test)]
impl ChannelStore {
    /// Publish the returned boundary only after its durable replace succeeds.
    pub(crate) fn persist_codex_span(
        &mut self,
        span: CodexEpisodeSpan,
    ) -> Result<CodexEpisodeSpan, StoreError> {
        let mut rotation = self.rotation()?;
        let existing = rotation
            .codex_spans
            .iter_mut()
            .find(|stored| stored.execution == span.execution && stored.episode == span.episode);
        match existing {
            Some(stored) => *stored = span.clone(),
            None => rotation.codex_spans.push(span.clone()),
        }
        self.write_rotation(&rotation)?;
        Ok(span)
    }
}

#[cfg(all(test, unix))]
#[path = "provenance_tests.rs"]
mod tests;
