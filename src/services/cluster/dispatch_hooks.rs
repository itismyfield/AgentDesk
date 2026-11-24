//! Dispatch-queue effects that cluster bootstrap starts but the dispatch layer owns.
//! The server passes them in, so the cluster runtime names no dispatch module.

use futures::future::BoxFuture;
use sqlx::PgPool;

use crate::config::ClusterConfig;

/// The node-join wait-queue wake and the stale claim-owner reassignment `bootstrap` runs.
#[derive(Clone, Copy)]
pub(crate) struct DispatchQueueHooks {
    /// Starts one wait-queue wake: pool, cluster config, trigger, source, dispatch id.
    pub(crate) wake_wait_queue:
        fn(PgPool, ClusterConfig, &'static str, &'static str, Option<String>),
    /// Reassigns stale dispatch_outbox claim owners once and returns how many moved.
    pub(crate) reassign_stale_claim_owners:
        for<'a> fn(&'a PgPool, &'a ClusterConfig) -> BoxFuture<'a, Result<usize, String>>,
}
