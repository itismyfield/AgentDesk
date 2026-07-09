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

/// #4370 (review r3): may THIS re-adopted row own a ledger entry / on-disk marker?
///
/// Only a REAL user turn carrying a REAL message id. Each exclusion closes a
/// concrete live-turn-theft or dead-weight path, BY CONSTRUCTION rather than by
/// an undocumented invariant about which turn classes can carry id 0:
///
///   - `request_owner_user_id == 0` — no owner, nothing to reclaim from.
///   - `request_owner_user_id == TUI_DIRECT_SYNTHETIC_OWNER_USER_ID` — the #4018
///     synthetic relay owner. `classify_reclaimable_mailbox_owner` short-circuits
///     that owner to `Synthetic` before it ever reads the marker or the ledger, so
///     recording it is inert dead weight (fresh-Claude r3 #4).
///   - `user_msg_id == 0` — an injected / task-notification row.
///     `effective_finalizer_turn_id()` then falls back to a value that is NOT
///     `user_msg_id`, so `stale_synthetic_mailbox_owner_reclaim_reason` would read
///     `state.user_msg_id != active_user_message_id` and misfire
///     `OwnerInflightReplaced` on a turn that is still LIVE, reclaiming it once it
///     aged past 120s (fresh-Claude r3 #1). It is also the shape codex r3 #1 needed
///     for its trailing-output theft chain. Excluding it here makes BOTH unreachable
///     at the source.
fn readopted_ledger_record_allowed(state: &inflight::InflightTurnState) -> bool {
    readopt_marker_eligible_real_user(state)
}

/// The single definition of "a real-user turn eligible for the
/// `readopted_from_inflight` marker": a real, non-synthetic owner carrying a real
/// anchored `user_msg_id`. Shared verbatim by the #4370 marker-WRITE gate
/// (`readopted_ledger_record_allowed`, above) and the #4380 crash-resume DLQ
/// backstop (`crash_resume_guard::crash_readopt_real_user_live_turn`), so the two
/// sites can never disagree on which rows carry the marker — the exact divergence
/// class that let the #4380 root bug (and its review defect 2) slip in: a doc/gate
/// claiming id-0 rows are excluded while the code only checked the owner id.
pub(in crate::services::discord) fn readopt_marker_eligible_real_user(
    state: &inflight::InflightTurnState,
) -> bool {
    state.request_owner_user_id != 0
        && state.request_owner_user_id
            != crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
        && state.user_msg_id != 0
}

/// #4370: mark this mailbox slot as re-adopted-from-inflight so the TUI-direct
/// synthetic `stale_reclaim` path recognises this real-user mailbox owner as
/// reclaimable-when-stale (generalising #4018's synthetic-owner-only reclaim to
/// the restart-resume path). Two records are written, each serving a different
/// reclaim shape:
///
///   * The in-memory `SharedData::readopted_mailbox_ledger` — AUTHORITATIVE for
///     the row-ABSENT reclaim (#4370 Path B). If the on-disk row is later cleared
///     while the mailbox stays stuck owned by the re-adopted real user, only this
///     process knows the mailbox is re-adopted; the ledger records the owner + the
///     mailbox `active_user_message_id` (the turn's effective finalizer id). A
///     live successor turn owns a DIFFERENT id and can never match, so the entry
///     is inert once its turn ends (see `ReadoptedMailboxOwner`).
///   * The on-disk `readopted_from_inflight` marker — the companion signal for a
///     PRESENT row, written through the identity-guarded single-field patch
///     `mark_readopted_from_inflight_if_identity_unchanged` (NOT a blind whole-row
///     save): a concurrently-cleared row is NOT resurrected (`Missing`), and a row
///     a newer turn re-owns is NOT clobbered (`IdentityMismatch`). It preserves
///     `restart_mode`, so it also lands on DrainRestart-preserved rows — where the
///     broad identity-refresh save would refuse to write, which is why the ABSENT
///     path relies on the ledger rather than this marker.
///
/// This does NOT touch the completion lifecycle: the re-adopted turn's own
/// `✅`/footer + analytics still fire (see the `readopted_from_inflight` field doc
/// for why it is DISTINCT from `relay_ownership_only`).
fn mark_readopted_from_inflight(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &inflight::InflightTurnState,
) {
    // The re-adopted mailbox slot carries the turn's effective finalizer id as its
    // `active_user_message_id` (`mailbox_try_start_turn(..., finalizer_msg_id)` /
    // the existing-active-turn rebind below), so the ledger keys the row-ABSENT
    // reclaim decision on exactly that id.
    let active_user_message_id = state.effective_finalizer_turn_id();
    shared.record_readopted_mailbox_owner(
        provider,
        channel_id.get(),
        state.request_owner_user_id,
        active_user_message_id,
    );

    let expected = inflight::InflightTurnIdentity::from_state(state);
    match inflight::mark_readopted_from_inflight_if_identity_unchanged(
        provider,
        channel_id.get(),
        &expected,
    ) {
        inflight::GuardedSaveOutcome::Saved => {}
        inflight::GuardedSaveOutcome::Missing => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                "readopted-from-inflight marker skipped: durable row cleared concurrently; not resurrecting (#4370)"
            );
        }
        inflight::GuardedSaveOutcome::IdentityMismatch => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                "readopted-from-inflight marker skipped: a newer turn owns the durable row; not clobbering (#4370)"
            );
        }
        inflight::GuardedSaveOutcome::IoError => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                "failed to persist readopted-from-inflight marker on re-adopted turn (#4370)"
            );
        }
    }
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
            channel_id = state.channel_id,
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
            // #4370: a real-user turn re-bound to the mailbox across a restart.
            if readopted_ledger_record_allowed(state) {
                mark_readopted_from_inflight(shared, &provider, channel_id, state);
            }
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
        // #4370: the mailbox now carries a re-adopted-from-inflight REAL user turn
        // (owner == request_owner_user_id). Record it in the ledger + on-disk
        // marker so a later starved injection / task-notification synthetic turn
        // can reclaim this mailbox once the re-adopted turn is stale — without
        // this, #4018's synthetic-owner-only reclaim can never free it and the
        // follow-up relay text is silently dropped.
        if readopted_ledger_record_allowed(state) {
            mark_readopted_from_inflight(shared, &provider, channel_id, state);
        }
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
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
                super::super::turn_finalizer::TerminalEvent::Complete,
                super::super::turn_finalizer::FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;
        assert!(matches!(
            outcome,
            super::super::turn_finalizer::FinalizeOutcome::Finalized { .. }
        ));
    }

    /// #4400 (b) implementation gate: the adopted orphan row (zero
    /// `request_owner_user_id` / `user_msg_id`, Watcher-owned — the #3107
    /// self-heal shape) reaches `reregister_active_turn_from_inflight` through
    /// the manual-rebind resume path. It must NOT seize the channel mailbox:
    /// `mailbox_try_start_turn` is only reachable for rows with a real request
    /// owner (the `request_owner_user_id == 0` early return), so a zero-id
    /// adoption can never block the next real turn's intake. It DOES re-seed
    /// the Watcher-owned finalizer ledger entry (under the synthetic finalizer
    /// id) so the respawned watcher can finalize the adopted turn.
    #[tokio::test(flavor = "current_thread")]
    async fn adopted_zero_owner_orphan_row_does_not_seize_the_mailbox() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let ch = ChannelId::new(52_484);
        let mut state = active_turn_state(ch.get(), 0);
        state.request_owner_user_id = 0;
        state.user_msg_id = 0;
        state.current_msg_id = 0;
        state.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);

        let restored = super::reregister_active_turn_from_inflight(&shared, &state).await;
        assert!(
            !restored,
            "a zero-owner row must not report a mailbox re-registration"
        );

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, ch).await;
        assert!(
            snapshot.cancel_token.is_none() && snapshot.active_user_message_id.is_none(),
            "#4400 gate: adopting a zero-id orphan must leave the mailbox ownerless — \
             seizing it would block the next real turn's intake"
        );
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(ch, shared.restart.current_generation)
                .await,
            "the adopted orphan still re-seeds a Watcher-owned finalizer ledger entry"
        );
    }

    // #3089 A0 — characterization of the recovery probe-classified outcome
    // (design §5 A0 item 3, signal #5 of 5). `RecoveryCompletionOutcome` is the
    // recovery engine's terminal-completion signal. Pinned inline in this
    // `#[cfg(test)] mod` block of the FROZEN (baseline 4090) file => ZERO prod
    // LoC.
    mod a0_characterization_tests {
        use super::super::RecoveryCompletionOutcome;

        #[test]
        fn a0_recovery_completion_proceeds_with_cleanup() {
            assert!(
                RecoveryCompletionOutcome::Emitted.should_proceed(),
                "Emitted proceeds"
            );
        }
    }
}

#[cfg(test)]
mod readopted_ledger_record_gate_tests {
    use super::{inflight, readopted_ledger_record_allowed};
    use crate::services::provider::ProviderKind;

    fn row(request_owner_user_id: u64, user_msg_id: u64) -> inflight::InflightTurnState {
        inflight::InflightTurnState::new(
            ProviderKind::Claude,
            4_370_000,
            Some("adk-cc".to_string()),
            request_owner_user_id,
            user_msg_id,
            user_msg_id,
            "user prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            128,
        )
    }

    // #4370 (review r3): only a REAL user turn with a REAL message id may own a
    // re-adopt ledger entry / on-disk marker. Both exclusions are load-bearing:
    //   * owner 1 (`TUI_DIRECT_SYNTHETIC_OWNER_USER_ID`) is short-circuited to
    //     `Synthetic` by `classify_reclaimable_mailbox_owner` before the marker or
    //     ledger is read, so a record is inert dead weight.
    //   * `user_msg_id == 0` makes `effective_finalizer_turn_id()` diverge from
    //     `user_msg_id`, which would misfire `OwnerInflightReplaced` on a LIVE turn.
    #[test]
    fn only_real_owner_with_real_message_id_is_recorded() {
        assert!(
            readopted_ledger_record_allowed(&row(343_742_347_365_974_026, 4_370_160)),
            "a real user turn with a real message id must be recorded"
        );
        assert!(
            !readopted_ledger_record_allowed(&row(0, 4_370_160)),
            "an ownerless row has nothing to reclaim from"
        );
        assert!(
            !readopted_ledger_record_allowed(&row(1, 4_370_160)),
            "the #4018 synthetic relay owner is classified before the marker is read"
        );
        assert!(
            !readopted_ledger_record_allowed(&row(343_742_347_365_974_026, 0)),
            "an id-0 (injected / task-notification) row would misfire OwnerInflightReplaced on a live turn"
        );
    }
}
