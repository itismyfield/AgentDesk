//! The claim's cluster config, read whole on a blocking thread: the file read, the YAML parse
//! and the data-dir creation of `load_graceful` never run on a runtime worker.

use sqlx::PgPool;

use super::{TaskDispatchClaimOutcome, TaskDispatchClaimRequest};
use crate::config::ClusterConfig;

pub async fn claim_task_dispatches(
    pool: &PgPool,
    request: &TaskDispatchClaimRequest,
) -> Result<TaskDispatchClaimOutcome, String> {
    claim_after_reading(pool, request, || crate::config::load_graceful().cluster).await
}

/// Claims only after `read` finished on a blocking thread; a read that panicked or was cancelled
/// fails the claim rather than claiming with defaults.
async fn claim_after_reading(
    pool: &PgPool,
    request: &TaskDispatchClaimRequest,
    read: impl FnOnce() -> ClusterConfig + Send + 'static,
) -> Result<TaskDispatchClaimOutcome, String> {
    let cluster_config = tokio::task::spawn_blocking(read)
        .await
        .map_err(|error| format!("read cluster config for task dispatch claims: {error}"))?;
    super::claim_task_dispatches_with_cluster_config(pool, request, &cluster_config).await
}
