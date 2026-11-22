//! The terminate service's HTTP force entry as the queue and session routes reach it, and the
//! values a forced run is bound to.

use futures::future::BoxFuture;
use sqlx::PgPool;

use super::RetiredExclusion;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;

/// A postlude's outcome, finished once the exclusion is gone: work that must not run while the
/// channel is excluded, such as a drain of the queue the termination kept.
pub(crate) trait Released: Send + Sized + 'static {
    fn released(self) -> BoxFuture<'static, Self>;
}

/// The selected owner and row dispatch. HTTP force also binds the captured dispatch; restart
/// can leave another dispatch on the row while settling only the captured turn.
#[derive(Clone)]
pub(crate) struct Expected {
    pub(crate) owner: HostedOwner,
    pub(crate) dispatch: Option<String>,
    pub(crate) captured_dispatch: bool,
}

/// The scoped terminate of execution `nonce` under `session_key`, bound to `expected`, with its
/// retire `postlude`; answers the termination outcome's name and detail with the applied postlude.
pub(crate) trait HttpTerminate {
    fn terminate_for_http<T, P>(
        &self,
        pool: PgPool,
        target: (String, String, Expected),
        reason: &str,
        postlude: P,
    ) -> impl Future<Output = ((&'static str, String), Option<T>)> + Send
    where
        T: Released,
        P: for<'a> FnOnce(&'a RetiredExclusion) -> BoxFuture<'a, T> + Send + 'static;
}
