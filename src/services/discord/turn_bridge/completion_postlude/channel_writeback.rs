//! #4658 F1 and #5464 T5 completion-side channel-effect isolation.
//!
//! The scheduled-snapshot turn START path already cold-starts an isolated
//! session (isolated `session_key`, no channel severance). The COMPLETION path
//! must be isolated too: at turn end `run_completion_postlude` writes the turn's
//! provider `session_id` and its user/assistant history back into the channel's
//! shared in-memory session (`data.sessions[channel_id]`). For a suppressed turn
//! that writeback would leak the snapshot session into the live channel, so the
//! next live user turn would silently RESUME the snapshot session instead of the
//! real conversation (the #4634 bug class, completion side).
//!
//! [`apply_channel_turn_writeback`] performs that writeback for a normal turn
//! and SKIPS every channel-session mutation for a suppressed turn, leaving both
//! `.history` and the provider `session_id` byte-for-byte unchanged.
//!
//! # Isolation invariant (single source of truth)
//!
//! `run_completion_postlude` combines scheduled-snapshot isolation with a fresh
//! `ChannelEpisodeScope` decision. A turn whose session key is isolated, whose
//! mailbox is foreign, or whose ownership is unprovable must produce ZERO
//! channel-scoped side-effects that a LATER LIVE TURN can observe. Every such
//! effect is gated on the combined `channel_effects_suppressed` predicate:
//!   1. sessions-map writeback — provider `session_id` + history into
//!      `data.sessions[channel_id]` (live intake resumes it). Guarded inside
//!      [`apply_channel_turn_writeback`].
//!   2. memento reflect  (`take_memento_reflect_request` → recalled at intake).
//!   3. memento capture  (`should_spawn_memory_capture` → recalled at intake).
//!   4. voluntary tool_feedback reminder stash — `store_voluntary_feedback_reminder`
//!      writes a (provider, channel_id) KV that the NEXT live intake takes and
//!      injects into the model prompt (`response_format.rs`). Gated via
//!      [`feedback_reminder_to_stash`].
//!   5. api-friction memory — `record_api_friction_reports` calls
//!      `backend.remember(..)`, landing in the agent's memento memory that a live
//!      turn's recall can surface.
//!   6. turn-end WIP warning stash — a provider/channel KV consumed by next intake.
//!
//! Final session status, watcher resume, turn-start removal, restart-report clear,
//! and mailbox recovery-marker clear use fresh ownership conjuncts at their own
//! effect groups rather than this helper predicate.
//!
//! Turn-OWN or observability-only effects are intentionally NOT gated: transcript,
//! analytics, quality and metric emits (dashboards, never read back into a live
//! prompt), plus identity-guarded inflight lifecycle and queued-turn drain. Provider
//! session clear is destructive state under `adk_session_key`; key separation does
//! not prove ownership for `Foreign`/`Unprovable`, so it uses the combined suppression
//! predicate. Provider session save already requires unsuppressed writeback output.
//!
//! # F-2 (documented limitation, non-blocking)
//!
//! The scheduled-snapshot operand of `channel_effects_suppressed` is RECOMPUTED
//! at completion rather than threading a start-time boolean (which would require a hotfile
//! `turn_bridge/mod.rs` logic change). If `session.channel_name` changes
//! mid-turn — a manual rebind (`recovery_engine/manual_rebind/episode_handoff.rs`)
//! or a `/session` rename (`commands/session.rs`) concurrent with a channel
//! rename — a NORMAL turn's recomputed canonical key could differ from its
//! start-time `adk_session_key` and be wrongly treated as isolated, skipping its
//! channel writeback. The window is extremely narrow and skip-in-rebind is safe
//! (no corruption, only a dropped writeback that self-heals next turn), so it is
//! recorded here, not re-architected.

use super::super::super::DiscordSession;
use super::super::memory_lifecycle::TurnEndMemoryPlan;
use crate::ui::ai_screen::{HistoryItem, HistoryType};

/// Outcome of the end-of-turn channel-session writeback.
pub(in crate::services::discord::turn_bridge) struct ChannelTurnWriteback {
    /// Provider `session_id` to persist to the DB under the turn's own
    /// `session_key`. `None` when the writeback was skipped (suppressed turn) or
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
/// When `channel_effects_suppressed` is `true` — because the session key is
/// isolated or fresh mailbox ownership is foreign/unprovable — the channel
/// session MUST be left completely unchanged. Every mutation is skipped so the
/// turn can never leak its provider `session_id` or text into a live conversation
/// it does not own.
pub(in crate::services::discord::turn_bridge) fn apply_channel_turn_writeback(
    session: &mut DiscordSession,
    channel_effects_suppressed: bool,
    plan: &TurnEndMemoryPlan,
    user_text: &str,
    full_response: &str,
    new_session_id: Option<&str>,
) -> ChannelTurnWriteback {
    // #4658 F1 isolation guard: a suppressed turn never touches the channel
    // session. Removing this early return re-introduces the completion-side
    // leak (covered by `scheduled_snapshot_turn_leaves_channel_session_untouched`).
    if channel_effects_suppressed {
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

/// Select the provider session key only when this completion owns channel effects
/// and the turn-end memory plan requests a destructive provider-session clear.
pub(in crate::services::discord::turn_bridge) fn provider_session_clear_key<'a>(
    channel_effects_suppressed: bool,
    clear_provider_session: bool,
    session_key: Option<&'a str>,
) -> Option<&'a str> {
    (!channel_effects_suppressed && clear_provider_session)
        .then_some(session_key)
        .flatten()
}

/// #4658 F1: gate the voluntary tool_feedback reminder stash on channel
/// ownership. `store_voluntary_feedback_reminder` writes a (provider,
/// channel_id) KV that the NEXT live intake takes and injects into the model
/// prompt (`response_format.rs`), so a scheduled-snapshot turn stashing a
/// reminder would leak its recall/feedback output into the live conversation's
/// next turn (same F1-invariant class as the sessions-map writeback).
///
/// Returns the reminder to stash ONLY for a channel-owning turn; a suppressed turn
/// (`channel_effects_suppressed`) yields `None` so nothing is written to the shared
/// channel KV.
pub(in crate::services::discord::turn_bridge) fn feedback_reminder_to_stash(
    channel_effects_suppressed: bool,
    reminder: Option<String>,
) -> Option<String> {
    // #4658 F1 isolation guard: removing this early return re-introduces the
    // completion-side reminder leak (covered by
    // `scheduled_snapshot_turn_does_not_stash_feedback_reminder`).
    if channel_effects_suppressed {
        return None;
    }
    reminder
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
            "suppressed turn must not overwrite the channel's provider session_id"
        );
        assert_eq!(
            history_snapshot(&session),
            before_history,
            "suppressed turn must not append its turn text to the channel history"
        );
        assert!(
            !outcome.persist_transcript,
            "suppressed turn must not drive channel-session transcript persistence"
        );
        assert_eq!(
            outcome.session_id_to_persist, None,
            "suppressed turn must not persist a session_id read from the channel session"
        );
    }

    #[test]
    fn foreign_completion_cannot_select_provider_session_clear() {
        assert_eq!(
            provider_session_clear_key(true, true, Some("host:live-channel")),
            None,
            "Foreign/Unprovable completion must not call clear_provider_session_id"
        );
        assert_eq!(
            provider_session_clear_key(false, true, Some("host:live-channel")),
            Some("host:live-channel")
        );
        assert_eq!(
            provider_session_clear_key(false, false, Some("host:live-channel")),
            None
        );
    }

    /// Mutation proof (F-1): a scheduled-snapshot turn must NOT stash a
    /// voluntary tool_feedback reminder into the channel-scoped KV — otherwise
    /// the next live intake would inject it into the live prompt. Deleting the
    /// isolation guard in `feedback_reminder_to_stash` makes this FAIL on the
    /// `is_none()` assertion (not a compile error).
    #[test]
    fn scheduled_snapshot_turn_does_not_stash_feedback_reminder() {
        let reminder = Some("please leave tool_feedback for your recall".to_string());

        let stashed = feedback_reminder_to_stash(true, reminder.clone());
        assert!(
            stashed.is_none(),
            "suppressed turn must not stash a feedback reminder into the channel KV"
        );

        // A normal (channel-owning) turn still stashes so live coverage stays.
        let stashed_normal = feedback_reminder_to_stash(false, reminder.clone());
        assert_eq!(
            stashed_normal, reminder,
            "normal turn must still stash the feedback reminder for next-turn injection"
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
