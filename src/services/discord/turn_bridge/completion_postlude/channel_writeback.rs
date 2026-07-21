//! #4658 F1 completion-side session isolation for scheduled-snapshot turns.
//!
//! The scheduled-snapshot turn START path already cold-starts an isolated
//! session (isolated `session_key`, no channel severance). The COMPLETION path
//! must be isolated too: at turn end `run_completion_postlude` writes the turn's
//! provider `session_id` and its user/assistant history back into the channel's
//! shared in-memory session (`data.sessions[channel_id]`). For a snapshot turn
//! that writeback would leak the snapshot session into the live channel, so the
//! next live user turn would silently RESUME the snapshot session instead of the
//! real conversation (the #4634 bug class, completion side).
//!
//! [`apply_channel_turn_writeback`] performs that writeback for a normal turn
//! and SKIPS every channel-session mutation for a snapshot turn, leaving both
//! `.history` and the provider `session_id` byte-for-byte unchanged.

use super::super::super::DiscordSession;
use super::super::memory_lifecycle::TurnEndMemoryPlan;
use crate::ui::ai_screen::{HistoryItem, HistoryType};

/// Outcome of the end-of-turn channel-session writeback.
pub(in crate::services::discord::turn_bridge) struct ChannelTurnWriteback {
    /// Provider `session_id` to persist to the DB under the turn's own
    /// `session_key`. `None` when the writeback was skipped (snapshot turn) or
    /// the session held no id.
    pub(in crate::services::discord::turn_bridge) session_id_to_persist: Option<String>,
    /// Whether this turn's transcript should be persisted.
    pub(in crate::services::discord::turn_bridge) persist_transcript: bool,
}

/// Apply the end-of-turn writeback to the channel's shared live session.
///
/// For a normal turn this pushes the user/assistant turn into `session.history`
/// and restores (or clears) the provider `session_id`, exactly as the inline
/// block did before extraction.
///
/// #4658 F1: when `isolated_from_channel` is `true` — a scheduled-snapshot turn
/// whose `session_key` is derived from the reservation label, not the channel
/// name — the channel session MUST be left completely unchanged. Every mutation
/// is skipped so the snapshot turn can never leak its provider `session_id` or
/// turn text into the channel's live conversation.
pub(in crate::services::discord::turn_bridge) fn apply_channel_turn_writeback(
    session: &mut DiscordSession,
    isolated_from_channel: bool,
    plan: &TurnEndMemoryPlan,
    user_text: &str,
    full_response: &str,
    new_session_id: Option<&str>,
) -> ChannelTurnWriteback {
    // #4658 F1 isolation guard: a snapshot turn never touches the channel
    // session. Removing this early return re-introduces the completion-side
    // leak (covered by `scheduled_snapshot_turn_leaves_channel_session_untouched`).
    if isolated_from_channel {
        return ChannelTurnWriteback {
            session_id_to_persist: None,
            persist_transcript: false,
        };
    }

    let mut persist_transcript = false;
    if plan.persist_transcript {
        session.history.push(HistoryItem {
            item_type: HistoryType::User,
            content: user_text.to_string(),
        });
        session.history.push(HistoryItem {
            item_type: HistoryType::Assistant,
            content: full_response.to_string(),
        });
        persist_transcript = true;
    }
    if plan.clear_provider_session {
        session.clear_provider_session();
    } else if let Some(sid) = new_session_id {
        session.restore_provider_session(Some(sid.to_string()));
    }
    ChannelTurnWriteback {
        session_id_to_persist: session.session_id.clone(),
        persist_transcript,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_channel_session() -> DiscordSession {
        DiscordSession {
            session_id: Some("live-channel-session".to_string()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: vec![
                HistoryItem {
                    item_type: HistoryType::User,
                    content: "live-u1".to_string(),
                },
                HistoryItem {
                    item_type: HistoryType::Assistant,
                    content: "live-a1".to_string(),
                },
            ],
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(42),
            channel_name: Some("live-channel".to_string()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        }
    }

    fn persist_plan() -> TurnEndMemoryPlan {
        TurnEndMemoryPlan {
            session_end_reason: None,
            clear_provider_session: false,
            persist_transcript: true,
            analyze_recall_feedback: false,
            spawn_capture: false,
        }
    }

    fn history_snapshot(session: &DiscordSession) -> Vec<(HistoryType, String)> {
        session
            .history
            .iter()
            .map(|item| (item.item_type, item.content.clone()))
            .collect()
    }

    /// Mutation proof: a scheduled-snapshot turn (isolated session key) must
    /// leave the channel's live in-memory session byte-for-byte unchanged.
    /// Deleting the isolation guard in `apply_channel_turn_writeback` makes this
    /// FAIL on the `session_id` / history assertions (not a compile error).
    #[tokio::test]
    async fn scheduled_snapshot_turn_leaves_channel_session_untouched() {
        let mut session = seeded_channel_session();
        let before_session_id = session.session_id.clone();
        let before_history = history_snapshot(&session);

        let outcome = apply_channel_turn_writeback(
            &mut session,
            true, // isolated scheduled-snapshot turn
            &persist_plan(),
            "snapshot-turn-user",
            "snapshot-turn-assistant",
            Some("snapshot-provider-session"),
        );

        assert_eq!(
            session.session_id, before_session_id,
            "snapshot turn must not overwrite the channel's provider session_id"
        );
        assert_eq!(
            history_snapshot(&session),
            before_history,
            "snapshot turn must not append its turn text to the channel history"
        );
        assert!(
            !outcome.persist_transcript,
            "snapshot turn must not drive channel-session transcript persistence"
        );
        assert_eq!(
            outcome.session_id_to_persist, None,
            "snapshot turn must not persist a session_id read from the channel session"
        );
    }

    /// Guardrail: a normal turn still writes back into the channel session so
    /// the isolation guard cannot silently suppress the live path.
    #[tokio::test]
    async fn normal_turn_writes_back_into_channel_session() {
        let mut session = seeded_channel_session();

        let outcome = apply_channel_turn_writeback(
            &mut session,
            false, // normal live turn
            &persist_plan(),
            "live-u2",
            "live-a2",
            Some("new-provider-session"),
        );

        assert_eq!(
            session.session_id.as_deref(),
            Some("new-provider-session"),
            "normal turn must restore the fresh provider session_id"
        );
        assert_eq!(
            session.history.len(),
            4,
            "normal turn must append the user+assistant pair to channel history"
        );
        assert!(outcome.persist_transcript);
        assert_eq!(
            outcome.session_id_to_persist.as_deref(),
            Some("new-provider-session")
        );
    }
}
