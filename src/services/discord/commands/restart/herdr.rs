//! The restart command consumes the existing exact termination and preserves waiting input.
use std::sync::Arc;

use futures::{FutureExt, future::BoxFuture};
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::super::super::SharedData;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    self, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
};
use crate::services::discord::herdr_terminate::http_force::{self, Released};
use crate::services::discord::herdr_terminate::{
    self, OperatorTerminate, RetiredExclusion, TerminationResult,
};
use crate::services::discord::queue_io::input_owner::restart_gate::{self, Gate};
use crate::services::discord::queue_io::input_owner::restart_seed::{
    self as seed, Admitted, Seed, SeedStatus,
};
use crate::services::herdr_launch::clear_epoch::{self, Epoch};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::restart_operation::Operation;

const FAILED: &str = "기존 세션의 종료를 확인하지 못했습니다.\n인사말 turn은 시작하지 못했습니다.";
const REFUSED: &str = "세션을 재시작하지 않았습니다. 현재 세션을 안전하게 확인할 수 없습니다.";

pub(super) async fn run<Warn, WarnFut, Say, SayFut>(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    (id, command, requester): (&str, &'static str, &str),
    warn: Warn,
    say: Say,
) -> Result<(), super::Error>
where
    Warn: FnOnce() -> WarnFut,
    WarnFut: std::future::Future<Output = Result<(), super::Error>>,
    Say: FnOnce(String) -> SayFut,
    SayFut: std::future::Future<Output = Result<(), super::Error>>,
{
    let operation = match Operation::new(provider.as_str(), &shared.token_hash, channel.get(), id) {
        Ok(operation) => operation,
        Err(_) => return say(REFUSED.into()).await,
    };
    if let Some(response) = restart_gate::completed(&operation) {
        return say(response).await;
    }
    let gate = match Gate::begin(&shared, channel) {
        Ok(gate) => gate,
        Err(_) => {
            let held = restart_gate::held(&shared, channel);
            let waited = restart_gate::wait_completed(&operation).await;
            return say(waited.unwrap_or_else(|| busy(held))).await;
        }
    };
    if let Some(response) = restart_gate::completed(&operation) {
        return say(response).await;
    }
    let epoch = clear_epoch::snapshot(provider.as_str(), &shared.token_hash, channel.get());
    let mailbox = shared.mailbox(channel);
    // The operation's own ticket is read before the current row gives it any new context, and
    // a request that ends nothing is answered without the loss warning.
    let settled = match Seed::prior(&mailbox, &operation).await {
        Ok(SeedStatus::Absent) | Err(_) => match select(&shared, &provider, channel).await {
            Selected::Refused(response) => Err(response),
            Selected::Vacant => Ok(None),
            Selected::Target(target) => Ok(Some(target)),
        },
        Ok(status) => Err(format!(
            "이미 처리한 재시작 요청입니다.\n{}",
            greeting(status)
        )),
    };
    let target = match settled {
        Ok(target) => target,
        Err(response) => {
            restart_gate::remember(&operation, response.clone());
            report(&provider, channel, (command, requester), "refused");
            drop(gate);
            return say(response).await;
        }
    };
    if target.as_ref().is_some_and(|target| target.live()) {
        warn().await?;
    }
    let requester = requester.to_string();
    let (answer, answered) = tokio::sync::oneshot::channel::<()>();
    let (result, finished) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let request = Request {
            shared,
            channel,
            epoch,
            operation: operation.clone(),
            by: (command, requester.clone()),
            target,
        };
        let (response, failed) = std::panic::AssertUnwindSafe(restart(request))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| (FAILED.into(), None));
        // The detached worker remembers the result before delivery can resume.
        restart_gate::remember(&operation, response.clone());
        let outcome = if failed.is_some() {
            "failed"
        } else {
            "answered"
        };
        report(&provider, channel, (command, requester.as_str()), outcome);
        let _ = result.send(response);
        // A failed restart answers first and withdraws its greeting after: a crash in between
        // leaves the greeting to its permit's refusal, never to a silent end.
        if let Some(seed) = failed {
            let _ = answered.await;
            seed.withdraw(&mailbox).await;
        }
        drop(gate);
    });
    let said = say(finished.await.unwrap_or_else(|_| FAILED.into())).await;
    drop(answer);
    said
}

fn busy(held: Option<std::time::Duration>) -> String {
    match held {
        Some(held) => format!(
            "다른 재시작 요청을 처리하고 있습니다({}초 경과).",
            held.as_secs()
        ),
        None => "다른 재시작 요청을 처리하고 있습니다.".into(),
    }
}

fn report(provider: &ProviderKind, channel: ChannelId, by: (&str, &str), outcome: &str) {
    tracing::info!(
        event = "discord_restart_triggered",
        channel_id = channel.get(),
        user_name = %by.1,
        command = %by.0,
        provider = provider.as_str(),
        host = "herdr",
        outcome,
    );
}

struct Request {
    shared: Arc<SharedData>,
    channel: ChannelId,
    epoch: Epoch,
    operation: Operation,
    by: (&'static str, String),
    target: Option<Box<Target>>,
}

/// The canonical row a restart acts on.
enum Selected {
    /// No execution to end: the greeting's own turn launches the session.
    Vacant,
    Refused(String),
    Target(Box<Target>),
}

struct Target {
    pool: PgPool,
    row: hosted_execution::HostedObservation,
    record: hosted_execution::HostedExecution,
    key: String,
    dispatch: Option<String>,
}

impl Target {
    /// Whether an execution is still to be ended; a retired row only gets the greeting.
    fn live(&self) -> bool {
        self.record.state != HostedState::Retired
    }
}

const UNPLACED: &str = "세션을 재시작하지 않았습니다. 새 세션의 생성 응답을 아직 확인하지 못했습니다.\n메시지를 보내면 기존 경로가 이 상태를 해결합니다.";

async fn select(shared: &Arc<SharedData>, provider: &ProviderKind, channel: ChannelId) -> Selected {
    let Some(pool) = shared.pg_pool.as_ref().cloned() else {
        return Selected::Refused(REFUSED.into());
    };
    let channel_key = channel.get().to_string();
    let row = match hosted_execution::load_hosted_execution_pg(
        &pool,
        HostedLookupKey::Canonical {
            provider: provider.as_str(),
            identity: CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &channel_key,
            },
        },
    )
    .await
    {
        HostedLookup::Found(row) => row,
        HostedLookup::Missing => return Selected::Vacant,
        _ => return Selected::Refused(REFUSED.into()),
    };
    let record = match &row.record {
        HostedRecord::Known(record) => record,
        // A row no Herdr execution ever held: the greeting's own turn launches or migrates it.
        HostedRecord::Legacy => return Selected::Vacant,
        _ => return Selected::Refused(REFUSED.into()),
    };
    if record.owner.provider != provider.as_str()
        || record.owner.discord_token_hash != shared.token_hash
        || record.owner.channel_id != channel_key
    {
        return Selected::Refused(REFUSED.into());
    }
    if record.location.is_none() && record.state != HostedState::Retired {
        return Selected::Refused(UNPLACED.into());
    }
    // Another node's execution is refused here, before any warning or greeting.
    let local = crate::config::session_hosts::local_node();
    let foreign = Some(record.owner.owner_node.as_str()) != local.as_deref()
        || (record.location.as_ref())
            .is_some_and(|at| at.execution_node != record.owner.owner_node);
    if foreign {
        return Selected::Refused(REFUSED.into());
    }
    let key =
        match sqlx::query_scalar::<_, String>("SELECT session_key FROM sessions WHERE id = $1")
            .bind(row.session_id())
            .fetch_one(&pool)
            .await
        {
            Ok(key) => key,
            Err(_) => return Selected::Refused(REFUSED.into()),
        };
    let dispatch =
        match hosted_execution::http_force::active_dispatch_pg(&pool, row.session_id()).await {
            Ok(dispatch) => dispatch,
            Err(_) => return Selected::Refused(REFUSED.into()),
        };
    let record = record.clone();
    Selected::Target(Box::new(Target {
        pool,
        row,
        record,
        key,
        dispatch,
    }))
}

/// The response, and the greeting this request must withdraw once that response was said.
async fn restart(request: Request) -> (String, Option<Seed>) {
    let Request {
        shared,
        channel,
        epoch,
        operation,
        by,
        target,
    } = request;
    let epoch = Arc::new(epoch);
    let mailbox = shared.mailbox(channel);
    // The turn captured now decides whether the row's dispatch is this restart's to detach.
    let expected = match target.as_deref().filter(|target| target.live()) {
        Some(target) => {
            let expect = herdr_terminate::restart_recovery::expected_for_restart;
            match expect(&shared, &target.record, target.dispatch.clone()).await {
                Ok(expected) => Some(expected),
                Err(result) => return (failure(result), None),
            }
        }
        None => None,
    };
    let mut seed = Seed::of(&mailbox, &operation).await;
    let owned = expected
        .as_ref()
        .filter(|expected| expected.captured_dispatch);
    let against = seed::Target {
        nonce: target.as_ref().map(|t| t.record.execution_nonce.as_str()),
        dispatch: owned.and_then(|expected| expected.dispatch.as_deref()),
    };
    #[cfg(test)]
    if let Some(probe) = PROBES.with(|slot| slot.borrow_mut().before_seed.take()) {
        probe().await;
    }
    let admitted = match seed.as_mut() {
        Ok(seed) => seed.admit(&mailbox, (by.0, &by.1), against).await,
        Err(reason) => Admitted::Refused(reason.clone()),
    };
    let (mut seed, own, mut absent) = match (admitted, seed) {
        (Admitted::Conflict, _) => return (REFUSED.into(), None),
        (Admitted::Created, Ok(seed)) => (Some(seed), true, None),
        (Admitted::Joined, Ok(seed)) => (Some(seed), false, None),
        (Admitted::Unconfirmed, Ok(seed)) => (Some(seed), true, Some(true)),
        (Admitted::Refused(reason), _) | (_, Err(reason)) => {
            tracing::info!(%reason, "restart goes on without a greeting");
            (None, false, Some(false))
        }
    };
    let ended = match target {
        None => Ok(Applied::Vacant),
        // Already retired: nothing is ended again; the greeting's permit detaches what is left.
        Some(target) => match expected {
            Some(expected) => terminate(&shared, *target, expected, Arc::clone(&epoch)).await,
            None => Ok(Applied::Retired),
        },
    };
    // An unconfirmed commit is settled under the same ticket once the old execution is decided.
    if absent == Some(true)
        && let Some(seed) = seed.as_mut()
    {
        let again = seed::Target {
            nonce: None,
            dispatch: None,
        };
        match seed.admit(&mailbox, (by.0, &by.1), again).await {
            Admitted::Created | Admitted::Joined => absent = None,
            _ => {}
        }
    }
    let status = match &seed {
        Some(seed) if absent.is_none() => seed.status(&mailbox).await.ok(),
        _ => None,
    };
    #[cfg(test)]
    if let Some(probe) = PROBES.with(|slot| slot.borrow_mut().seed_admitted.take()) {
        probe().await;
    }
    // Include no-close requests and admission retries: clear may finish before either commit.
    let ended = ended.map(|applied| {
        if epoch.unchanged() {
            applied
        } else {
            Applied::Cleared
        }
    });
    match ended {
        Ok(Applied::Cleared) => {
            if let Some(seed) = seed.take().filter(|_| own) {
                // A vacant row cannot refuse a late greeting; keep delivery gated until removal.
                while let Err(error) = seed.withdraw_confirmed(&mailbox).await {
                    tracing::warn!(%error, channel_id = channel.get(), "clear waits for restart greeting withdrawal");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            (Applied::Cleared.response().into(), None)
        }
        Ok(applied) => {
            let greeting = match (status, absent) {
                (Some(status), _) => greeting(status),
                (None, Some(true)) => "인사말 등록을 확인하지 못했습니다.",
                (None, _) => "인사말 turn은 시작하지 못했습니다.",
            };
            (format!("{}\n{greeting}", applied.response()), None)
        }
        Err(response) => (response, seed.filter(|_| own)),
    }
}

fn greeting(status: SeedStatus) -> &'static str {
    match status {
        SeedStatus::Queued | SeedStatus::Attempt => "인사말을 대기열에 등록했습니다.",
        SeedStatus::Settled => "인사말 turn이 시작됐습니다.",
        SeedStatus::Retained => "인사말 turn을 시작하지 못했습니다. 실패 알림을 확인해 주세요.",
        SeedStatus::Gone | SeedStatus::Absent => "인사말 turn은 시작하지 못했습니다.",
    }
}

/// Ends the exact old execution, or finishes one that already ended, and runs the postlude.
async fn terminate(
    shared: &Arc<SharedData>,
    target: Target,
    expected: http_force::Expected,
    epoch: Arc<Epoch>,
) -> Result<Applied, String> {
    let Target {
        pool,
        row,
        record,
        key,
        ..
    } = target;
    let Some(registry) = shared.health_registry() else {
        return Err(REFUSED.into());
    };
    let request = || OperatorTerminate {
        session_key: key.clone(),
        execution_nonce: record.execution_nonce.clone(),
    };
    let id = row.session_id();
    let recovered = herdr_terminate::restart_recovery::recover_ended(
        request(),
        expected.clone(),
        shared.clone(),
        &pool,
    );
    let applied = match recovered.await.map_err(failure)? {
        Some(held) => {
            tracing::info!(
                sessions_row = id,
                "restart finished an execution that had already ended"
            );
            let result = apply(&held, &pool, id, &epoch).await;
            drop(held);
            result.released().await
        }
        None => {
            let pool_for_postlude = pool.clone();
            let (result, applied) = http_force::terminate_for_http(
                &registry,
                pool,
                (request(), expected),
                "restart",
                move |held| {
                    async move { apply(held, &pool_for_postlude, id, &epoch).await }.boxed()
                },
            )
            .await;
            applied.ok_or_else(|| failure(result))?
        }
    };
    match applied {
        Applied::Failed => Err(Applied::Failed.response().into()),
        applied => Ok(applied),
    }
}

/// The greeting starts only once the execution it was queued against is retired and detached;
/// after a daemon death the permit finishes that itself and never sends a close.
pub(in crate::services::discord) async fn seed_permit(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    context: &serde_json::Value,
) -> Result<(), String> {
    let against = seed::target(context);
    let target = match select(shared, &shared.provider, channel).await {
        Selected::Vacant => return Ok(()),
        Selected::Refused(_) => return Err("현재 세션을 확인할 수 없습니다".into()),
        Selected::Target(target) => target,
    };
    let retired = target.record.state == HostedState::Retired;
    if Some(target.record.execution_nonce.as_str()) != against.nonce {
        // Another execution's retired row is vacant; a live one is not this restart's successor.
        return match retired {
            true => Ok(()),
            false => Err("재시작 대상이 아닌 세션이 실행 중입니다".into()),
        };
    }
    if retired && (target.dispatch.is_none() || target.dispatch.as_deref() != against.dispatch) {
        return Ok(());
    }
    let epoch = clear_epoch::snapshot(shared.provider.as_str(), &shared.token_hash, channel.get());
    let Target {
        pool,
        row,
        record,
        key,
        dispatch,
    } = *target;
    // The greeting's durable context is the evidence of which dispatch its restart captured.
    let expected = http_force::Expected {
        owner: record.owner.clone(),
        captured_dispatch: dispatch.is_some() && dispatch.as_deref() == against.dispatch,
        dispatch,
    };
    let request = OperatorTerminate {
        session_key: key,
        execution_nonce: record.execution_nonce.clone(),
    };
    let recover = herdr_terminate::restart_recovery::recover_ended;
    let recovered = recover(request, expected, shared.clone(), &pool).await;
    let outcome = recovered
        .as_ref()
        .map(Option::is_some)
        .map_err(|e| e.outcome().0);
    tracing::info!(
        channel_id = channel.get(),
        ?outcome,
        "restart greeting permit recovery"
    );
    match recovered {
        Ok(Some(held)) => match apply(&held, &pool, row.session_id(), &epoch).await {
            Applied::Failed => Err("기존 세션 정리를 마치지 못했습니다".into()),
            _ => Ok(()),
        },
        Ok(None) => Err("재시작이 끝나지 않아 기존 세션이 계속 실행 중입니다".into()),
        Err(result) => Err(format!(
            "기존 세션의 종료를 확인하지 못했습니다: {}",
            result.outcome().1
        )),
    }
}

enum Applied {
    Vacant,
    Retired,
    Ended,
    Cleared,
    Failed,
}
impl Applied {
    fn response(&self) -> &'static str {
        match self {
            Self::Vacant => "종료할 활성 세션이 없어 새 세션으로 시작합니다.",
            Self::Retired => "기존 세션은 이미 종료되어 있습니다.",
            Self::Ended => "기존 세션 종료를 확인했습니다.",
            Self::Cleared => "겹친 비우기 요청으로 재시작을 중단했습니다.",
            Self::Failed => {
                "기존 세션 종료를 확인했습니다.\n세션 정리를 마치지 못했습니다. 인사말 turn은 시작하지 못했습니다."
            }
        }
    }
}
impl Released for Applied {
    fn released(self) -> BoxFuture<'static, Self> {
        Box::pin(async move { self })
    }
}
#[cfg(test)]
type DisconnectProbe = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;
#[cfg(test)]
#[derive(Default)]
struct RestartProbes {
    before_disconnect: Option<DisconnectProbe>,
    before_seed: Option<DisconnectProbe>,
    seed_admitted: Option<DisconnectProbe>,
}
#[cfg(test)]
thread_local! {
    static PROBES: std::cell::RefCell<RestartProbes> = std::cell::RefCell::new(RestartProbes::default());
}
#[cfg(test)]
pub(in crate::services::discord) fn probe_restart_seed_admitted_for_tests(probe: DisconnectProbe) {
    PROBES.with(|slot| slot.borrow_mut().seed_admitted = Some(probe));
}
#[cfg(test)]
pub(in crate::services::discord) fn probe_restart_seed_for_tests(probe: DisconnectProbe) {
    PROBES.with(|slot| slot.borrow_mut().before_seed = Some(probe));
}
#[cfg(test)]
pub(in crate::services::discord) fn probe_restart_disconnect_for_tests(probe: DisconnectProbe) {
    PROBES.with(|slot| slot.borrow_mut().before_disconnect = Some(probe));
}

async fn apply(held: &RetiredExclusion, pool: &PgPool, id: i64, epoch: &Epoch) -> Applied {
    if !epoch.unchanged() {
        return clear_selector(held, pool, id).await;
    }
    #[cfg(test)]
    if let Some(probe) = PROBES.with(|slot| slot.borrow_mut().before_disconnect.take()) {
        probe().await;
    }
    match held.disconnect_session_keeping_provider(pool).await {
        Ok(_) if epoch.unchanged() => Applied::Ended,
        Ok(_) => clear_selector(held, pool, id).await,
        Err(error) => {
            tracing::warn!(%error, "restart disconnect failed");
            Applied::Failed
        }
    }
}
async fn clear_selector(held: &RetiredExclusion, pool: &PgPool, id: i64) -> Applied {
    let Some(record) = held.execution() else {
        return Applied::Failed;
    };
    let mut retired = record.clone();
    retired.state = HostedState::Retired;
    match hosted_execution::http_force::clear_retired_selector_pg(pool, id, &retired).await {
        Ok(true) => Applied::Cleared,
        Ok(false) | Err(_) => Applied::Failed,
    }
}

fn failure(result: TerminationResult) -> String {
    let (outcome, detail) = result.outcome();
    tracing::warn!(outcome, %detail, "restart termination did not complete");
    match result {
        TerminationResult::Refused(_) | TerminationResult::Disabled => REFUSED.into(),
        _ => FAILED.into(),
    }
}
