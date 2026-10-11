//! Explicit operator termination, dormant until an operator entry owns activation.
// Test-only until the operator entry lands; writer gates still scan this file as production.
#![cfg(test)]
use super::turn_finalizer::cleanup::SyntheticClaimSnapshot;
use super::turn_finalizer::{FinalizeContext, FinalizeOutcome, TerminalEvent, TurnKey};
use super::{ChannelId, SharedData};
use crate::db::dispatched_sessions::hosted_execution::{
    self, HostedCasOutcome, HostedExecution, HostedLookup, HostedLookupKey, HostedObservation,
    HostedRecord, HostedState,
};
use crate::services::claude::herdr_turn::{HoldRelease, release_hold};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::{HerdrTarget, herdr_endpoints};
use crate::services::termination_audit::host_terminate::herdr_terminate::{
    HerdrTerminateResult, OperatorTerminateWarrant, terminate_herdr_once,
};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, MailboxUnreachable};
use sqlx::PgPool;
use std::sync::Arc;

pub(crate) struct OperatorTerminate {
    pub(crate) session_key: String,
    pub(crate) execution_nonce: String,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminationResult {
    Refused(String),
    Close(HerdrTerminateResult),
    ProviderSurvivedClose,
    Indeterminate(String),
    Retired,
}
struct CapturedTurn {
    key: TurnKey,
    token: Arc<CancelToken>,
    /// The actor seen holding `token`; settlement is confirmed on it, never on a re-resolved one.
    mailbox: ChannelMailboxHandle,
    snapshot: SyntheticClaimSnapshot,
}
async fn capture_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<Option<CapturedTurn>, String> {
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return Ok(None);
    };
    let state = mailbox
        .try_snapshot()
        .await
        .map_err(|MailboxUnreachable| "mailbox unreadable")?;
    let Some(token) = state.cancel_token else {
        return Ok(None);
    };
    let id = state
        .active_user_message_id
        .ok_or("active turn has no message identity")?
        .get();
    let nonce = state
        .active_turn_nonce
        .filter(|n| !n.is_empty())
        .ok_or("active turn has no nonce")?;
    let row = super::inflight::load_inflight_state_read_only(provider, channel.get())
        .filter(|row| {
            row.effective_finalizer_turn_id() == id && row.turn_nonce.as_deref() == Some(&nonce)
        })
        .ok_or("active turn has no matching captured inflight evidence")?;
    let mut snapshot = SyntheticClaimSnapshot::from_row(&row);
    snapshot.recovery_actor = Some(Arc::downgrade(&token));
    Ok(Some(CapturedTurn {
        key: TurnKey::new(channel, id, shared.restart.current_generation)
            .with_episode_nonce(Some(&nonce)),
        token,
        mailbox,
        snapshot,
    }))
}
async fn read_end(target: HerdrTarget, record: HostedExecution) -> Result<(), TerminationResult> {
    let view = herdr_endpoints()
        .view(&record)
        .ok_or_else(|| TerminationResult::Refused("no local endpoint".into()))?;
    tokio::task::spawn_blocking(move || {
        let pane = view.read_execution();
        if !crate::cli::herdr::confirmed_ended(&record, &pane) {
            return Err(TerminationResult::Indeterminate(format!("pane: {pane:?}")));
        }
        match target.provider_absent() {
            Ok(true) => Ok(()),
            Ok(false) => Err(TerminationResult::ProviderSurvivedClose),
            Err(e) => Err(TerminationResult::Indeterminate(e)),
        }
    })
    .await
    .map_err(|e| TerminationResult::Indeterminate(e.to_string()))?
}
async fn retire_confirmed(
    pool: &PgPool,
    row: &HostedObservation,
    record: &HostedExecution,
) -> Result<(), TerminationResult> {
    match hosted_execution::retire_pg(pool, row, &record.owner, &record.execution_nonce).await {
        Ok(HostedCasOutcome::Written) => Ok(()),
        Ok(HostedCasOutcome::Stale) => {
            Err(TerminationResult::Indeterminate("retire CAS stale".into()))
        }
        Err(e) => Err(TerminationResult::Indeterminate(format!(
            "retire CAS: {e:?}"
        ))),
    }
}

#[cfg(test)]
type SettlementWindowProbe =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>;
#[cfg(test)]
type ProbeSlot = std::cell::RefCell<Option<SettlementWindowProbe>>;
#[cfg(test)]
thread_local! {
    static SETTLEMENT_WINDOW: ProbeSlot = const { std::cell::RefCell::new(None) };
    static AFTER_SETTLEMENT: ProbeSlot = const { std::cell::RefCell::new(None) };
}
/// Runs once between the hold release and the captured-turn settlement on this thread.
#[cfg(test)]
pub(crate) fn probe_settlement_window(probe: SettlementWindowProbe) {
    SETTLEMENT_WINDOW.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once after the captured-turn settlement, while the exclusion is still held.
#[cfg(test)]
pub(crate) fn probe_after_settlement(probe: SettlementWindowProbe) {
    AFTER_SETTLEMENT.with(|slot| *slot.borrow_mut() = Some(probe));
}
#[cfg(test)]
async fn run_probe(slot: &'static std::thread::LocalKey<ProbeSlot>) {
    if let Some(probe) = slot.with(|slot| slot.borrow_mut().take()) {
        probe().await;
    }
}

/// `exclusion` (channel transition guard and execution fence) drops only after settlement, so
/// input arriving once the hold is gone waits in the queue instead of reaching the pane.
async fn settle_and_release(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    nonce: &str,
    turn: Option<CapturedTurn>,
    exclusion: impl Sized,
) -> TerminationResult {
    if release_hold(nonce) != HoldRelease::Released {
        return TerminationResult::Indeterminate("retired but hold release incomplete".into());
    }
    #[cfg(test)]
    run_probe(&SETTLEMENT_WINDOW).await;
    let result = settle_captured(shared, provider, turn).await;
    #[cfg(test)]
    run_probe(&AFTER_SETTLEMENT).await;
    drop(exclusion);
    result
}

async fn settle_captured(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    turn: Option<CapturedTurn>,
) -> TerminationResult {
    let Some(CapturedTurn {
        key,
        token,
        mailbox,
        snapshot,
    }) = turn
    else {
        return TerminationResult::Retired;
    };
    let outcome = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            key,
            provider.clone(),
            TerminalEvent::HostTerminated,
            FinalizeContext::host_terminated(),
            Some(snapshot),
            shared.clone(),
        )
        .await;
    let confirmed = match outcome {
        FinalizeOutcome::Finalized {
            removed_token: Some(removed),
            ..
        } if Arc::ptr_eq(&removed, &token) => Ok(()),
        FinalizeOutcome::AlreadyFinalized => {
            confirm_captured_released(shared, key.channel_id, &mailbox, &token).await
        }
        _ => Err("captured turn settlement unconfirmed"),
    };
    match confirmed {
        Ok(()) => TerminationResult::Retired,
        // The CAS and hold release already happened; this reports that, it does not undo them.
        Err(why) => TerminationResult::Indeterminate(format!("retired, hold released; {why}")),
    }
}

/// `AlreadyFinalized` also covers a finalizer that failed before releasing `token`, so success
/// needs the captured actor to answer without it and still be the channel's actor.
async fn confirm_captured_released(
    shared: &SharedData,
    channel: ChannelId,
    mailbox: &ChannelMailboxHandle,
    token: &Arc<CancelToken>,
) -> Result<(), &'static str> {
    match mailbox.cancel_token().await {
        Err(MailboxUnreachable) => return Err("captured mailbox unreadable"),
        Ok(Some(active)) if Arc::ptr_eq(&active, token) => {
            return Err("captured turn still active");
        }
        Ok(_) => {}
    }
    match shared.mailbox_peek(channel) {
        Some(current) if current.same_actor(mailbox) => Ok(()),
        _ => Err("captured mailbox replaced"),
    }
}
/// No production entry calls this until the operator activation slice is approved.
pub(crate) async fn terminate_explicit_herdr(
    request: OperatorTerminate,
    shared: Arc<SharedData>,
    pool: &PgPool,
) -> TerminationResult {
    if !crate::config_live_reload::current()
        .is_some_and(|c| c.runtime.herdr_terminate_enabled == Some(true))
    {
        return TerminationResult::Refused("terminate disabled".into());
    }
    let lookup = load(pool, &request.session_key).await;
    let row = match lookup {
        Ok(row) => row,
        Err(result) => return result,
    };
    let HostedRecord::Known(record) = &row.record else {
        return TerminationResult::Refused("unknown execution".into());
    };
    if record.execution_nonce != request.execution_nonce
        || record.state == HostedState::Retired
        || record.owner.discord_token_hash != shared.token_hash
        || Some(record.owner.owner_node.clone()) != crate::config::session_hosts::local_node()
        || record
            .location
            .as_ref()
            .is_none_or(|l| l.execution_node != record.owner.owner_node)
    {
        return TerminationResult::Refused("execution identity differs".into());
    }
    let Some(provider) = ProviderKind::from_str(&record.owner.provider) else {
        return TerminationResult::Refused("unknown provider".into());
    };
    let Ok(channel) = record.owner.channel_id.parse::<u64>() else {
        return TerminationResult::Refused("unknown channel".into());
    };
    if channel == 0 {
        return TerminationResult::Refused("unknown channel".into());
    }
    let channel = ChannelId::new(channel);
    let turn = match capture_turn(&shared, &provider, channel).await {
        Ok(turn) => turn,
        Err(e) => return TerminationResult::Refused(e),
    };
    let Ok(transition) = shared.acquire_session_transition(channel).await else {
        return TerminationResult::Refused("transition busy".into());
    };
    // An unreadable or replaced actor is a changed turn, never an idle channel.
    let same_turn = match (&turn, shared.mailbox_peek(channel)) {
        (None, None) => true,
        (None, Some(now)) => matches!(now.cancel_token().await, Ok(None)),
        (Some(turn), Some(now)) => {
            now.same_actor(&turn.mailbox)
                && matches!(now.cancel_token().await, Ok(Some(active)) if Arc::ptr_eq(&active, &turn.token))
        }
        (Some(_), None) => false,
    };
    if !same_turn {
        return TerminationResult::Refused("turn changed under transition".into());
    }
    let Some(target) = herdr_endpoints().target(record) else {
        return TerminationResult::Refused("missing launch evidence".into());
    };
    let fence = match target.termination_fence() {
        Ok(fence) => Arc::new(fence),
        Err(e) => return TerminationResult::Refused(format!("{e:?}")),
    };
    if load(pool, &request.session_key).await.as_ref() != Ok(&row) {
        fence.reopen();
        return TerminationResult::Refused("row changed under fence".into());
    }
    let read_target = target.clone();
    match tokio::task::spawn_blocking(move || read_target.revalidate_termination_reads()).await {
        Ok(Ok(())) => {}
        other => {
            fence.reopen();
            return TerminationResult::Refused(format!("termination provenance: {other:?}"));
        }
    }
    let view = herdr_endpoints().view(record).unwrap();
    let pane = match tokio::task::spawn_blocking(move || view.read_execution()).await {
        Ok(pane) => pane,
        Err(e) => {
            fence.reopen();
            return TerminationResult::Indeterminate(e.to_string());
        }
    };
    if !crate::cli::herdr::confirmed_ended(record, &pane) {
        // Blocking socket work remains outside the async executor; the fence stays owned here.
        let close_target = target.clone();
        let close_fence = fence.clone();
        let result = match tokio::task::spawn_blocking(move || {
            terminate_herdr_once(OperatorTerminateWarrant::issue_fenced(
                close_target,
                &close_fence,
            ))
        })
        .await
        {
            Ok(result) => result,
            Err(e) => return TerminationResult::Indeterminate(e.to_string()),
        };
        if !matches!(
            result,
            HerdrTerminateResult::Acknowledged | HerdrTerminateResult::Indeterminate(_)
        ) {
            fence.reopen();
            return TerminationResult::Close(result);
        }
    }
    if let Err(result) = read_end(target, record.clone()).await {
        return result;
    }
    if let Err(result) = retire_confirmed(pool, &row, record).await {
        return result;
    }
    settle_and_release(
        &shared,
        &provider,
        &record.execution_nonce,
        turn,
        (transition, fence),
    )
    .await
}
async fn load(pool: &PgPool, key: &str) -> Result<HostedObservation, TerminationResult> {
    match hosted_execution::load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(row) => Ok(row),
        HostedLookup::Missing => Err(TerminationResult::Refused("missing row".into())),
        HostedLookup::Conflict(e) => Err(TerminationResult::Refused(format!("conflict: {e:?}"))),
        HostedLookup::Unknown(e) => Err(TerminationResult::Refused(format!("unknown: {e}"))),
    }
}

/// Queue access for settlement-window tests that live outside the discord module.
#[cfg(test)]
pub(crate) mod test_queue {
    use super::{Arc, ChannelId, ProviderKind, SharedData};
    use serenity::model::id::{MessageId, UserId};

    pub(crate) const ORIGINAL: &str = "original input preserved";

    pub(crate) async fn queue_original(shared: &Arc<SharedData>, channel: ChannelId) {
        let provider = ProviderKind::Claude;
        let prompt = super::super::Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: MessageId::new(61),
            queued_generation: super::super::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(61)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: ORIGINAL.into(),
            mode: super::super::InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        shared
            .mailbox(channel)
            .replace_queue(
                vec![prompt],
                super::super::queue_persistence_context(shared, &provider, channel),
            )
            .await;
    }

    /// The idle-queue dequeue a kickoff performs; `None` means nothing may start yet.
    pub(crate) async fn take_queued(
        shared: &Arc<SharedData>,
        channel: ChannelId,
    ) -> Option<String> {
        super::super::idle_queue_take_next_soft_if_ready(shared, &ProviderKind::Claude, channel)
            .await
            .intervention
            .map(|intervention| intervention.text)
    }

    pub(crate) async fn queued_texts(shared: &Arc<SharedData>, channel: ChannelId) -> Vec<String> {
        let state = shared.mailbox(channel).snapshot().await;
        state
            .intervention_queue
            .iter()
            .map(|i| i.text.clone())
            .collect()
    }
}

/// An active turn for settlement tests that live outside the discord module.
#[cfg(test)]
pub(crate) mod test_turn {
    use super::super::inflight::{InflightTurnState, RelayOwnerKind, save_inflight_state};
    use super::super::turn_finalizer::CompletionAdmissionPlan;
    use super::{
        Arc, CancelToken, ChannelId, FinalizeContext, FinalizeOutcome, ProviderKind, SharedData,
        TerminalEvent, TurnKey,
    };
    use serenity::model::id::{MessageId, UserId};
    use std::sync::atomic::Ordering;

    /// Turn `nonce` holds `channel` on message `id`, with matching inflight evidence and a live
    /// finalizer entry, as a running watcher-owned turn leaves them.
    pub(crate) async fn start(
        shared: &Arc<SharedData>,
        channel: ChannelId,
        id: u64,
        nonce: &str,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(nonce.into())));
        shared
            .mailbox(channel)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(id))
            .await;
        shared.restart.global_active.fetch_add(1, Ordering::Relaxed);
        shared
            .turn_finalizer
            .register_start_with_completion_admission(
                key(shared, channel, id, nonce),
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                shared,
            );
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel.get(),
            None,
            7,
            id,
            id + 1,
            "original prompt".into(),
            None,
            None,
            None,
            None,
            0,
        );
        row.turn_nonce = Some(nonce.into());
        save_inflight_state(&row).unwrap();
        token
    }

    fn key(shared: &SharedData, channel: ChannelId, id: u64, nonce: &str) -> TurnKey {
        TurnKey::new(channel, id, shared.restart.current_generation).with_episode_nonce(Some(nonce))
    }

    /// A natural terminal settles turn `nonce`, releasing exactly `token`.
    pub(crate) async fn finish_naturally(
        shared: &Arc<SharedData>,
        channel: ChannelId,
        id: u64,
        nonce: &str,
        token: &Arc<CancelToken>,
    ) {
        let outcome = shared
            .turn_finalizer
            .submit_terminal(
                key(shared, channel, id, nonce),
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;
        assert!(
            matches!(&outcome, FinalizeOutcome::Finalized { removed_token: Some(t), .. } if Arc::ptr_eq(t, token)),
            "the natural terminal releases exactly this turn"
        );
    }

    /// The channel's active token; a mailbox that does not answer fails the test.
    pub(crate) async fn active(
        shared: &SharedData,
        channel: ChannelId,
    ) -> Option<Arc<CancelToken>> {
        let mailbox = shared.mailbox_peek(channel)?;
        mailbox.cancel_token().await.expect("mailbox answers")
    }

    /// Whether the channel's registered mailbox actor still answers a read.
    pub(crate) async fn answers(shared: &SharedData, channel: ChannelId) -> bool {
        let Some(mailbox) = shared.mailbox_peek(channel) else {
            return false;
        };
        mailbox.try_snapshot().await.is_ok()
    }

    /// The persisted inflight turn's message id and nonce.
    pub(crate) fn inflight(channel: ChannelId) -> Option<(u64, Option<String>)> {
        super::super::inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get())
            .map(|row| (row.effective_finalizer_turn_id(), row.turn_nonce))
    }

    /// The next finalize on this thread fails before releasing its token.
    pub(crate) fn arm_settlement_panic() {
        super::super::turn_finalizer::arm_finalize_panic_once_for_test();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::model::id::{MessageId, UserId};
    use std::sync::atomic::Ordering;

    #[test]
    fn m1_disabled_service_has_zero_io() {
        let _lock = crate::config::shared_test_env_lock();
        let _root = crate::config::TestRuntimeRootGuard::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = super::super::make_shared_data_for_tests();
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://postgres@127.0.0.1:1/absent")
                    .unwrap();
                let result = terminate_explicit_herdr(
                    OperatorTerminate {
                        session_key: "missing".into(),
                        execution_nonce: "nonce".into(),
                    },
                    shared,
                    &pool,
                )
                .await;
                assert_eq!(
                    result,
                    TerminationResult::Refused("terminate disabled".into())
                );
                pool.close().await;
            });
    }

    #[tokio::test]
    async fn m1_retire_cas_failure_preserves_successor_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let channel = "5340115";
        let owner = hosted_execution::tests::owner(channel);
        let record = hosted_execution::tests::record(&owner, "execution-cas", HostedState::Bound);
        let key = "claude/discord_0123456789abcdef/test-node:AgentDesk-claude-m1cas";
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
            .bind(key).bind(&owner.discord_token_hash).bind(channel)
            .bind(serde_json::to_value(&record).unwrap()).execute(&pool).await.unwrap();
        let row = load(&pool, key).await.unwrap();
        sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
            .bind(key)
            .bind(
                serde_json::to_value(hosted_execution::tests::record(
                    &owner,
                    "successor-cas",
                    HostedState::Bound,
                ))
                .unwrap(),
            )
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            retire_confirmed(&pool, &row, &record).await,
            Err(TerminationResult::Indeterminate(_))
        ));
        let live = load(&pool, key).await.unwrap();
        assert!(
            matches!(live.record, HostedRecord::Known(r) if r.state == HostedState::Bound && r.execution_nonce == "successor-cas")
        );
        pool.close().await;
        db.drop().await;
    }

    #[test]
    fn m1_hold_release_window_preserves_input_until_settlement() {
        let _lock = crate::config::shared_test_env_lock();
        let _root = crate::config::TestRuntimeRootGuard::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = super::super::make_shared_data_for_tests();
                let channel = ChannelId::new(5_340_114);
                let transition = shared.session_transition_lock(channel).lock_owned().await;
                crate::services::claude::herdr_turn::hold("execution-window").unwrap();
                test_queue::queue_original(&shared, channel).await;
                let opened = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (probe_shared, probe_opened) = (shared.clone(), opened.clone());
                probe_settlement_window(Box::new(move || {
                    Box::pin(async move {
                        assert!(
                            crate::services::claude::herdr_turn::input_holds()
                                .unwrap()
                                .is_empty(),
                            "the window opens only after the hold is gone"
                        );
                        assert_eq!(
                            test_queue::take_queued(&probe_shared, channel).await,
                            None,
                            "input arriving before settlement must not start"
                        );
                        assert_eq!(
                            test_queue::queued_texts(&probe_shared, channel).await,
                            vec![test_queue::ORIGINAL.to_string()]
                        );
                        probe_opened.store(true, Ordering::Relaxed);
                    })
                }));
                assert_eq!(
                    settle_and_release(
                        &shared,
                        &ProviderKind::Claude,
                        "execution-window",
                        None,
                        transition
                    )
                    .await,
                    TerminationResult::Retired
                );
                assert!(opened.load(Ordering::Relaxed));
                assert_eq!(
                    test_queue::take_queued(&shared, channel).await.as_deref(),
                    Some(test_queue::ORIGINAL)
                );
                assert_eq!(test_queue::take_queued(&shared, channel).await, None);
            });
    }

    #[test]
    fn m1_host_terminated_settles_exact_turn_and_publishes_queue_once() {
        let _lock = crate::config::shared_test_env_lock();
        let _root = crate::config::TestRuntimeRootGuard::new();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let shared = super::super::make_shared_data_for_tests();
            let channel = ChannelId::new(5_340_113);
            let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some("turn-a".into())));
            shared.mailbox(channel).restore_active_turn(token.clone(), UserId::new(7), MessageId::new(51)).await;
            shared.restart.global_active.store(1, Ordering::Relaxed);
            let key = TurnKey::new(channel, 51, shared.restart.current_generation).with_episode_nonce(Some("turn-a"));
            shared.turn_finalizer.register_start_with_completion_admission(key, ProviderKind::Claude,
                super::super::inflight::RelayOwnerKind::Watcher,
                super::super::turn_finalizer::CompletionAdmissionPlan::AfterTerminalProjectionAndDispositionSettled,
                &shared);
            let mut row = super::super::inflight::InflightTurnState::new(ProviderKind::Claude,
                channel.get(), None, 7, 51, 52, "original prompt".into(), None, None, None, None, 0);
            row.turn_nonce = Some("turn-a".into());
            super::super::inflight::save_inflight_state(&row).unwrap();
            let captured = capture_turn(&shared, &ProviderKind::Claude, channel).await.unwrap();
            crate::services::claude::herdr_turn::hold("execution-a").unwrap();
            let mut events = super::super::turn_completion_events::subscribe_turn_completion_events(&shared);
            test_queue::queue_original(&shared, channel).await;
            let transition = shared.session_transition_lock(channel).lock_owned().await;
            let probe_shared = shared.clone();
            probe_settlement_window(Box::new(move || Box::pin(async move {
                assert!(probe_shared.session_transition_lock(channel).try_lock_owned().is_err(), "transition stays owned until settlement");
                assert_eq!(test_queue::take_queued(&probe_shared, channel).await, None);
            })));
            assert_eq!(settle_and_release(&shared, &ProviderKind::Claude, "execution-a", captured, transition).await, TerminationResult::Retired);
            assert!(crate::services::claude::herdr_turn::input_holds().unwrap().is_empty());
            let mut eligible = 0;
            while let Ok(event) = events.try_recv() { if event.queue_is_eligible() { eligible += 1; } }
            assert_eq!(eligible, 1, "host settlement authorizes exactly one queue edge despite missing output barriers");
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
            assert!(shared.mailbox(channel).snapshot().await.cancel_token.is_none());
            assert!(!token.cancelled.load(Ordering::Relaxed));
            assert_eq!(test_queue::take_queued(&shared, channel).await.as_deref(), Some(test_queue::ORIGINAL), "the settled channel kicks its queue exactly once");
            assert_eq!(test_queue::take_queued(&shared, channel).await, None);
        });
    }

    // A natural terminal settled A and B took the channel before the late HostTerminated for A:
    // that duplicate settles nothing of B and still reports the retire.
    #[test]
    fn m2_natural_a_then_b_late_settlement_preserves_b() {
        let _lock = crate::config::shared_test_env_lock();
        let _root = crate::config::TestRuntimeRootGuard::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = super::super::make_shared_data_for_tests();
                let channel = ChannelId::new(5_340_116);
                let token_a = test_turn::start(&shared, channel, 51, "turn-a").await;
                let captured = capture_turn(&shared, &ProviderKind::Claude, channel)
                    .await
                    .unwrap();
                crate::services::claude::herdr_turn::hold("execution-a").unwrap();
                crate::services::claude::herdr_turn::hold("execution-b").unwrap();
                let transition = shared.session_transition_lock(channel).lock_owned().await;
                let token_b = Arc::new(std::sync::Mutex::new(None));
                let (probe_shared, probe_a, probe_b) =
                    (shared.clone(), token_a.clone(), token_b.clone());
                probe_settlement_window(Box::new(move || {
                    Box::pin(async move {
                        test_turn::finish_naturally(&probe_shared, channel, 51, "turn-a", &probe_a)
                            .await;
                        let b = test_turn::start(&probe_shared, channel, 61, "turn-b").await;
                        *probe_b.lock().unwrap() = Some(b);
                    })
                }));
                assert_eq!(
                    settle_and_release(
                        &shared,
                        &ProviderKind::Claude,
                        "execution-a",
                        captured,
                        transition
                    )
                    .await,
                    TerminationResult::Retired
                );
                let token_b = token_b
                    .lock()
                    .unwrap()
                    .take()
                    .expect("B started in the window");
                let active = test_turn::active(&shared, channel).await.expect("B active");
                assert!(Arc::ptr_eq(&active, &token_b));
                assert!(!token_b.cancelled.load(Ordering::Relaxed));
                assert_eq!(token_b.turn_nonce(), Some("turn-b"));
                assert_eq!(
                    test_turn::inflight(channel),
                    Some((61, Some("turn-b".into())))
                );
                let holds = crate::services::claude::herdr_turn::input_holds().unwrap();
                assert!(holds.iter().any(|(nonce, _)| nonce == "execution-b"));
                assert!(!holds.iter().any(|(nonce, _)| nonce == "execution-a"));
                assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            });
    }

    // A mailbox actor that never answers is not an idle channel: capture refuses instead of
    // proceeding as if no turn were running.
    #[test]
    fn m2_unreadable_mailbox_is_never_an_idle_channel() {
        let _lock = crate::config::shared_test_env_lock();
        let _root = crate::config::TestRuntimeRootGuard::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let shared = super::super::make_shared_data_for_tests();
                let channel = ChannelId::new(5_340_117);
                shared.mailboxes.insert_unreachable_for_test(channel);
                assert!(matches!(
                    capture_turn(&shared, &ProviderKind::Claude, channel).await,
                    Err(why) if why == "mailbox unreadable"
                ));
            });
    }
}
