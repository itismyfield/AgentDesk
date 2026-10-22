//! Explicit termination of one Herdr execution, by the operator route, a user clear or an HTTP
//! force cancel.
pub(crate) mod http_force;
mod retired_exclusion;
pub(crate) mod root_replaced;

pub(crate) use retired_exclusion::RetiredExclusion;

use super::health::HealthRegistry;
use super::turn_finalizer::cleanup::SyntheticClaimSnapshot;
use super::turn_finalizer::{FinalizeContext, FinalizeOutcome, TerminalEvent, TurnKey};
use super::{ChannelId, SharedData};
use crate::db::dispatched_sessions::hosted_execution::{
    self, HostedCasOutcome, HostedExecution, HostedLookup, HostedLookupKey, HostedObservation,
    HostedRecord, HostedState,
};
use crate::services::claude::herdr_turn::{HoldRelease, release_hold};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::session_host::TerminationFence;
use crate::services::session_host::{HerdrTarget, herdr_endpoints};
use crate::services::termination_audit::host_terminate::herdr_terminate::{
    HerdrTerminateResult, OperatorTerminateWarrant, TerminateRefusal, terminate_herdr_once,
};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, MailboxUnreachable};
use futures::FutureExt;
use sqlx::PgPool;
use std::sync::Arc;

pub(crate) struct OperatorTerminate {
    pub(crate) session_key: String,
    pub(crate) execution_nonce: String,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TerminationResult {
    /// The live switch is off; nothing was read or written.
    Disabled,
    Refused(String),
    Close(HerdrTerminateResult),
    ProviderSurvivedClose,
    Indeterminate(String),
    Retired,
}

impl TerminationResult {
    /// The result's name and detail, shared by the operator route's reply and the result log.
    pub(crate) fn outcome(&self) -> (&'static str, String) {
        use HerdrTerminateResult as Close;
        match self {
            Self::Disabled | Self::Close(Close::Refused(TerminateRefusal::Disabled)) => {
                ("disabled", "terminate disabled".into())
            }
            Self::Refused(why) => ("refused", why.clone()),
            Self::Close(Close::Refused(TerminateRefusal::Gate(gate))) => {
                ("gate_refused", format!("{gate:?}"))
            }
            Self::Close(Close::NotSent(why)) => ("not_sent", why.clone()),
            Self::Close(Close::ConfirmationRequired) => (
                "confirmation_required",
                "herdr asks for a confirmation the service never gives".into(),
            ),
            Self::Close(Close::Acknowledged) => {
                ("indeterminate", "close acknowledged, run unfinished".into())
            }
            Self::Close(Close::Indeterminate(why)) | Self::Indeterminate(why) => {
                ("indeterminate", why.clone())
            }
            Self::ProviderSurvivedClose => (
                "provider_survived_close",
                "the provider outlived the close".into(),
            ),
            Self::Retired => ("retired", "retired".into()),
        }
    }
}

/// The one live-switch read the operator route, its adapter, a forced HTTP cancel and the
/// service share.
pub(crate) fn terminate_enabled() -> bool {
    crate::config_live_reload::current()
        .is_some_and(|config| config.runtime.herdr_terminate_enabled == Some(true))
}

/// Reopens the fence when the run ends before it could have sent its close, a panic included;
/// once disarmed, the fence stays shut as the close may have reached Herdr.
struct UnsentClose(Option<Arc<TerminationFence>>);
impl UnsentClose {
    fn disarm(mut self) {
        self.0 = None;
    }
}
impl Drop for UnsentClose {
    fn drop(&mut self) {
        if let Some(fence) = self.0.take() {
            fence.reopen();
        }
    }
}
pub(super) struct CapturedTurn {
    key: TurnKey,
    pub(super) token: Arc<CancelToken>,
    /// The actor seen holding `token`; settlement is confirmed on it, never on a re-resolved one.
    mailbox: ChannelMailboxHandle,
    snapshot: SyntheticClaimSnapshot,
    dispatch_id: Option<String>,
}
pub(super) async fn capture_turn(
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
    let dispatch_id = row.dispatch_id.clone();
    Ok(Some(CapturedTurn {
        key: TurnKey::new(channel, id, shared.restart.current_generation)
            .with_episode_nonce(Some(&nonce)),
        token,
        mailbox,
        snapshot,
        dispatch_id,
    }))
}
async fn read_end(target: HerdrTarget, record: HostedExecution) -> Result<(), TerminationResult> {
    // The close may have been applied, so a lost endpoint leaves the outcome open.
    let view = herdr_endpoints().view(&record).ok_or_else(|| {
        TerminationResult::Indeterminate("no local endpoint after the close".into())
    })?;
    tokio::task::spawn_blocking(move || {
        let pane = view.read_execution();
        if !view.ended(&record, &pane) {
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
/// What a fresh read of a stored execution shows; nothing is closed or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecutionEnd {
    /// The pane is gone or left to a lone shell the execution ended under; its provider is gone.
    Ended,
    /// The pane no longer runs it, but the provider process lives.
    ProviderAlive,
    /// The pane still runs it, or a read failed.
    Unconfirmed,
}

/// `read_end` for a caller that sent no close.
pub(super) async fn execution_end(target: HerdrTarget, record: HostedExecution) -> ExecutionEnd {
    match read_end(target, record).await {
        Ok(()) => ExecutionEnd::Ended,
        Err(TerminationResult::ProviderSurvivedClose) => ExecutionEnd::ProviderAlive,
        Err(_) => ExecutionEnd::Unconfirmed,
    }
}
pub(super) async fn retire_confirmed(
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
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;
#[cfg(test)]
type ProbeSlot = std::cell::RefCell<Option<SettlementWindowProbe>>;
#[cfg(test)]
thread_local! {
    static SETTLEMENT_WINDOW: ProbeSlot = const { std::cell::RefCell::new(None) };
    static AFTER_SETTLEMENT: ProbeSlot = const { std::cell::RefCell::new(None) };
    static AFTER_CAPTURE: ProbeSlot = const { std::cell::RefCell::new(None) };
    static AFTER_FENCE: ProbeSlot = const { std::cell::RefCell::new(None) };
    static AFTER_CLOSE: ProbeSlot = const { std::cell::RefCell::new(None) };
    static BEFORE_POSTLUDE: ProbeSlot = const { std::cell::RefCell::new(None) };
    static BEFORE_SELECTION: ProbeSlot = const { std::cell::RefCell::new(None) };
    static BEFORE_RELEASED: ProbeSlot = const { std::cell::RefCell::new(None) };
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
/// Runs once after the turn is captured and before the transition is taken.
#[cfg(test)]
pub(crate) fn probe_after_capture(probe: SettlementWindowProbe) {
    AFTER_CAPTURE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once right after the termination fence is taken, before any close.
#[cfg(test)]
pub(crate) fn probe_after_fence(probe: SettlementWindowProbe) {
    AFTER_FENCE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once after the close returned, or after the pane read as already ended.
#[cfg(test)]
pub(crate) fn probe_after_close(probe: SettlementWindowProbe) {
    AFTER_CLOSE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once when a scoped run hands its exclusion to an HTTP postlude, before the postlude.
#[cfg(test)]
pub(crate) fn probe_before_postlude(probe: SettlementWindowProbe) {
    BEFORE_POSTLUDE.with(|slot| *slot.borrow_mut() = Some(probe));
}
/// Runs once when a forced HTTP branch is about to pick its execution, after the request's own
/// session read.
#[cfg(test)]
pub(crate) fn probe_before_selection(probe: SettlementWindowProbe) {
    BEFORE_SELECTION.with(|slot| *slot.borrow_mut() = Some(probe));
}
#[cfg(test)]
pub(crate) async fn run_before_selection_probe() {
    run_probe(&BEFORE_SELECTION).await;
}
/// Runs once when a postlude's released step starts, after the exclusion was dropped.
#[cfg(test)]
pub(crate) fn probe_before_released(probe: SettlementWindowProbe) {
    BEFORE_RELEASED.with(|slot| *slot.borrow_mut() = Some(probe));
}
#[cfg(test)]
pub(crate) async fn run_before_released_probe() {
    run_probe(&BEFORE_RELEASED).await;
}
#[cfg(test)]
async fn run_probe(slot: &'static std::thread::LocalKey<ProbeSlot>) {
    if let Some(probe) = slot.with(|slot| slot.borrow_mut().take()) {
        probe().await;
    }
}

/// `exclusion` (transition guard and fence) comes back only once the turn settled and is dropped
/// on any other answer, so input arriving once the hold is gone waits in the queue.
async fn settle_and_release<E>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    nonce: &str,
    turn: Option<CapturedTurn>,
    exclusion: E,
) -> Result<E, TerminationResult> {
    if release_hold(nonce) != HoldRelease::Released {
        return Err(TerminationResult::Indeterminate(
            "retired but hold release incomplete".into(),
        ));
    }
    #[cfg(test)]
    run_probe(&SETTLEMENT_WINDOW).await;
    let result = settle_captured(shared, provider, turn).await;
    #[cfg(test)]
    run_probe(&AFTER_SETTLEMENT).await;
    match result {
        TerminationResult::Retired => Ok(exclusion),
        other => Err(other),
    }
}

pub(super) async fn settle_captured(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    turn: Option<CapturedTurn>,
) -> TerminationResult {
    let Some(CapturedTurn {
        key,
        token,
        mailbox,
        snapshot,
        ..
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
/// Production callers: [`terminate_registered_herdr`] and a user clear of a Herdr channel.
pub(crate) async fn terminate_explicit_herdr(
    request: OperatorTerminate,
    shared: Arc<SharedData>,
    pool: &PgPool,
) -> TerminationResult {
    match terminate_scoped(request, shared, pool, None).await {
        Ok(exclusion) => {
            drop(exclusion);
            TerminationResult::Retired
        }
        Err(result) => result,
    }
}

/// The service up to its settlement. A `dispatch` expectation binds the run to the dispatch the
/// caller selected: the captured turn's and the row's active one must both be it.
async fn terminate_scoped(
    request: OperatorTerminate,
    shared: Arc<SharedData>,
    pool: &PgPool,
    dispatch: Option<Option<String>>,
) -> Result<RetiredExclusion, TerminationResult> {
    use TerminationResult::{Indeterminate, Refused};
    if !terminate_enabled() {
        return Err(TerminationResult::Disabled);
    }
    let row = load(pool, &request.session_key).await?;
    let HostedRecord::Known(record) = &row.record else {
        return Err(Refused("unknown execution".into()));
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
        return Err(Refused("execution identity differs".into()));
    }
    let Some(provider) = ProviderKind::from_str(&record.owner.provider) else {
        return Err(Refused("unknown provider".into()));
    };
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|c| *c != 0);
    let Some(channel) = channel.map(ChannelId::new) else {
        return Err(Refused("unknown channel".into()));
    };
    let turn = capture_turn(&shared, &provider, channel)
        .await
        .map_err(Refused)?;
    #[cfg(test)]
    run_probe(&AFTER_CAPTURE).await;
    let Ok(transition) = shared.acquire_session_transition(channel).await else {
        return Err(Refused("transition busy".into()));
    };
    // An unreadable or replaced actor is a changed turn, never an idle channel.
    let actor = shared.mailbox_peek(channel);
    let same_turn = match (&turn, &actor) {
        (None, None) => true,
        (None, Some(now)) => matches!(now.cancel_token().await, Ok(None)),
        (Some(turn), Some(now)) => {
            now.same_actor(&turn.mailbox)
                && matches!(now.cancel_token().await, Ok(Some(active)) if Arc::ptr_eq(&active, &turn.token))
        }
        (Some(_), None) => false,
    };
    if !same_turn {
        return Err(Refused("turn changed under transition".into()));
    }
    let captured = turn.as_ref().and_then(|turn| turn.dispatch_id.as_ref());
    if dispatch.as_ref().is_some_and(|d| d.as_ref() != captured) {
        return Err(Refused("captured turn runs another dispatch".into()));
    }
    let Some(target) = herdr_endpoints().target(record) else {
        return Err(Refused("missing launch evidence".into()));
    };
    let fence = match target.termination_fence() {
        Ok(fence) => Arc::new(fence),
        Err(e) => return Err(Refused(format!("{e:?}"))),
    };
    let unsent = UnsentClose(Some(fence.clone()));
    #[cfg(test)]
    run_probe(&AFTER_FENCE).await;
    if load(pool, &request.session_key).await.as_ref() != Ok(&row)
        || !http_force::dispatch_holds(pool, &row, dispatch.as_ref()).await
    {
        fence.reopen();
        return Err(Refused("row changed under fence".into()));
    }
    let read_target = target.clone();
    match tokio::task::spawn_blocking(move || read_target.revalidate_termination_reads()).await {
        Ok(Ok(_)) => {}
        other => {
            fence.reopen();
            return Err(Refused(format!("termination provenance: {other:?}")));
        }
    }
    let Some(view) = herdr_endpoints().view(record) else {
        fence.reopen();
        return Err(Refused("no local endpoint".into()));
    };
    let read = record.clone();
    let ended = match tokio::task::spawn_blocking(move || view.ended(&read, &view.read_execution()))
        .await
    {
        Ok(ended) => ended,
        Err(e) => {
            fence.reopen();
            return Err(Indeterminate(e.to_string()));
        }
    };
    unsent.disarm();
    if !ended {
        // Blocking socket work remains outside the async executor; the fence stays owned here.
        let close_target = target.clone();
        let close_fence = fence.clone();
        let result = tokio::task::spawn_blocking(move || {
            terminate_herdr_once(OperatorTerminateWarrant::issue_fenced(
                close_target,
                &close_fence,
            ))
        })
        .await
        .map_err(|e| Indeterminate(e.to_string()))?;
        if !matches!(
            result,
            HerdrTerminateResult::Acknowledged | HerdrTerminateResult::Indeterminate(_)
        ) {
            fence.reopen();
            return Err(TerminationResult::Close(result));
        }
    }
    #[cfg(test)]
    run_probe(&AFTER_CLOSE).await;
    read_end(target, record.clone()).await?;
    retire_confirmed(pool, &row, record).await?;
    let token = turn.as_ref().map(|turn| turn.token.clone());
    let nonce = record.execution_nonce.clone();
    let exclusion = (transition, fence);
    let (transition, fence) =
        settle_and_release(&shared, &provider, &nonce, turn, exclusion).await?;
    let parts = (transition, fence, actor, token);
    Ok(RetiredExclusion::new(
        parts,
        row,
        dispatch.flatten(),
        provider,
        channel,
    ))
}
async fn load(pool: &PgPool, key: &str) -> Result<HostedObservation, TerminationResult> {
    match hosted_execution::load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(row) => Ok(row),
        HostedLookup::Missing => Err(TerminationResult::Refused("missing row".into())),
        HostedLookup::Conflict(e) => Err(TerminationResult::Refused(format!("conflict: {e:?}"))),
        HostedLookup::Unknown(e) => Err(TerminationResult::Refused(format!("unknown: {e}"))),
    }
}

/// The operator route's entry. The switch is read before any IO; the service then runs on a
/// detached task so a dropped request cannot stop it between its close and its settlement.
pub(crate) async fn terminate_registered_herdr(
    registry: &HealthRegistry,
    pool: PgPool,
    request: OperatorTerminate,
    reason: String,
) -> TerminationResult {
    if !terminate_enabled() {
        return TerminationResult::Disabled;
    }
    let logged = Logged {
        key: request.session_key.clone(),
        nonce: request.execution_nonce.clone(),
        reason,
        started: std::time::Instant::now(),
    };
    let shared = match runtime_of(registry, &pool, &request.session_key).await {
        Ok(shared) => shared,
        Err(result) => {
            logged.write(&result);
            return result;
        }
    };
    let run = tokio::spawn(async move {
        let service = terminate_explicit_herdr(request, shared, &pool);
        let result = std::panic::AssertUnwindSafe(service)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| TerminationResult::Indeterminate("terminate task panicked".into()));
        logged.write(&result);
        result
    });
    // Only a shutting-down runtime cancels the task, which then leaves no result log.
    run.await
        .unwrap_or_else(|_| TerminationResult::Indeterminate("terminate task cancelled".into()))
}

/// The runtime owning the row's channel; a refusal here took and wrote nothing.
async fn runtime_of(
    registry: &HealthRegistry,
    pool: &PgPool,
    session_key: &str,
) -> Result<Arc<SharedData>, TerminationResult> {
    let row = load(pool, session_key).await?;
    let HostedRecord::Known(record) = &row.record else {
        return Err(TerminationResult::Refused("unknown execution".into()));
    };
    let provider = ProviderKind::from_str(&record.owner.provider)
        .ok_or_else(|| TerminationResult::Refused("unknown provider".into()))?;
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|c| *c != 0);
    let channel = channel.ok_or_else(|| TerminationResult::Refused("unknown channel".into()))?;
    registry
        .shared_for_provider_on_channel(&provider, ChannelId::new(channel))
        .await
        .ok_or_else(|| TerminationResult::Refused("no runtime owns the channel".into()))
}

/// One result record per run, written where the run ends so a dropped request still leaves it.
struct Logged {
    key: String,
    nonce: String,
    reason: String,
    started: std::time::Instant,
}
impl Logged {
    fn write(&self, result: &TerminationResult) {
        if *result == TerminationResult::Disabled {
            return;
        }
        let (name, detail) = result.outcome();
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (session_key, execution_nonce, reason) = (&self.key, &self.nonce, &self.reason);
        if *result == TerminationResult::Retired {
            tracing::info!(target: "agentdesk::herdr_terminate", %session_key, %execution_nonce,
                %reason, result = name, %detail, elapsed_ms, "herdr terminate finished");
        } else {
            tracing::warn!(target: "agentdesk::herdr_terminate", %session_key, %execution_nonce,
                %reason, result = name, %detail, elapsed_ms, "herdr terminate finished");
        }
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
#[path = "herdr_terminate_route_tests.rs"]
pub(crate) mod route_tests;

#[cfg(test)]
#[path = "herdr_terminate_exit_tests.rs"]
mod exit_tests;

#[cfg(test)]
#[path = "herdr_terminate_tests.rs"]
mod tests;
