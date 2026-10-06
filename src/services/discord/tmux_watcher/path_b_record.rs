//! The inflight row the watcher's completion Path B records, and the dispatch bound to it.

use std::future::Future;

use super::*;
use crate::db::session_transcripts::{PersistSessionTranscript, SessionTranscriptEvent};

/// Which read supplied the recorded row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathBSource {
    /// The re-read after delivery.
    Late,
    /// The pre-relay pin, used when a concurrent finalizer already removed the row.
    Pinned,
}

pub(super) struct PathBRecord<'a> {
    pub(super) state: &'a InflightTurnState,
    pub(super) source: PathBSource,
}

/// Where a Path B transcript row goes, apart from the recorded turn itself.
pub(super) struct PathBTranscript<'a> {
    pub(super) pool: Option<&'a sqlx::PgPool>,
    pub(super) channel_id: ChannelId,
    pub(super) provider: &'a ProviderKind,
    pub(super) agent_id: Option<&'a str>,
    pub(super) assistant_message: &'a str,
    pub(super) events: &'a [SessionTranscriptEvent],
}

impl<'a> PathBRecord<'a> {
    /// The late row when present, else the pin when this committed range is the pin's turn.
    pub(super) fn select(
        late: Option<&'a InflightTurnState>,
        pin: Option<&'a InflightTurnState>,
        tmux_session_name: &str,
        current_offset: u64,
    ) -> Option<Self> {
        let record = match late {
            Some(state) => Self {
                state,
                source: PathBSource::Late,
            },
            None if super::turn_identity::pinned_finalize_user_msg_id(
                pin,
                tmux_session_name,
                current_offset,
            ) != 0 =>
            {
                Self {
                    state: pin?,
                    source: PathBSource::Pinned,
                }
            }
            None => return None,
        };
        watcher_completion_lifecycle_applies(record.state).then_some(record)
    }

    /// Dispatch evidence bound to this row; only a late row may borrow the thread's pending one.
    pub(super) async fn dispatch_id<F>(&self, thread_fallback: impl FnOnce() -> F) -> Option<String>
    where
        F: Future<Output = Option<String>>,
    {
        let bound = self.state.dispatch_id.clone().or_else(|| {
            crate::services::discord::adk_session::parse_dispatch_id(&self.state.user_text)
        });
        match (bound, self.source) {
            (Some(dispatch_id), _) => Some(dispatch_id),
            (None, PathBSource::Late) => thread_fallback().await,
            (None, PathBSource::Pinned) => None,
        }
    }

    /// Writes this turn's transcript row and returns the dispatch it attributed.
    pub(super) async fn persist_transcript<F>(
        &self,
        target: PathBTranscript<'_>,
        thread_fallback: impl FnOnce() -> F,
    ) -> Option<String>
    where
        F: Future<Output = Option<String>>,
    {
        let state = self.state;
        let dispatch_id = self.dispatch_id(thread_fallback).await;
        let turn_id = format!("discord:{}:{}", target.channel_id.get(), state.user_msg_id);
        let channel_id = target.channel_id.get().to_string();
        let persisted = crate::db::session_transcripts::persist_turn_db(
            target.pool,
            PersistSessionTranscript {
                turn_id: &turn_id,
                session_key: state.session_key.as_deref(),
                channel_id: Some(channel_id.as_str()),
                agent_id: target.agent_id,
                provider: Some(target.provider.as_str()),
                dispatch_id: dispatch_id.as_deref(),
                user_message: &state.user_text,
                assistant_message: target.assistant_message,
                events: target.events,
                duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                turn_started_at_millis:
                    crate::db::session_transcripts::discord_message_started_at_millis(Some(
                        serenity::MessageId::new(state.user_msg_id),
                    )),
            },
        )
        .await;
        if let Err(e) = persisted {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
        }
        dispatch_id
    }
}

#[cfg(test)]
#[path = "path_b_record_tests.rs"]
mod tests;
