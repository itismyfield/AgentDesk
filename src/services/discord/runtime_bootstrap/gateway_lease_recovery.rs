use super::*;

pub(super) const GATEWAY_STANDBY_RETRY_MIN_SECS: u64 = 30;
pub(super) const GATEWAY_STANDBY_RETRY_JITTER_SECS: u64 = 30;
pub(super) const GATEWAY_ORPHAN_MIN_AGE_SECS: i64 = 30 * 60;
pub(super) const GATEWAY_LEASE_APPLICATION_PREFIX: &str = "agentdesk:gateway:";

pub(super) fn gateway_lease_application_name(provider: &ProviderKind) -> String {
    let config = crate::config::load_graceful();
    let instance_id = config
        .cluster
        .instance_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config()
        });
    format!(
        "{GATEWAY_LEASE_APPLICATION_PREFIX}{instance_id}:{}:{}",
        std::process::id(),
        provider.as_str()
    )
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(super) struct GatewayLeaseHolder {
    pub(super) pid: i32,
    pub(super) application_name: String,
    pub(super) instance_id: Option<String>,
    pub(super) node_status: Option<String>,
    pub(super) heartbeat_recent: Option<bool>,
    pub(super) process_matches: Option<bool>,
}

pub(super) fn gateway_holder_is_reapable(holder: &GatewayLeaseHolder) -> bool {
    holder
        .application_name
        .starts_with(GATEWAY_LEASE_APPLICATION_PREFIX)
        && holder.instance_id.is_some()
        && holder.node_status.as_deref() != Some("online")
        && holder.heartbeat_recent == Some(false)
        && holder.process_matches == Some(true)
}

pub(super) async fn reap_orphaned_gateway_lease_once(
    pool: &sqlx::PgPool,
    lock_id: i64,
    provider: &ProviderKind,
) -> Result<bool, String> {
    let self_pid = std::process::id() as i32;
    let holder = sqlx::query_as::<_, GatewayLeaseHolder>(
        r#"
        SELECT a.pid,
               a.application_name,
               n.instance_id,
               n.status AS node_status,
               (n.last_heartbeat_at >= NOW() - ($3::BIGINT * INTERVAL '1 second')) AS heartbeat_recent,
               (n.process_id = a.pid) AS process_matches
          FROM pg_locks l
          JOIN pg_stat_activity a ON a.pid = l.pid
          LEFT JOIN worker_nodes n
            ON a.application_name = $4 || n.instance_id || ':' || a.pid::TEXT || ':' || $5
         WHERE l.locktype = 'advisory'
           AND l.granted
           AND l.classid = (($1::BIGINT >> 32) & 4294967295)::OID
           AND l.objid = ($1::BIGINT & 4294967295)::OID
           AND l.objsubid = 1
           AND a.pid <> $2
           AND a.state = 'idle'
           AND a.backend_start < NOW() - ($3::BIGINT * INTERVAL '1 second')
        "#,
    )
    .bind(lock_id)
    .bind(self_pid)
    .bind(GATEWAY_ORPHAN_MIN_AGE_SECS)
    .bind(GATEWAY_LEASE_APPLICATION_PREFIX)
    .bind(provider.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("inspect discord gateway lease holder: {error}"))?;

    let Some(holder) = holder else {
        return Ok(false);
    };
    if !gateway_holder_is_reapable(&holder) {
        tracing::warn!(
            pid = holder.pid,
            application_name = %holder.application_name,
            instance_id = ?holder.instance_id,
            node_status = ?holder.node_status,
            heartbeat_recent = ?holder.heartbeat_recent,
            process_matches = ?holder.process_matches,
            "GATEWAY-LEASE: stale-looking holder failed orphan safety checks; leaving it untouched"
        );
        return Ok(false);
    }

    let terminated = sqlx::query_scalar::<_, bool>("SELECT pg_terminate_backend($1)")
        .bind(holder.pid)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("terminate orphaned discord gateway lease holder: {error}"))?;
    if terminated {
        tracing::warn!(
            pid = holder.pid,
            instance_id = holder.instance_id.as_deref().unwrap_or("unknown"),
            provider = provider.as_str(),
            "GATEWAY-LEASE: terminated orphaned stale lease backend"
        );
    }
    Ok(terminated)
}

pub(super) fn standby_retry_delay() -> Duration {
    use rand::Rng;
    Duration::from_secs(
        GATEWAY_STANDBY_RETRY_MIN_SECS
            + rand::thread_rng().gen_range(0..=GATEWAY_STANDBY_RETRY_JITTER_SECS),
    )
}

/// Retry a confirmed standby lease until it becomes available. The provider's
/// `SharedData` and intake workers are already live, so starting a second gateway
/// in place would duplicate runtime state. Releasing the probe lease and exiting
/// the process lets launchd rebuild every provider atomically on a clean runtime.
pub(super) async fn spawn_standby_gateway_retry(
    shared: Arc<SharedData>,
    token_hash: String,
    provider: ProviderKind,
) {
    let Some(pool) = shared.pg_pool.clone() else {
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(standby_retry_delay()).await;
            if shared
                .restart
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            if shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::Acquire)
                > 0
                || shared
                    .restart
                    .global_finalizing
                    .load(std::sync::atomic::Ordering::Acquire)
                    > 0
            {
                continue;
            }
            match super::gateway_lease::try_acquire_discord_gateway_lease(
                &pool,
                &token_hash,
                &provider,
            )
            .await
            {
                Ok(Some(lease)) => {
                    drop(lease);
                    tracing::warn!(
                        provider = provider.as_str(),
                        "GATEWAY-LEASE: standby observed an available lease; exiting for clean launchd promotion"
                    );
                    std::process::exit(0);
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    provider = provider.as_str(),
                    "GATEWAY-LEASE: standby retry failed: {error}"
                ),
            }
        }
    });
}
