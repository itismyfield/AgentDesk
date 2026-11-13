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
use crate::services::herdr_launch::clear_epoch::{self, Epoch};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::restart_operation::Operation;

const FAILED: &str = "기존 세션의 종료를 확인하지 못했습니다.\n인사말 turn은 시작하지 못했습니다.";
const REFUSED: &str = "세션을 재시작하지 않았습니다. 현재 세션을 안전하게 확인할 수 없습니다.";
const ENDED: &str = "기존 세션 종료를 확인했습니다.\n인사말 turn은 시작하지 못했습니다.";

pub(super) async fn run<Warn, WarnFut>(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    id: &str,
    warn: Warn,
) -> Result<String, super::Error>
where
    Warn: FnOnce() -> WarnFut,
    WarnFut: std::future::Future<Output = Result<(), super::Error>>,
{
    let operation = match Operation::new(provider.as_str(), &shared.token_hash, channel.get(), id) {
        Ok(operation) => operation,
        Err(_) => return Ok(REFUSED.into()),
    };
    if let Some(response) = restart_gate::completed(&operation) {
        return Ok(response);
    }
    let gate = match Gate::begin(&shared, channel) {
        Ok(gate) => gate,
        Err(_) => {
            return Ok(restart_gate::wait_completed(&operation)
                .await
                .unwrap_or_else(|| "다른 재시작 요청을 처리하고 있습니다.".into()));
        }
    };
    if let Some(response) = restart_gate::completed(&operation) {
        return Ok(response);
    }
    let epoch = clear_epoch::snapshot(provider.as_str(), &shared.token_hash, channel.get());
    warn().await?;
    let task = tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(terminate(shared, provider, channel, epoch))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| FAILED.into());
        // The detached worker remembers the result before delivery can resume.
        restart_gate::remember(&operation, result.clone());
        drop(gate);
        result
    });
    Ok(task.await.unwrap_or_else(|_| FAILED.into()))
}

async fn terminate(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    epoch: Epoch,
) -> String {
    let Some(pool) = shared.pg_pool.as_ref().cloned() else {
        return REFUSED.into();
    };
    let Some(registry) = shared.health_registry() else {
        return REFUSED.into();
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
        HostedLookup::Missing => {
            return "종료할 활성 세션을 찾지 못했습니다.\n인사말 turn은 시작하지 못했습니다."
                .into();
        }
        _ => return REFUSED.into(),
    };
    let HostedRecord::Known(record) = &row.record else {
        return REFUSED.into();
    };
    if record.location.is_none()
        || record.owner.provider != provider.as_str()
        || record.owner.discord_token_hash != shared.token_hash
        || record.owner.channel_id != channel_key
    {
        return REFUSED.into();
    }
    let key =
        match sqlx::query_scalar::<_, String>("SELECT session_key FROM sessions WHERE id = $1")
            .bind(row.session_id())
            .fetch_one(&pool)
            .await
        {
            Ok(key) => key,
            Err(_) => return REFUSED.into(),
        };
    let dispatch =
        match hosted_execution::http_force::active_dispatch_pg(&pool, row.session_id()).await {
            Ok(dispatch) => dispatch,
            Err(_) => return REFUSED.into(),
        };
    let expected =
        match herdr_terminate::restart_recovery::expected_for_restart(&shared, record, dispatch)
            .await
        {
            Ok(expected) => expected,
            Err(result) => return failure(result),
        };
    let request = || OperatorTerminate {
        session_key: key.clone(),
        execution_nonce: record.execution_nonce.clone(),
    };
    let id = row.session_id();
    match herdr_terminate::restart_recovery::recover_ended(
        request(),
        expected.clone(),
        shared.clone(),
        &pool,
    )
    .await
    {
        Ok(Some(held)) => {
            let result = apply(&held, &pool, id, &epoch).await;
            drop(held);
            result.released().await.response()
        }
        Ok(None) => {
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
            applied
                .map(Applied::response)
                .unwrap_or_else(|| failure(result))
        }
        Err(result) => failure(result),
    }
}

enum Applied {
    Ended,
    Cleared,
    Failed,
}
impl Applied {
    fn response(self) -> String {
        match self {
            Self::Ended => ENDED.into(),
            Self::Cleared => {
                "기존 세션 종료를 확인했습니다.\n겹친 비우기 요청으로 재시작을 중단했습니다.".into()
            }
            Self::Failed => "기존 세션 종료를 확인했습니다.\n세션 정리를 마치지 못했습니다. 인사말 turn은 시작하지 못했습니다.".into(),
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
thread_local! {
    static BEFORE_DISCONNECT: std::cell::RefCell<Option<DisconnectProbe>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(in crate::services::discord) fn probe_restart_disconnect_for_tests(probe: DisconnectProbe) {
    BEFORE_DISCONNECT.with(|slot| *slot.borrow_mut() = Some(probe));
}

async fn apply(held: &RetiredExclusion, pool: &PgPool, id: i64, epoch: &Epoch) -> Applied {
    if !epoch.unchanged() {
        return clear_selector(held, pool, id).await;
    }
    #[cfg(test)]
    if let Some(probe) = BEFORE_DISCONNECT.with(|slot| slot.borrow_mut().take()) {
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
