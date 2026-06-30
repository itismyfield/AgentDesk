//! Runtime-rediscovery recovery path (#3834 decompose split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: the second recovery
//! path — runtime recovery — i.e. `reregister_active_turn_from_inflight` (the
//! mid-execution mailbox/runtime reattach entry) and its private support helper
//! `reseed_watcher_owned_finalizer_ledger`. They depend only on the parent
//! module's re-exported types and helpers (`SharedData`, `ProviderKind`,
//! `ChannelId` / `MessageId` / `UserId`, `CancelToken`, the `inflight` /
//! `turn_finalizer` modules, `mailbox_snapshot` / `mailbox_try_start_turn` /
//! `ensure_cancel_token_bound_from_inflight_state` / `clear_inflight_state`,
//! `finish_recovered_turn_mailbox`, and `recovery_terminal_delivery_already_committed`),
//! pulled in via `use super::*`, so this cluster lives in a leaf module.
//! `reregister_active_turn_from_inflight` is re-exported by the root so
//! `recovery_engine::reregister_active_turn_from_inflight` stays valid for its
//! `watchers::lifecycle` / `manual_rebind` callers and for the
//! `restore_inflight_turns` (restart-path) reattach calls; the reseed helper is
//! private to this module. Moved verbatim — zero logic change.

use super::*;

/// #3248/#3645: recovery must re-seed a Watcher-owned ledger entry after
/// restart, keyed by the stable finalizer id, so busy GateTimeout defers and
/// the far-backstop can reconcile. `register_start` is idempotent for the same
/// `TurnKey`, so a later bridge handoff is a no-op refresh.
fn reseed_watcher_owned_finalizer_ledger(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    finalizer_turn_id: u64,
    provider: &ProviderKind,
) {
    // id-0 would key the channel-only orphan slot. Only seed a full-identity
    // Watcher entry; synthetic live turns use their persisted finalizer_turn_id.
    if finalizer_turn_id == 0 {
        return;
    }
    shared.turn_finalizer.register_start(
        super::turn_finalizer::TurnKey::new(
            channel_id,
            finalizer_turn_id,
            shared.restart.current_generation,
        ),
        provider.clone(),
        super::inflight::RelayOwnerKind::Watcher,
        shared, // #3016 phase-5a: prime the reconcile cache at register time.
    );
}

pub(in crate::services::discord) async fn reregister_active_turn_from_inflight(
    shared: &Arc<SharedData>,
    state: &inflight::InflightTurnState,
) -> bool {
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    if finalizer_turn_id == 0 {
        return false;
    }

    let channel_id = ChannelId::new(state.channel_id);
    let finalizer_msg_id = MessageId::new(finalizer_turn_id);
    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    let Some(provider) = ProviderKind::from_str(&state.provider) else {
        tracing::error!(
            "inflight reregister failed: provider={} channel_id={} error=unsupported_provider",
            state.provider,
            state.channel_id
        );
        return false;
    };
    if recovery_terminal_delivery_already_committed(state) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            finalizer_turn_id,
            "inflight reregister skipped: terminal delivery already committed; clearing stale active turn state"
        );
        finish_recovered_turn_mailbox(
            shared,
            &provider,
            channel_id,
            "recovery_terminal_delivery_already_committed",
        )
        .await;
        clear_inflight_state(&provider, state.channel_id);
        return false;
    }
    if snapshot.cancel_token.is_some() {
        if let Some(token) = snapshot.cancel_token.as_ref()
            && snapshot.active_user_message_id == Some(finalizer_msg_id)
        {
            super::ensure_cancel_token_bound_from_inflight_state(
                &provider,
                state,
                token,
                "inflight reregister existing active turn",
            );
        }
        let restored = snapshot.active_user_message_id == Some(finalizer_msg_id);
        if restored {
            reseed_watcher_owned_finalizer_ledger(shared, channel_id, finalizer_turn_id, &provider);
        }
        return restored;
    }

    if state.request_owner_user_id == 0 {
        reseed_watcher_owned_finalizer_ledger(shared, channel_id, finalizer_turn_id, &provider);
        return false;
    }

    let cancel_token = Arc::new(CancelToken::new());
    super::ensure_cancel_token_bound_from_inflight_state(
        &provider,
        state,
        &cancel_token,
        "inflight reregister active turn",
    );

    let started = super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token,
        UserId::new(state.request_owner_user_id),
        finalizer_msg_id,
    )
    .await;
    if started {
        reseed_watcher_owned_finalizer_ledger(shared, channel_id, finalizer_turn_id, &provider);
    }
    started
}

#[cfg(test)]
mod delivered_inflight_reregister_tests {
    use super::{inflight, recovery_terminal_delivery_already_committed};
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;

    #[test]
    fn committed_terminal_delivery_is_not_recoverable_active_turn() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            4243,
            Some("adk-cc".to_string()),
            7,
            9101,
            9102,
            "summarize recent PRs".to_string(),
            Some("session-2".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.full_response = "already posted response".to_string();
        state.response_sent_offset = state.full_response.len();
        state.terminal_delivery_committed = true;

        assert!(recovery_terminal_delivery_already_committed(&state));
    }

    #[test]
    fn ordinary_inflight_still_recoverable_even_with_relayed_prefix() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            4244,
            Some("adk-cc".to_string()),
            7,
            9201,
            9202,
            "continue streaming".to_string(),
            Some("session-3".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            256,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.full_response = "partial response".to_string();
        state.response_sent_offset = state.full_response.len();

        assert!(!recovery_terminal_delivery_already_committed(&state));
    }
}

// #3248 gap-1 — the pane-alive reattach path (`reregister_active_turn_from_inflight`)
// must re-seed the single-authority finalizer ledger with a Watcher-owned entry
// after a mid-turn dcserver restart clears the in-memory ledger. Without it the
// live pane never auto-reconciles (the watcher's id-0 gate-timeout creates a
// `relay_owner=None` orphan that finalizes immediately instead of arming the 8s
// backstop, and the far-backstop reconcile — which collects only
// `relay_owner==Watcher` rows — never catches it), so a NEW user turn is required.
#[cfg(test)]
mod reregister_ledger_reseed_tests {
    use super::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;
    use serenity::model::id::ChannelId;

    fn active_turn_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        // A live (not-yet-committed) ordinary turn whose ids are all non-zero, so
        // it passes the `reregister_active_turn_from_inflight` early guard and is
        // NOT short-circuited by `recovery_terminal_delivery_already_committed`.
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-cc".to_string()),
            7, // request_owner_user_id
            user_msg_id,
            user_msg_id + 1, // current_msg_id
            "live prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            0,
        )
    }

    // Gap-1: a fresh reattach (empty mailbox → mailbox turn started) re-seeds the
    // ledger so the turn is a LIVE Watcher-pending entry. This is precisely the
    // state that makes the watcher's gate-timeout arm its backstop instead of
    // finalizing-as-orphan, and makes the far-backstop reconcile able to collect
    // the row — so the live pane auto-reconciles WITHOUT a new user turn.
    #[tokio::test(flavor = "current_thread")]
    async fn reattach_reseeds_watcher_owned_ledger_entry() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(52_481);
        let state = active_turn_state(ch.get(), 9001);

        // Pre-condition: post-restart the in-memory ledger is empty — no
        // watcher-pending entry exists for this turn yet.
        assert!(
            !shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "ledger must start empty (simulating a post-restart in-memory ledger)"
        );

        let restored = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(
            restored,
            "an empty mailbox must let the reattach start the active turn"
        );

        // Post-condition: the ledger now has a LIVE Watcher-owned entry under the
        // turn's full identity + the current (restart) generation.
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "#3248 gap-1: reattach must register_start the turn as Watcher-owned"
        );
    }

    // Idempotency: a second reattach of the SAME turn (or a later bridge handoff
    // register_start) must NOT error or duplicate/over-finalize — the actor's
    // `Start` handler is entry().and_modify().or_insert() and never resurrects a
    // finalized turn. The turn stays a single live Watcher-pending entry.
    #[tokio::test(flavor = "current_thread")]
    async fn repeated_reattach_is_idempotent_single_watcher_entry() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(52_482);
        let state = active_turn_state(ch.get(), 9101);

        assert!(super::reregister_active_turn_from_inflight(&shared, &state).await);
        // Second call: the mailbox already holds the active turn, so this takes
        // the "existing active turn" rebind branch and re-seeds again.
        let restored_again = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(
            restored_again,
            "re-attaching an already-active turn re-binds (returns true) without panic"
        );

        // Still exactly one live Watcher-pending entry (idempotent re-register).
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "repeated reattach keeps a single live Watcher-pending entry"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_user_msg_id_reseeds_with_finalizer_turn_id() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(52_483);
        let mut state = active_turn_state(ch.get(), 0);
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.finalizer_turn_id = 9_010_777;

        let restored = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(
            restored,
            "a zero user_msg_id turn with a stable finalizer_turn_id is re-attached"
        );
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "zero-id recovery must seed a Watcher entry under finalizer_turn_id"
        );
        let outcome = shared
            .turn_finalizer
            .submit_terminal(
                super::super::turn_finalizer::TurnKey::new(
                    ch,
                    state.finalizer_turn_id,
                    shared.restart.current_generation,
                ),
                ProviderKind::Claude,
                super::super::turn_finalizer::TerminalEvent::GateTimeout {
                    pane_quiescent: Some(false),
                },
                super::super::turn_finalizer::FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;
        assert!(matches!(
            outcome,
            super::super::turn_finalizer::FinalizeOutcome::Deferred
        ));
    }

    // #3089 A0 — characterization of the recovery probe-classified outcome
    // (design §5 A0 item 3, signal #5 of 5). `RecoveryCompletionOutcome` is the
    // recovery engine's terminal-completion signal; BOTH arms `should_proceed()`
    // (a suppressed visible completion is NOT a delivery failure, so callers
    // still release mailbox/inflight ownership). Pinned inline in this
    // `#[cfg(test)] mod` block of the FROZEN (baseline 4090) file => ZERO prod
    // LoC.
    mod a0_characterization_tests {
        use super::super::RecoveryCompletionOutcome;

        #[test]
        fn a0_both_recovery_outcomes_proceed_with_cleanup() {
            assert!(
                RecoveryCompletionOutcome::Emitted.should_proceed(),
                "Emitted proceeds"
            );
            assert!(
                RecoveryCompletionOutcome::VisibleCompletionSuppressed.should_proceed(),
                "VisibleCompletionSuppressed still proceeds (terminal delivery is authoritative)"
            );
        }

        #[test]
        fn a0_recovery_outcomes_are_two_distinct_arms() {
            assert_ne!(
                RecoveryCompletionOutcome::Emitted,
                RecoveryCompletionOutcome::VisibleCompletionSuppressed
            );
        }
    }
}
