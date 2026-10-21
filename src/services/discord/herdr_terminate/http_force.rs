//! An HTTP force cancel's way into the P11 terminate service: the scoped run and the caller's
//! postlude share one detached task, so a dropped request ends neither, and the postlude runs
//! only while the run's [`RetiredExclusion`] is held.

use futures::FutureExt;
use futures::future::BoxFuture;
use sqlx::PgPool;

use super::{
    HealthRegistry, HostedObservation, Logged, OperatorTerminate, RetiredExclusion,
    TerminationResult, runtime_of, terminate_enabled, terminate_scoped,
};
use crate::db::dispatched_sessions::hosted_execution::http_force::active_dispatch_pg;
use crate::services::cluster::channel_home::{CommandPermit, command_scope};

/// Runs the scoped terminate bound to `dispatch` and, only on a retire, `postlude` while the
/// exclusion is held; `permit` keeps the channel's home command open until the result is logged.
pub(crate) async fn terminate_for_http<T, P>(
    registry: &HealthRegistry,
    pool: PgPool,
    (request, dispatch): (OperatorTerminate, Option<String>),
    permit: Option<CommandPermit>,
    reason: &str,
    postlude: P,
) -> (TerminationResult, Option<T>)
where
    T: Send + 'static,
    P: for<'a> FnOnce(&'a RetiredExclusion) -> BoxFuture<'a, T> + Send + 'static,
{
    if !terminate_enabled() {
        return (TerminationResult::Disabled, None);
    }
    let logged = Logged {
        key: request.session_key.clone(),
        nonce: request.execution_nonce.clone(),
        reason: reason.into(),
        started: std::time::Instant::now(),
    };
    let shared = match runtime_of(registry, &pool, &request.session_key).await {
        Ok(shared) => shared,
        Err(result) => {
            logged.write(&result);
            return (result, None);
        }
    };
    let run = tokio::spawn(command_scope(permit, async move {
        let service = async {
            match terminate_scoped(request, shared, &pool, Some(dispatch)).await {
                Ok(exclusion) => {
                    #[cfg(test)]
                    super::run_probe(&super::BEFORE_POSTLUDE).await;
                    let applied = postlude(&exclusion).await;
                    drop(exclusion);
                    (TerminationResult::Retired, Some(applied))
                }
                Err(result) => (result, None),
            }
        };
        let panicked = || {
            (
                TerminationResult::Indeterminate("terminate task panicked".into()),
                None,
            )
        };
        let outcome = std::panic::AssertUnwindSafe(service)
            .catch_unwind()
            .await
            .unwrap_or_else(|_| panicked());
        logged.write(&outcome.0);
        outcome
    }));
    let cancelled = TerminationResult::Indeterminate("terminate task cancelled".into());
    run.await.unwrap_or((cancelled, None))
}

/// Whether the row still names `dispatch` as its active one; no expectation always holds.
pub(super) async fn dispatch_holds(
    pool: &PgPool,
    row: &HostedObservation,
    dispatch: Option<&Option<String>>,
) -> bool {
    let Some(expected) = dispatch else {
        return true;
    };
    active_dispatch_pg(pool, row.session_id()).await.as_ref() == Ok(expected)
}

#[cfg(test)]
#[path = "http_force_tests.rs"]
mod tests;
