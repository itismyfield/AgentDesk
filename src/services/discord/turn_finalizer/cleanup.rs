use std::sync::Arc;

use crate::services::provider::ProviderKind;

use super::{FinalizeContext, TerminalEvent, TurnKey};
use crate::services::discord::SharedData;

#[derive(Clone, Copy)]
struct ReactionCleanupRequest {
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    add_checkmark: bool,
    source: &'static str,
}

/// Backstop-only reaction cleanup for terminal paths that skipped the normal
/// watcher `⏳ -> ✅` block.
///
/// Reachable production matrix:
/// * `watcher` / `bridge` / `monitor` submitters pass `clear_inflight = false`,
///   so they never run this helper; watcher owns the normal committed-output
///   reaction block when it is safe to claim completion.
/// * `gate_backstop()` is not a production `submit_terminal` context. It is
///   reached only by `run_backstop_finalize -> do_finalize` after a deferred
///   busy-pane gate, where no caller remains to clear inflight or the reaction.
/// * the no-owner restored-watcher path mutates watcher context into the same
///   `clear_inflight && kickoff_queue && !completion_cleanup && !voice` shape.
///   That path also skipped the normal watcher block, so it needs this fallback.
/// * `AlreadyFinalized` losers only inherit their submitter context, so they
///   cannot become the backstop reaction owner after someone else won the gate.
pub(super) fn finalized_reaction_lifecycle(
    key: TurnKey,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
    source: &'static str,
) {
    if !ctx.clear_inflight
        || !ctx.kickoff_queue
        || ctx.allow_completion_cleanup
        || ctx.drain_voice
        || key.user_msg_id == 0
    {
        return;
    }
    let message_id = serenity::model::id::MessageId::new(key.user_msg_id);
    schedule_reaction_cleanup(
        shared.clone(),
        ReactionCleanupRequest {
            channel_id: key.channel_id,
            message_id,
            add_checkmark: !matches!(event, TerminalEvent::Cancel),
            source,
        },
    );
}

/// Late `AlreadyFinalized` losers still perform guarded active-state cleanup.
/// This is intentionally narrower than `do_finalize`: only the same real turn id
/// can lose mailbox/inflight state, so a newer active turn is preserved.
pub(super) async fn already_finalized_active_state(
    key: TurnKey,
    provider: &ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) {
    if key.user_msg_id == 0 {
        return;
    }

    let _ = crate::services::discord::inflight::clear_inflight_state_if_matches(
        provider,
        key.channel_id.get(),
        key.user_msg_id,
    );

    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        key.channel_id,
        serenity::model::id::MessageId::new(key.user_msg_id),
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        return;
    };

    if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
        token.mark_completion_cleanup();
    }
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::saturating_decrement_global_active(shared);
    super::super::clear_watchdog_deadline_override(key.channel_id.get()).await;
    shared
        .dispatch_thread_parents
        .retain(|_, thread| *thread != key.channel_id);
    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&key.channel_id);
    }
}

#[cfg(not(test))]
fn schedule_reaction_cleanup(shared: Arc<SharedData>, request: ReactionCleanupRequest) {
    super::super::task_supervisor::spawn_observed("turn_finalizer_reaction_cleanup", async move {
        apply_reaction(
            &shared,
            request.channel_id,
            request.message_id,
            '⏳',
            false,
            request.source,
        )
        .await;
        if request.add_checkmark {
            apply_reaction(
                &shared,
                request.channel_id,
                request.message_id,
                '✅',
                true,
                request.source,
            )
            .await;
        }
    });
}

#[cfg(not(test))]
async fn apply_reaction(
    shared: &Arc<SharedData>,
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            channel = channel_id.get(),
            message = message_id.get(),
            emoji = %emoji,
            source,
            "turn finalizer reaction cleanup skipped; provider serenity http unavailable"
        );
        return;
    };
    if add {
        super::super::formatting::add_reaction_raw(&http, channel_id, message_id, emoji).await;
    } else {
        super::super::formatting::remove_reaction_raw(&http, channel_id, message_id, emoji).await;
    }
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReactionCleanupRecord {
    pub(super) channel_id: u64,
    pub(super) message_id: u64,
    pub(super) emoji: char,
    pub(super) add: bool,
    pub(super) source: &'static str,
}

#[cfg(test)]
static REACTION_CLEANUP_RECORDS: std::sync::LazyLock<
    std::sync::Mutex<Option<Vec<ReactionCleanupRecord>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
pub(super) fn begin_reaction_cleanup_recording() {
    *REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock") = Some(Vec::new());
}

#[cfg(test)]
pub(super) fn take_reaction_cleanup_records() -> Vec<ReactionCleanupRecord> {
    REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .take()
        .unwrap_or_default()
}

#[cfg(test)]
fn schedule_reaction_cleanup(_shared: Arc<SharedData>, request: ReactionCleanupRequest) {
    record_reaction(
        request.channel_id,
        request.message_id,
        '⏳',
        false,
        request.source,
    );
    if request.add_checkmark {
        record_reaction(
            request.channel_id,
            request.message_id,
            '✅',
            true,
            request.source,
        );
    }
}

#[cfg(test)]
fn record_reaction(
    channel_id: serenity::model::id::ChannelId,
    message_id: serenity::model::id::MessageId,
    emoji: char,
    add: bool,
    source: &'static str,
) {
    if let Some(records) = REACTION_CLEANUP_RECORDS
        .lock()
        .expect("reaction cleanup recorder lock")
        .as_mut()
    {
        records.push(ReactionCleanupRecord {
            channel_id: channel_id.get(),
            message_id: message_id.get(),
            emoji,
            add,
            source,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        FinalizeContext, FinalizeOutcome, GATE_BACKSTOP, RECONCILE_INTERVAL, TerminalEvent,
        TurnFinalizer, TurnKey,
    };
    use super::*;
    use crate::services::discord::inflight::RelayOwnerKind;
    use crate::services::provider::{CancelToken, ProviderKind};
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn test_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
    }

    fn with_isolated_runtime_root(f: impl FnOnce()) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let tmp = tempfile::tempdir().expect("create temp runtime dir for reaction cleanup test");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path().to_str().unwrap());
        }
        f();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }

    async fn seed_active_turn(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        user_msg_id: u64,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(channel_id)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(user_msg_id))
            .await;
        token
    }

    fn recorded_actions(records: &[ReactionCleanupRecord]) -> Vec<(u64, u64, char, bool)> {
        records
            .iter()
            .map(|record| {
                (
                    record.channel_id,
                    record.message_id,
                    record.emoji,
                    record.add,
                )
            })
            .collect()
    }

    #[test]
    fn reconciler_backstop_finalize_removes_hourglass_and_marks_complete() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_100);
                let tid = 3_334_101_u64;
                shared
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let deferred = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(deferred, FinalizeOutcome::Deferred));

                tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
                tokio::task::yield_now().await;

                let records = take_reaction_cleanup_records();
                assert_eq!(
                    recorded_actions(&records),
                    vec![(ch.get(), tid, '⏳', false), (ch.get(), tid, '✅', true)]
                );
                assert!(records.iter().all(|record| record.source == "finalized"));
            });
        });
    }

    #[test]
    fn already_finalized_loser_does_not_claim_reaction_cleanup() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_200);
                let tid = 3_334_201_u64;
                shared
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let first = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(first, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());

                begin_reaction_cleanup_recording();
                let late = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::bridge(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
                assert!(
                    take_reaction_cleanup_records().is_empty(),
                    "reachable AlreadyFinalized losers inherit watcher/bridge/monitor context and must not masquerade as the backstop reaction owner"
                );
            });
        });
    }

    #[test]
    fn watcher_context_skips_extra_reaction_calls() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_400);
                let tid = 3_334_401_u64;
                shared
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _token = seed_active_turn(&shared, ch, tid).await;
                let fin = TurnFinalizer::spawn();
                let key = TurnKey::new(ch, tid, 0);
                fin.register_start(key, ProviderKind::Claude, RelayOwnerKind::Watcher, &shared);

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        key,
                        ProviderKind::Claude,
                        TerminalEvent::Complete,
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }

    #[test]
    fn cleanup_targets_turn_identity_and_skips_synthetic_id_zero() {
        with_isolated_runtime_root(|| {
            test_rt().block_on(async {
                let shared =
                    super::super::super::make_shared_data_for_tests_with_storage(None, None);
                let ch = ChannelId::new(3_334_500);
                let old_tid = 3_334_501_u64;
                let newer_tid = 3_334_502_u64;
                shared
                    .global_active
                    .store(1, std::sync::atomic::Ordering::Relaxed);
                let _newer = seed_active_turn(&shared, ch, newer_tid).await;
                let fin = TurnFinalizer::spawn();

                begin_reaction_cleanup_recording();
                let outcome = fin
                    .submit_terminal(
                        TurnKey::new(ch, old_tid, 0),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(
                    outcome,
                    FinalizeOutcome::Finalized {
                        removed_token: None,
                        ..
                    }
                ));
                let records = take_reaction_cleanup_records();
                assert_eq!(recorded_actions(&records).len(), 2);
                assert!(records.iter().all(|record| record.message_id == old_tid));
                assert!(records.iter().all(|record| record.message_id != newer_tid));
                assert!(shared.mailbox(ch).has_active_turn().await);

                begin_reaction_cleanup_recording();
                let zero = fin
                    .submit_terminal(
                        TurnKey::new(ChannelId::new(3_334_600), 0, 0),
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(zero, FinalizeOutcome::Finalized { .. }));
                assert!(take_reaction_cleanup_records().is_empty());
            });
        });
    }
}
