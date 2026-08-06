use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serenity::model::id::{ChannelId, MessageId, UserId};

use super::*;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

fn terminal_snapshot(user_msg_id: u64, turn_nonce: &str) -> SyntheticClaimSnapshot {
    SyntheticClaimSnapshot {
        user_msg_id,
        turn_nonce: Some(turn_nonce.to_string()),
        turn_source_external: false,
        relay_owner_watcher: false,
        injected_prompt_message_id: None,
        tmux_session_name: None,
        started_at: String::new(),
        status_message_id: None,
        status_panel_generation: 0,
        save_generation: 0,
        current_tool_line: None,
        turn_start_offset: None,
        relay_ownership_only: false,
        relay_owner_kind: RelayOwnerKind::None,
    }
}

fn queued_intervention(message_id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::load_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "queued after residual owner".to_string(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Stand in for "a turn-bridge loop still owns this turn": the release gate only
/// asks whether the row exists, so a placeholder body is enough.
fn seed_inflight_row(channel_id: ChannelId) -> std::path::PathBuf {
    let root = crate::services::discord::inflight::inflight_runtime_root().expect("isolated root");
    let path = crate::services::discord::inflight::inflight_state_path(
        &root,
        &ProviderKind::Claude,
        channel_id.get(),
    );
    std::fs::create_dir_all(path.parent().expect("provider dir")).expect("create provider dir");
    std::fs::write(&path, "{}").expect("seed inflight row");
    path
}

async fn reconcile_once(shared: &Arc<SharedData>) {
    let mut ledger = HashMap::new();
    let mut pending_admission = HashMap::new();
    reconcile::reconcile(&mut ledger, &mut pending_admission, shared).await;
}

async fn seed_active_with_queue(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    active_user_msg_id: u64,
    queued_user_msg_id: u64,
) -> Arc<CancelToken> {
    seed_owner(
        shared,
        channel_id,
        active_user_msg_id,
        queued_user_msg_id,
        Arc::new(CancelToken::new()),
    )
    .await
}

async fn seed_owner(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    active_user_msg_id: u64,
    queued_user_msg_id: u64,
    token: Arc<CancelToken>,
) -> Arc<CancelToken> {
    assert!(
        crate::services::discord::mailbox_try_start_turn(
            shared,
            channel_id,
            token.clone(),
            UserId::new(7),
            MessageId::new(active_user_msg_id),
        )
        .await
    );
    shared
        .mailbox(channel_id)
        .replace_queue(
            vec![queued_intervention(queued_user_msg_id)],
            crate::services::discord::queue_persistence_context(
                shared,
                &ProviderKind::Claude,
                channel_id,
            ),
        )
        .await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    token
}

/// What `/health` would print for this channel right now.
async fn health_status(shared: &Arc<SharedData>, channel_id: ChannelId) -> &'static str {
    use crate::services::discord::health::mailbox;
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
    mailbox::mailbox_agent_turn_status(
        snapshot.cancel_token.is_some(),
        mailbox::residual_occupancy(shared, channel_id, &snapshot),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn kickoff_guard_skip_preserves_queue_and_arms_recovery() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_050);
        let active_user_msg_id = 5_068_051;
        let queued_user_msg_id = 5_068_052;
        seed_active_with_queue(&shared, channel_id, active_user_msg_id, queued_user_msg_id).await;

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        super::handle_idle_queue_guard_skip(&shared, &ProviderKind::Claude, channel_id, &snapshot)
            .await;

        let after = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            after.intervention_queue.len(),
            1,
            "guard skip must not dequeue"
        );
        assert_eq!(
            after.intervention_queue[0].message_id.get(),
            queued_user_msg_id
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "guard skip with preserved backlog must leave a retry owner"
        );
    })
    .await;
}

/// #5068 r3 (R2) — the state health used to be unable to name.
///
/// A residue with no terminal episode nonce is held by the reconciler FOREVER:
/// no I/O gate clears, and nothing but a user cancel or a newer episode changes
/// the answer. Before r3 `residual_occupancy` collapsed to a single bool, so
/// this channel reported `active` — byte-identical to a turn that is simply
/// running, which is precisely the case an operator most needs to spot. Health
/// still refuses to claim `residual` (the reconciler will not act), but it now
/// says the mailbox is held rather than pretending it is busy.
#[tokio::test(flavor = "current_thread")]
async fn health_names_the_permanently_held_residue_instead_of_calling_it_active() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_500);
        let terminal_user_msg_id = 5_068_501;
        seed_owner(
            &shared,
            channel_id,
            5_068_502,
            5_068_503,
            Arc::new(CancelToken::from_persisted_turn_nonce(None)),
        )
        .await;
        let mut nonceless_terminal = terminal_snapshot(terminal_user_msg_id, "");
        nonceless_terminal.turn_nonce = None;

        do_finalize(
            TurnKey::new(channel_id, terminal_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(&nonceless_terminal),
            &shared,
        )
        .await;
        reconcile_once(&shared).await;

        assert!(
            crate::services::discord::mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_some(),
            "fixture premise: the reconciler holds this residue and never resolves it"
        );
        assert_eq!(
            health_status(&shared, channel_id).await,
            "residual_held",
            "an indefinitely stranded mailbox must not be reported as an ordinary active turn"
        );
    })
    .await;
}

/// #5068 r3 (R3) — the documented asymmetry, on the shape that actually has an
/// inflight row instead of one where the conjunct is vacuous.
///
/// `residual_occupancy` claims it does not consult the I/O evidence gates,
/// because a residue waiting on a turn-bridge owner is transient and still worth
/// showing. This pins that claim against the reconciler's real behaviour in the
/// same fixture: the reconciler HOLDS (the row is on disk) and health
/// nevertheless reports `residual`, not `active` and not `residual_held`.
#[tokio::test(flavor = "current_thread")]
async fn health_still_shows_residual_while_an_inflight_owner_holds_the_release() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_600);
        let terminal_user_msg_id = 5_068_601;
        let token = seed_active_with_queue(&shared, channel_id, 5_068_602, 5_068_603).await;
        let nonce = token.turn_nonce().expect("fresh token nonce").to_string();
        let inflight_row = seed_inflight_row(channel_id);

        do_finalize(
            TurnKey::new(channel_id, terminal_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(&terminal_snapshot(terminal_user_msg_id, &nonce)),
            &shared,
        )
        .await;
        reconcile_once(&shared).await;

        assert!(
            crate::services::discord::mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_some(),
            "fixture premise: the inflight row makes the reconciler hold this tick"
        );
        assert_eq!(
            health_status(&shared, channel_id).await,
            "residual",
            "a residue held only by an I/O gate stays visible as residual"
        );

        // And the label tracks the decider: once the gate clears and the
        // reconciler releases, health stops reporting occupancy at all.
        std::fs::remove_file(&inflight_row).expect("clear inflight row");
        reconcile_once(&shared).await;
        assert_eq!(health_status(&shared, channel_id).await, "idle");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_miss_residue_reconcile_releases_same_terminal_episode_and_rearms_queue() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_100);
        let terminal_user_msg_id = 5_068_101;
        let residual_user_msg_id = 5_068_102;
        let queued_user_msg_id = 5_068_103;
        let token = seed_active_with_queue(
            &shared,
            channel_id,
            residual_user_msg_id,
            queued_user_msg_id,
        )
        .await;
        let nonce = token.turn_nonce().expect("fresh token nonce").to_string();
        let terminal_snapshot = terminal_snapshot(terminal_user_msg_id, &nonce);

        let outcome = do_finalize(
            TurnKey::new(channel_id, terminal_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(&terminal_snapshot),
            &shared,
        )
        .await;
        assert!(matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: None,
                ..
            }
        ));
        assert!(
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .contains_key(&channel_id),
            "guarded miss must leave a reconciler-owned residue"
        );
        let residue = shared
            .turn_finalizer
            .guarded_finish_residues()
            .get(&channel_id)
            .expect("residue asserted present");
        assert_eq!(residue.expected_user_msg_id, terminal_user_msg_id);
        assert_eq!(residue.active_user_msg_id, residual_user_msg_id);
        assert_eq!(residue.reason(), "active_owner_identity_mismatch");
        drop(residue);

        // #5068 r2 (R4): the inflight conjunct was vacuously true in every r1
        // fixture. A live turn-bridge owner holds the release even here, where
        // the terminal episode is proven, and the residue survives for retry.
        let inflight_row = seed_inflight_row(channel_id);
        reconcile_once(&shared).await;
        assert!(
            crate::services::discord::mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_some(),
            "an inflight owner must hold the release despite a proven terminal episode"
        );
        assert!(
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .contains_key(&channel_id),
            "a held residue stays armed for the next reconcile tick"
        );
        std::fs::remove_file(&inflight_row).expect("clear inflight row");

        reconcile_once(&shared).await;

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert!(
            snapshot.cancel_token.is_none(),
            "the same terminal episode must release its residual mailbox owner"
        );
        assert_eq!(
            snapshot
                .intervention_queue
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            vec![MessageId::new(queued_user_msg_id)],
            "recovery must preserve the queued user message"
        );
        assert!(
            shared
                .restart
                .deferred_hook_channels
                .contains_key(&channel_id),
            "residual release must leave a queue kickoff owner"
        );
        assert!(
            !shared
                .turn_finalizer
                .guarded_finish_residues()
                .contains_key(&channel_id)
        );
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    })
    .await;
}

/// #5068 r3 (R5) — the REPORTED incident, and this fix's honest scope boundary.
///
/// In the report (2026-07-31) the guarded miss at `13:19:48` is followed by
/// `13:22:12`–`13:22:36` `tui_direct_pending_start` finding a FOREIGN prior
/// inflight *still live on the same channel*, aborting "without overwriting".
/// Nothing in that window clears the row, so the fixture seeds it BEFORE the
/// finalize and never removes it. Even in the shape most favourable to recovery
/// (the terminal carries the owner's own episode nonce, so `release_authorized`
/// holds and only the inflight conjunct is left), every tick holds: a live
/// turn-bridge owner is precisely the evidence #5176 forbids releasing under,
/// and overriding it is the #5191 double-execution hazard. So this commit gives
/// the residue a recovery OWNER and reclaims the mailbox as soon as the row goes
/// away (pinned by the release test above) — but while the row survives, so does
/// the user-visible symptom. Draining a stranded FOREIGN row is a different
/// owner's job (`STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS` demotion), not this fix.
#[tokio::test(flavor = "current_thread")]
async fn issue_shape_surviving_inflight_row_holds_recovery_on_every_tick() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_400);
        let terminal_user_msg_id = 5_068_401;
        let token = seed_active_with_queue(&shared, channel_id, 5_068_402, 5_068_403).await;
        let nonce = token.turn_nonce().expect("fresh token nonce").to_string();
        let inflight_row = seed_inflight_row(channel_id);

        do_finalize(
            TurnKey::new(channel_id, terminal_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(&terminal_snapshot(terminal_user_msg_id, &nonce)),
            &shared,
        )
        .await;
        assert!(
            inflight_row.exists(),
            "the incident's premise is a row the guarded-miss finalize does NOT clear"
        );

        // The report's escalation budget spans ~24s of 1s reconcile ticks.
        for _ in 0..5 {
            reconcile_once(&shared).await;
        }

        assert!(
            crate::services::discord::mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_some(),
            "MEASURED: while the FOREIGN row survives the reconciler holds every tick, so the \
             reported symptom is NOT recovered by this commit alone"
        );
        assert!(
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .contains_key(&channel_id),
            "the residue stays armed, so recovery is owned rather than abandoned"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn guarded_miss_residue_reconcile_preserves_live_newer_episode() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_200);
        let stale_terminal_user_msg_id = 5_068_201;
        let live_user_msg_id = 5_068_202;
        let queued_user_msg_id = 5_068_203;
        let live_token =
            seed_active_with_queue(&shared, channel_id, live_user_msg_id, queued_user_msg_id).await;
        let stale_terminal_snapshot =
            terminal_snapshot(stale_terminal_user_msg_id, "different-terminal-episode");

        let outcome = do_finalize(
            TurnKey::new(channel_id, stale_terminal_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(&stale_terminal_snapshot),
            &shared,
        )
        .await;
        assert!(matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: None,
                ..
            }
        ));

        reconcile_once(&shared).await;

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            snapshot.active_user_message_id,
            Some(MessageId::new(live_user_msg_id))
        );
        assert!(Arc::ptr_eq(
            snapshot.cancel_token.as_ref().expect("live token survives"),
            &live_token
        ));
        assert!(!live_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(
            !shared
                .turn_finalizer
                .guarded_finish_residues()
                .contains_key(&channel_id),
            "a nonce-proven newer episode consumes the stale residue without being released"
        );
    })
    .await;
}

/// #5068: a residue with NO terminal episode nonce must be held, never released.
///
/// A recovery-restored token carries no persisted nonce, so the mailbox reports
/// `active_turn_nonce = None` and the terminal carries none either. Comparing
/// the two as `Option == Option` reads `None`/`None` as "same episode"
/// (fail-open); `same_terminal_episode` is fail-closed, so nothing here proves a
/// release. This fixture is what makes the "held pending terminal evidence"
/// branch non-vacuous: granting release authority unconditionally — in
/// `reconcile.rs` or inside `release_authorized` — releases a mailbox nobody
/// proved terminal and the assertions below fail on their own.
#[tokio::test(flavor = "current_thread")]
async fn guarded_miss_residue_without_terminal_nonce_is_held() {
    crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel_id = ChannelId::new(5_068_300);
        let terminal_user_msg_id = 5_068_301;
        let token = seed_owner(
            &shared,
            channel_id,
            5_068_302,
            5_068_303,
            Arc::new(CancelToken::from_persisted_turn_nonce(None)),
        )
        .await;
        let mut nonceless_terminal = terminal_snapshot(terminal_user_msg_id, "");
        nonceless_terminal.turn_nonce = None;

        do_finalize(
            TurnKey::new(channel_id, terminal_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            Some(&nonceless_terminal),
            &shared,
        )
        .await;

        let observed = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert!(
            observed.active_turn_nonce.is_none(),
            "this fixture exists to exercise the both-nonces-absent shape"
        );
        reconcile_once(&shared).await;

        let after = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert!(
            Arc::ptr_eq(
                after
                    .cancel_token
                    .as_ref()
                    .expect("a missing terminal nonce must never authorize a release"),
                &token
            ),
            "the residual owner is still the exact token the reconciler refused to release"
        );
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(
            shared
                .turn_finalizer
                .guarded_finish_residues()
                .contains_key(&channel_id),
            "a residue held for want of evidence stays armed instead of being dropped"
        );
    })
    .await;
}
