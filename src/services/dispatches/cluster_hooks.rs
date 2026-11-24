//! The dispatch functions cluster bootstrap runs on node join and on its claim-owner sweep.

use futures::future::BoxFuture;
use sqlx::PgPool;

use crate::config::ClusterConfig;
use crate::services::cluster::dispatch_hooks::DispatchQueueHooks;

pub(crate) const HOOKS: DispatchQueueHooks = DispatchQueueHooks {
    wake_wait_queue: super::wait_queue::spawn_wait_queue_wake_pg,
    reassign_stale_claim_owners,
};

fn reassign_stale_claim_owners<'a>(
    pool: &'a PgPool,
    cluster_config: &'a ClusterConfig,
) -> BoxFuture<'a, Result<usize, String>> {
    Box::pin(
        super::outbox_claiming::reassign_stale_dispatch_outbox_claim_owners_with_cluster_config_pg(
            pool,
            cluster_config,
        ),
    )
}
