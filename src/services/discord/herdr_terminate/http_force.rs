//! An HTTP force cancel's way into the P11 terminate service: the scoped run and its postlude
//! share one detached task, and the postlude runs only while the [`RetiredExclusion`] is held.

use futures::FutureExt;
use futures::future::BoxFuture;
use sqlx::PgPool;

use super::{
    HealthRegistry, HostedObservation, Logged, OperatorTerminate, RetiredExclusion,
    TerminationResult, runtime_of, terminate_enabled, terminate_scoped,
};
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::db::dispatched_sessions::hosted_execution::http_force::active_dispatch_pg;

/// A postlude's outcome, finished once the exclusion is gone: work that must not run while the
/// channel is excluded, such as a drain of the queue the termination kept.
pub(crate) trait Released: Send + Sized + 'static {
    fn released(self) -> BoxFuture<'static, Self>;
}

/// What an HTTP force selected besides key and nonce: the execution's whole owner and its row's
/// dispatch, none included. The operator route and a clear expect neither.
pub(crate) struct Expected {
    pub(crate) owner: HostedOwner,
    pub(crate) dispatch: Option<String>,
}

/// Runs the scoped terminate bound to `expected`, on a retire `postlude` under the held exclusion
/// and then its released step before the result is logged.
pub(crate) async fn terminate_for_http<T, P>(
    registry: &HealthRegistry,
    pool: PgPool,
    (request, expected): (OperatorTerminate, Expected),
    reason: &str,
    postlude: P,
) -> (TerminationResult, Option<T>)
where
    T: Released,
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
    let run = tokio::spawn(async move {
        let service = async {
            match terminate_scoped(request, shared, &pool, Some(expected)).await {
                Ok(exclusion) => {
                    #[cfg(test)]
                    super::run_probe(&super::BEFORE_POSTLUDE).await;
                    let applied = postlude(&exclusion).await;
                    drop(exclusion);
                    let applied = applied.released().await;
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
    });
    let cancelled = TerminationResult::Indeterminate("terminate task cancelled".into());
    run.await.unwrap_or((cancelled, None))
}

/// Whether the row still names the expected dispatch as its active one; no expectation holds.
pub(super) async fn dispatch_holds(
    pool: &PgPool,
    row: &HostedObservation,
    expected: Option<&Expected>,
) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    active_dispatch_pg(pool, row.session_id()).await.as_ref() == Ok(&expected.dispatch)
}

#[cfg(test)]
#[path = "http_force_tests.rs"]
mod tests;
