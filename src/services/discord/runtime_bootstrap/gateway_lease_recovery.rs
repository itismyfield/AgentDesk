use super::*;

pub(super) static STANDBY_PROMOTION_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static RESTART_ARTIFACT_BOOT_INSTANT: std::sync::OnceLock<std::time::SystemTime> =
    std::sync::OnceLock::new();

pub(super) const GATEWAY_STANDBY_RETRY_MIN_SECS: u64 = 30;
pub(super) const GATEWAY_STANDBY_RETRY_JITTER_SECS: u64 = 30;
pub(super) const GATEWAY_ORPHAN_MIN_AGE_SECS: i64 = 30 * 60;
pub(super) const GATEWAY_LEASE_APPLICATION_PREFIX: &str = "agentdesk:gateway:";

fn gateway_instance_tag(instance_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(instance_id.as_bytes());
    hex::encode(&digest[..8])
}

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
    gateway_lease_application_name_for(&instance_id, std::process::id(), provider.as_str())
}

pub(super) fn gateway_lease_application_name_for(
    instance_id: &str,
    dcserver_pid: u32,
    provider: &str,
) -> String {
    let name = format!(
        "{GATEWAY_LEASE_APPLICATION_PREFIX}{}:{dcserver_pid}:{provider}",
        gateway_instance_tag(instance_id)
    );
    debug_assert!(name.len() <= 63);
    name
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(super) struct GatewayLeaseHolder {
    pub(super) pid: i32,
    pub(super) application_name: String,
    pub(super) instance_id: Option<String>,
    pub(super) node_status: Option<String>,
    pub(super) heartbeat_recent: Option<bool>,
    pub(super) process_matches: Option<bool>,
    pub(super) dcserver_pid: Option<i32>,
}

pub(super) fn gateway_holder_is_reapable(holder: &GatewayLeaseHolder) -> bool {
    holder
        .application_name
        .starts_with(GATEWAY_LEASE_APPLICATION_PREFIX)
        && holder.instance_id.is_some()
        && holder.node_status.as_deref() != Some("online")
        && holder.heartbeat_recent == Some(false)
        && holder.process_matches == Some(true)
        && holder.dcserver_pid.is_some()
}

pub(super) async fn reap_orphaned_gateway_lease_once(
    pool: &sqlx::PgPool,
    lock_id: i64,
    provider: &ProviderKind,
) -> Result<bool, String> {
    reap_orphaned_gateway_lease_with_min_age(pool, lock_id, provider, GATEWAY_ORPHAN_MIN_AGE_SECS)
        .await
}

pub(super) async fn reap_orphaned_gateway_lease_with_min_age(
    pool: &sqlx::PgPool,
    lock_id: i64,
    provider: &ProviderKind,
    min_age_secs: i64,
) -> Result<bool, String> {
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
    reap_orphaned_gateway_lease_for_instance_with_min_age(
        pool,
        lock_id,
        provider,
        min_age_secs,
        &instance_id,
    )
    .await
}

pub(super) async fn reap_orphaned_gateway_lease_for_instance_with_min_age(
    pool: &sqlx::PgPool,
    lock_id: i64,
    provider: &ProviderKind,
    min_age_secs: i64,
    instance_id: &str,
) -> Result<bool, String> {
    let instance_tag = gateway_instance_tag(instance_id);
    let holder = sqlx::query_as::<_, GatewayLeaseHolder>(
        r#"
        SELECT a.pid,
               a.application_name,
               n.instance_id,
               n.status AS node_status,
               (n.last_heartbeat_at >= NOW() - ($2::BIGINT * INTERVAL '1 second')) AS heartbeat_recent,
               (n.process_id IS NOT NULL) AS process_matches,
               parsed[2]::INTEGER AS dcserver_pid
          FROM pg_locks l
          JOIN pg_stat_activity a ON a.pid = l.pid
          LEFT JOIN LATERAL regexp_match(
              a.application_name,
              '^agentdesk:gateway:([0-9a-f]{16}):([0-9]+):([^:]+)$'
          ) parsed ON TRUE
          LEFT JOIN worker_nodes n
            ON n.instance_id = $5
           AND n.process_id = parsed[2]::INTEGER
           AND parsed[1] = $6
           AND parsed[3] = $4
         WHERE l.locktype = 'advisory'
           AND l.granted
           AND l.classid = (($1::BIGINT >> 32) & 4294967295)::OID
           AND l.objid = ($1::BIGINT & 4294967295)::OID
           AND l.objsubid = 1
           AND a.pid <> pg_backend_pid()
           AND a.application_name LIKE $3 || '%'
           AND a.state = 'idle'
           AND a.state_change < NOW() - ($2::BIGINT * INTERVAL '1 second')
           AND a.backend_start < NOW() - ($2::BIGINT * INTERVAL '1 second')
        "#,
    )
    .bind(lock_id)
    .bind(min_age_secs)
    .bind(GATEWAY_LEASE_APPLICATION_PREFIX)
    .bind(provider.as_str())
    .bind(instance_id)
    .bind(&instance_tag)
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

pub(super) fn record_restart_artifact_boot_instant() {
    let _ = RESTART_ARTIFACT_BOOT_INSTANT.set(std::time::SystemTime::now());
}

fn restart_artifact_boot_instant() -> std::time::SystemTime {
    *RESTART_ARTIFACT_BOOT_INSTANT.get_or_init(std::time::SystemTime::now)
}

pub(super) fn restart_artifact_is_current_lifetime(root: &std::path::Path, name: &str) -> bool {
    restart_artifact_is_newer_than(root, name, restart_artifact_boot_instant())
}

pub(super) fn restart_artifact_is_newer_than(
    root: &std::path::Path,
    name: &str,
    boot_instant: std::time::SystemTime,
) -> bool {
    std::fs::metadata(root.join(name))
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified >= boot_instant)
}

pub(super) fn standby_retry_delay() -> Duration {
    use rand::Rng;
    Duration::from_secs(
        GATEWAY_STANDBY_RETRY_MIN_SECS
            + rand::thread_rng().gen_range(0..=GATEWAY_STANDBY_RETRY_JITTER_SECS),
    )
}

fn runtime_is_idle(shared: &SharedData) -> bool {
    shared
        .restart
        .global_active
        .load(std::sync::atomic::Ordering::Acquire)
        == 0
        && shared
            .restart
            .global_finalizing
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
}

fn unfence_runtimes(runtimes: &[Arc<SharedData>]) {
    for runtime in runtimes {
        runtime.restart.intake_worker_lifecycle.unfence_admission();
        runtime
            .restart
            .restart_pending
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) fn restart_file_nonce(root: &std::path::Path, name: &str) -> Option<String> {
    std::fs::read_to_string(root.join(name))
        .ok()
        .and_then(|request| {
            request
                .lines()
                .find_map(|line| line.strip_prefix("nonce="))
                .map(str::to_owned)
        })
}

fn restart_file_matches(root: &std::path::Path, name: &str, nonce: &str) -> bool {
    restart_file_nonce(root, name).as_deref() == Some(nonce)
}

pub(super) fn try_create_restart_marker(
    marker: &std::path::Path,
    request: &str,
) -> std::io::Result<bool> {
    use std::io::Write;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(error),
    };
    if let Err(error) = file
        .write_all(request.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(marker);
        return Err(error);
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PromotionHandoffOutcome {
    Committed,
    Cancelled,
    Superseded,
}

pub(super) async fn wait_for_promotion_handoff(
    root: &std::path::Path,
    nonce: &str,
) -> PromotionHandoffOutcome {
    loop {
        // A matching persisted acknowledgement is the point of no return. Check
        // it before cancellation because clear may arrive after durable commit.
        if restart_file_matches(root, "restart_persisted", nonce)
            && restart_artifact_is_current_lifetime(root, "restart_persisted")
        {
            return PromotionHandoffOutcome::Committed;
        }
        if restart_file_matches(root, "restart_cancelled", nonce)
            && restart_artifact_is_current_lifetime(root, "restart_cancelled")
        {
            return PromotionHandoffOutcome::Cancelled;
        }
        match std::fs::read_to_string(root.join("restart_pending")) {
            Ok(request) if request.lines().any(|line| line == format!("nonce={nonce}")) => {}
            Ok(_) => return PromotionHandoffOutcome::Superseded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return PromotionHandoffOutcome::Cancelled;
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn follow_promotion_handoff_chain(
    root: &std::path::Path,
    initial_nonce: String,
) -> PromotionHandoffOutcome {
    let mut nonce = initial_nonce;
    loop {
        match wait_for_promotion_handoff(root, &nonce).await {
            PromotionHandoffOutcome::Superseded => {
                // The process-wide cancellation owner transfers to the current
                // pending nonce. Keep following until the chain commits or is
                // cancelled; no provider poller is assumed to have observed it.
                if restart_file_nonce(root, "restart_pending").as_deref() == Some(nonce.as_str()) {
                    continue;
                }
                if let Some(next_nonce) = restart_file_nonce(root, "restart_pending") {
                    nonce = next_nonce;
                    continue;
                }
                if restart_artifact_is_current_lifetime(root, "restart_persisted") {
                    return PromotionHandoffOutcome::Committed;
                }
                return PromotionHandoffOutcome::Cancelled;
            }
            terminal => return terminal,
        }
    }
}

pub(super) fn recover_cancelled_promotion(runtimes: &[Arc<SharedData>]) {
    unfence_runtimes(runtimes);
    STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
}

async fn attempt_clean_standby_promotion(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    lease: crate::db::postgres::AdvisoryLockLease,
) -> bool {
    if STANDBY_PROMOTION_IN_PROGRESS
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        drop(lease);
        return false;
    }

    let runtimes = shared
        .health_registry
        .upgrade()
        .map(|registry| async move { registry.provider_runtimes().await })
        .expect("registered standby keeps the process health registry alive")
        .await;
    for runtime in &runtimes {
        runtime.restart.intake_worker_lifecycle.fence_admission();
        runtime
            .restart
            .restart_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    for runtime in &runtimes {
        runtime
            .restart
            .intake_worker_lifecycle
            .wait_until_drained()
            .await;
    }

    if !runtime_is_idle(shared) {
        drop(lease);
        unfence_runtimes(&runtimes);
        STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
        return false;
    }

    for runtime in &runtimes {
        let runtime_provider = runtime.provider.clone();
        let drain = mailbox_restart_drain_all(runtime, &runtime_provider).await;
        if drain.queued_count > 0 || !drain.persistence_errors.is_empty() {
            drop(lease);
            unfence_runtimes(&runtimes);
            STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
            return false;
        }
    }

    drop(lease);
    let Some(root) = crate::agentdesk_runtime_root() else {
        unfence_runtimes(&runtimes);
        STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
        return false;
    };
    let nonce = uuid::Uuid::new_v4().to_string();
    let marker = root.join("restart_pending");
    let request = format!(
        "nonce={nonce}\nreason=gateway_standby_promotion\nprovider={}\n",
        provider.as_str()
    );
    match try_create_restart_marker(&marker, &request) {
        Ok(true) => {}
        Ok(false) => {
            // A deploy/restart request already owns the marker. Monitor that
            // nonce as the process-wide handoff owner: if it commits, the shared
            // fence stays closed; if it is cancelled/removed before commit, this
            // promotion must restore its preflight fence and resume lease retry.
            let Some(existing_nonce) = restart_file_nonce(&root, "restart_pending") else {
                // The committer publishes restart_persisted before removing the
                // pending marker. Presence of any persisted acknowledgement is
                // therefore sufficient proof that this marker was committed.
                if restart_artifact_is_current_lifetime(&root, "restart_persisted") {
                    STANDBY_PROMOTION_IN_PROGRESS
                        .store(false, std::sync::atomic::Ordering::Release);
                    return true;
                }
                recover_cancelled_promotion(&runtimes);
                return false;
            };
            return match follow_promotion_handoff_chain(&root, existing_nonce).await {
                PromotionHandoffOutcome::Committed => {
                    STANDBY_PROMOTION_IN_PROGRESS
                        .store(false, std::sync::atomic::Ordering::Release);
                    true
                }
                PromotionHandoffOutcome::Cancelled => {
                    recover_cancelled_promotion(&runtimes);
                    false
                }
                PromotionHandoffOutcome::Superseded => {
                    unreachable!("handoff chain resolves supersession internally")
                }
            };
        }
        Err(error) => {
            tracing::error!(%error, "GATEWAY-LEASE: failed to publish standby promotion restart marker");
            unfence_runtimes(&runtimes);
            STANDBY_PROMOTION_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
            return false;
        }
    }
    // Keep every runtime fenced while the process-wide owner watches the nonce.
    // A cancellation may remove the marker before any 10s provider poller sees
    // it; this owner still restores every runtime and permits lease retries.
    match follow_promotion_handoff_chain(&root, nonce).await {
        PromotionHandoffOutcome::Committed => true,
        PromotionHandoffOutcome::Cancelled => {
            recover_cancelled_promotion(&runtimes);
            false
        }
        PromotionHandoffOutcome::Superseded => {
            unreachable!("handoff chain resolves supersession internally")
        }
    }
}

/// Retry a confirmed standby lease until it becomes available. The provider's
/// `SharedData` and intake workers are already live, so promotion uses the
/// existing fenced deferred-restart path rather than constructing a second
/// gateway in place.
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
            match super::gateway_lease::try_acquire_discord_gateway_lease(
                &pool,
                &token_hash,
                &provider,
            )
            .await
            {
                Ok(Some(lease)) => {
                    if attempt_clean_standby_promotion(&shared, &provider, lease).await {
                        tracing::warn!(
                            provider = provider.as_str(),
                            "GATEWAY-LEASE: standby published a fenced graceful promotion restart"
                        );
                        return;
                    }
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
