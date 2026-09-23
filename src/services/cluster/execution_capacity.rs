//! One budget for admitted intake and actual provider execution. Outbox rows
//! reserve capacity; fenced renewable leases cover execution after delivery.

use crate::services::provider::CancelToken;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

mod store;
#[cfg(test)]
mod tests;

static LOCAL: OnceLock<(u32, Arc<Semaphore>)> = OnceLock::new();
const LEASE_SECONDS: i64 = 30;
// Renew three times per lease period; each DB operation must finish sooner.
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(LEASE_SECONDS as u64 / 3);
const LEASE_IO_TIMEOUT: Duration = Duration::from_secs(5);
// Match the execution_slots admission bound when comparing advertised occupancy.
const MAX_RANKED_SLOTS: u64 = 1024;
pub(crate) const EXHAUSTED: &str = "node_execution_capacity_exhausted";
pub(crate) fn is_exhausted(error: &sqlx::Error) -> bool {
    error.as_database_error().and_then(|e| e.constraint())
        == Some("node_execution_capacity_available")
}

pub(crate) fn publish(caps: &mut serde_json::Map<String, Value>) {
    let cluster = crate::config::load_graceful().cluster;
    caps.remove("execution_capacity");
    if let Some(slots) = LOCAL
        .get()
        .map(|v| v.0)
        .or(cluster.execution_slots.filter(|_| cluster.enabled))
    {
        // Capacity changes require restart, so advertisement and local guard
        // can never disagree after a config hot reload.
        let (slots, _) = LOCAL.get_or_init(|| (slots, Arc::new(Semaphore::new(slots as usize))));
        caps.insert(
            "execution_capacity".into(),
            json!({"version":1,"slots":slots}),
        );
    }
}

pub(crate) fn automatic_enabled() -> bool {
    crate::config_live_reload::current()
        .map(|config| config.cluster.intake_routing.capacity_aware)
        .unwrap_or_else(|| {
            crate::config::load_graceful()
                .cluster
                .intake_routing
                .capacity_aware
        })
}

/// Order available bounded candidates by occupancy ratio, oldest assignment,
/// then identity. Legacy/unbounded nodes never absorb automatic overflow.
pub(crate) fn rank(nodes: &mut Vec<Value>) {
    nodes.retain(|node| {
        node.pointer("/capabilities/execution_capacity/version") == Some(&json!(1))
            && node
                .pointer("/capabilities/execution_capacity/slots")
                .and_then(Value::as_u64)
                .is_some_and(|slots| {
                    slots > node["execution_occupied"].as_u64().unwrap_or(u64::MAX)
                })
    });
    nodes.sort_by(|a, b| {
        let slots = |n: &Value| {
            n.pointer("/capabilities/execution_capacity/slots")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(MAX_RANKED_SLOTS)
        };
        let used = |n: &Value| {
            n["execution_occupied"]
                .as_u64()
                .unwrap_or(0)
                .min(MAX_RANKED_SLOTS)
        };
        (used(a) * slots(b))
            .cmp(&(used(b) * slots(a)))
            .then_with(|| {
                a["last_execution_assignment_at"]
                    .as_str()
                    .cmp(&b["last_execution_assignment_at"].as_str())
            })
            .then_with(|| a["instance_id"].as_str().cmp(&b["instance_id"].as_str()))
    });
    for (rank, node) in nodes.iter_mut().enumerate() {
        node["capacity_rank"] = json!(rank);
    }
}

pub(crate) struct ExecutionGuard {
    stop: Option<oneshot::Sender<()>>,
    released: Option<oneshot::Receiver<()>>,
    runtime: tokio::runtime::Handle,
    _permit: OwnedSemaphorePermit,
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // execute() owns this guard on a blocking thread. Complete release before
        // the next queued turn can acquire the same provider/channel identity.
        if let Some(released) = self.released.take() {
            self.runtime.block_on(async {
                let _ = tokio::time::timeout(LEASE_IO_TIMEOUT, released).await;
            });
        }
    }
}

/// Called inside the provider's blocking task, before it starts any process.
pub(crate) fn acquire(
    pool: Option<&PgPool>,
    provider: &str,
    channel: u64,
    cancel: Arc<CancelToken>,
) -> Result<Option<ExecutionGuard>, String> {
    let config = crate::config::load_graceful();
    if LOCAL.get().is_none()
        && (!config.cluster.enabled || config.cluster.execution_slots.is_none())
    {
        return Ok(None);
    }
    let slots = config
        .cluster
        .execution_slots
        .ok_or("execution_slots changed; restart the node")?;
    let (configured, semaphore) =
        LOCAL.get_or_init(|| (slots, Arc::new(Semaphore::new(slots as usize))));
    if *configured != slots {
        return Err("execution_slots changed; restart the node before new execution".into());
    }
    let permit = semaphore.clone().try_acquire_owned().map_err(|_| {
        "node execution capacity exhausted; retry when a turn completes".to_string()
    })?;
    let pool = pool
        .ok_or("execution capacity requires PostgreSQL")?
        .clone();
    let instance = super::node_registry::resolve_self_instance_id_without_config();
    let provider = provider.to_string();
    let channel = channel.to_string();
    acquire_guard(pool, instance, provider, channel, cancel, permit).map(Some)
}

fn acquire_guard(
    pool: PgPool,
    instance: String,
    provider: String,
    channel: String,
    cancel: Arc<CancelToken>,
    permit: OwnedSemaphorePermit,
) -> Result<ExecutionGuard, String> {
    let nonce = uuid::Uuid::new_v4();
    let handle = tokio::runtime::Handle::current();
    handle.block_on(async {
        tokio::time::timeout(
            LEASE_IO_TIMEOUT,
            store::acquire(&pool, &instance, &provider, &channel, nonce),
        )
        .await
        .map_err(|_| "execution lease admission timed out".to_string())?
        .map_err(|e| format!("execution lease admission: {e}"))
    })?;
    let (stop, mut stopped) = oneshot::channel();
    let (released_tx, released) = oneshot::channel();
    handle.spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                _ = tokio::time::sleep(LEASE_RENEW_INTERVAL) => {
                    let renewed = tokio::time::timeout(LEASE_IO_TIMEOUT,store::renew(&pool,&instance,&provider,&channel,nonce)).await;
                    if !matches!(renewed,Ok(Ok(true))) {
                        tracing::error!(%instance,%provider,%channel,"execution lease lost; cancelling provider");
                        let _ = tokio::task::spawn_blocking(move || cancel.cancel_with_tmux_cleanup()).await;
                        // Keep the local permit until actual execute() returns.
                        break;
                    }
                }
            }
        }
        if let Err(error) = store::release(&pool,&instance,&provider,&channel,nonce).await {
            tracing::warn!(%error,"execution lease release failed; expiry will reclaim it");
        }
        let _ = released_tx.send(());
    });
    Ok(ExecutionGuard {
        stop: Some(stop),
        released: Some(released),
        runtime: handle,
        _permit: permit,
    })
}
