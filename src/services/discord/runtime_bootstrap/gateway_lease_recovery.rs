use super::*;

pub(super) static STANDBY_PROMOTION_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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

fn restart_path_nonce(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok().and_then(|request| {
        request
            .lines()
            .find_map(|line| line.strip_prefix("nonce="))
            .map(str::to_owned)
    })
}

pub(super) fn restart_file_nonce(root: &std::path::Path, name: &str) -> Option<String> {
    restart_path_nonce(&root.join(name))
}

fn restart_file_matches(root: &std::path::Path, name: &str, nonce: &str) -> bool {
    restart_file_nonce(root, name).as_deref() == Some(nonce)
}

/// #5254 §2-3: a nonce is a pathname component under D11, so it is validated
/// rather than trusted. Whole-string, never line-anchored — a `grep -Eqx`-shaped
/// gate succeeds on any one matching line, so it admits both `x\n../escape` and
/// `x\nescape`: the clean `x` line alone satisfies it and the rest smuggles.
fn nonce_is_path_safe(nonce: &str) -> bool {
    !nonce.is_empty()
        && nonce != "."
        && nonce != ".."
        && nonce.len() <= 128
        && nonce
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// #5254 D11-1: the per-request immutable name of a restart artifact. `None` is
/// the fail-closed disposition of a nonce that fails the charset gate: no
/// caller ever builds a path out of an unvalidated string.
pub(super) fn restart_request_artifact_path(
    root: &std::path::Path,
    name: &str,
    nonce: &str,
) -> Option<std::path::PathBuf> {
    nonce_is_path_safe(nonce).then(|| root.join(format!("{name}.{nonce}")))
}

/// #5254 D11-3 as corrected by ERRATUM R3-E5: the terminal read is three-valued
/// and only the per-request identity name carries green authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalProof {
    /// The identity artifact exists and its body nonce agrees. I2c/I3 back it.
    Proven,
    /// Only the fixed-name index carries this nonce. NOT a durability proof: a
    /// pre-I2c publisher both retracts such a proof after its post-rename
    /// re-check [E1] and survives publishing it under supersession [E2]. It
    /// still beats `Absent`, which resolves nothing by itself: this module has
    /// no timer, so an `Absent` read keeps polling until the marker disappears
    /// (`Cancelled`), is replaced by another nonce (`Superseded`), or a terminal
    /// artifact for this nonce appears. The timeout belongs to the shell gate.
    LegacyIndexOnly,
    Absent,
}

/// Three-valued, identity-first read of one restart artifact family.
pub(super) fn restart_artifact_proof(
    root: &std::path::Path,
    name: &str,
    nonce: &str,
) -> TerminalProof {
    let Some(identity) = restart_request_artifact_path(root, name, nonce) else {
        // ERRATUM §E5.4: an unsafe nonce yields `Absent`, not a fallback read.
        // The index is not a promotion path, so there is nothing to fall back
        // to and no route by which an unvalidated nonce becomes evidence. The
        // label is §E5.4's other half: §E5.8 4-R leaves diagnostics as the only
        // residual risk of a non-authoritative index, and a silent `Absent` here
        // leaves an operator watching a fence with nothing naming why.
        tracing::warn!(
            root = %root.display(),
            name = name,
            "restart-nonce-unsafe: refusing to read a terminal artifact for an unvalidated nonce"
        );
        return TerminalProof::Absent;
    };
    if restart_path_nonce(&identity).as_deref() == Some(nonce) {
        return TerminalProof::Proven;
    }
    if restart_file_matches(root, name, nonce) {
        return TerminalProof::LegacyIndexOnly;
    }
    TerminalProof::Absent
}

/// ERRATUM §E5.2 `terminal_proof(root, nonce)`.
pub(super) fn terminal_proof(root: &std::path::Path, nonce: &str) -> TerminalProof {
    restart_artifact_proof(root, "restart_persisted", nonce)
}

/// #5254 D1 + D11-1: stage-then-link, twice. The body lands in a dot-prefixed
/// stage that no reader resolves, so both reachable names — the identity name
/// and the canonical lease — are hard links to an already complete inode and I1
/// has no partial-creation window on either.
///
/// `Ok(true)` acquired the lease; `Ok(false)` is the honest "another request
/// holds it". Every `Err` is a fail-closed refusal (unsafe nonce, reused nonce,
/// I/O), and no path publishes the canonical lease without its identity name.
pub(super) fn try_create_restart_marker(
    root: &std::path::Path,
    nonce: &str,
    request: &str,
) -> std::io::Result<bool> {
    use std::io::Write;
    let Some(identity) = restart_request_artifact_path(root, "restart_pending", nonce) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "restart-nonce-unsafe: refusing to build a marker pathname",
        ));
    };
    let stage = root.join(format!(
        ".restart_pending.stage.{nonce}.{}",
        uuid::Uuid::new_v4()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)?;
    let staged = file
        .write_all(request.as_bytes())
        .and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&stage);
        return Err(error);
    }

    // (1) identity link. EEXIST on this name means the nonce was reused.
    let linked = std::fs::hard_link(&stage, &identity);
    let _ = std::fs::remove_file(&stage);
    if let Err(error) = linked {
        return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "restart-nonce-reused: an identity marker for this nonce already exists",
            )
        } else {
            error
        });
    }

    // (2) lease link. EEXIST on this name is the honest refusal. Drop our own
    // identity name on the way out — I2d makes that unlink safe because no
    // other request can name it — so a refusal leaves no orphan behind.
    match std::fs::hard_link(&identity, root.join("restart_pending")) {
        Ok(()) => Ok(true),
        Err(error) => {
            let _ = std::fs::remove_file(&identity);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

/// #5254 D11-2 + ERRATUM §E5.2/§E8.2: publish the terminal artifact
/// identity-first.
///
/// The identity name is the only green authority; the fixed-name index is a
/// hard link *derived* from it. Within the current request's terminal-proof
/// judging window — before this identity can become trailing-cleanup or
/// retention-sweep material — that derivation is what lets
/// `restart_artifact_proof` attribute an identity-less index to a pre-I2c
/// publisher. Outside that window the same state is reachable from our own
/// publish, and R3-E5 correction 1 already disposes of it as non-green
/// fail-forward. So a nonce that cannot spell an identity name refuses the
/// commit outright (E5.2 corollary), and §E8.2 gates the index on the first
/// `fsync_parent_dir`: while the identity's directory entry is not known to be
/// durable, a second pathname would *manufacture* that identity-less shape
/// inside the judging window rather than inherit it.
///
/// The `atomic_write` rename is the point of no return: the latch takes it
/// before any other call can fail, and everything after it is log-only or
/// skipped, because the outcome is already decided and I2c owes the caller an
/// exit.
pub(super) fn publish_restart_terminal(
    root: &std::path::Path,
    nonce: &str,
    body: &str,
) -> std::io::Result<()> {
    let Some(identity) = restart_request_artifact_path(root, "restart_persisted", nonce) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "restart-nonce-unsafe: refusing to publish a terminal artifact",
        ));
    };
    runtime_store::atomic_write(&identity, body).map_err(std::io::Error::other)?;
    latch_commit(nonce);
    if let Err(error) = runtime_store::fsync_parent_dir(&identity) {
        tracing::warn!(%error, "restart persisted parent dir fsync failed; commit proceeds");
        return Ok(());
    }
    let staged = root.join(format!(".restart_persisted.idx.{}", uuid::Uuid::new_v4()));
    if let Err(error) = std::fs::hard_link(&identity, &staged)
        .and_then(|()| std::fs::rename(&staged, root.join("restart_persisted")))
    {
        let _ = std::fs::remove_file(&staged);
        tracing::warn!(%error, "restart persisted index refresh failed; commit proceeds");
    }
    Ok(())
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
        // it before cancellation because clear may arrive after durable commit,
        // and because a rename that carried our nonce necessarily happened
        // after that cancellation (ERRATUM §E5.2 priority order).
        //
        // #5254 D8: nonce equality is the sole authority here. The retired
        // mtime lifetime conjunction was a wall-clock comparison, so a clock
        // regression made an honest commit look non-current and folded a real
        // handoff into a cancellation.
        match terminal_proof(root, nonce) {
            TerminalProof::Proven => return PromotionHandoffOutcome::Committed,
            TerminalProof::LegacyIndexOnly => {
                // R3-E5: the index is not a durability authority, and this arm
                // asserts no durability of its own. It decides more than the
                // shared fence, though: `Committed` keeps every runtime fenced,
                // returns `true` out of `attempt_clean_standby_promotion`, and
                // so ends `spawn_standby_gateway_retry`'s loop for good behind a
                // success log that does not restate this demotion. That is
                // deliberately main's disposition, which #5254 S1 must preserve
                // for a nonce-bearing marker: a pre-I2c publisher that already
                // renamed our nonce into place has resolved the handoff, so stop
                // retrying the lease. What this arm cannot claim is that nothing
                // promotes an index observation — the deploy gate in
                // `scripts/_defaults.sh` still matches this fixed-name body by
                // nonce and calls that green (`acknowledged:nonce`) until S4/S5
                // remove it. The label below is this arm's honesty, not its fix.
                tracing::warn!(
                    root = %root.display(),
                    "GATEWAY-LEASE: restart handoff resolved by a legacy fixed-name publisher; durability is not proven for this nonce"
                );
                return PromotionHandoffOutcome::Committed;
            }
            TerminalProof::Absent => {}
        }
        if restart_artifact_proof(root, "restart_cancelled", nonce) != TerminalProof::Absent {
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
                // #5254 D8: no nonce-free shortcut. A marker that carries no
                // nonce cannot attribute a terminal artifact to any request, so
                // the chain folds to cancellation and the caller restores its
                // preflight fence and resumes lease retry.
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

pub(super) async fn attempt_clean_standby_promotion(
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
    let request = format!(
        "nonce={nonce}\nreason=gateway_standby_promotion\nprovider={}\n",
        provider.as_str()
    );
    match try_create_restart_marker(&root, &nonce, &request) {
        Ok(true) => {}
        Ok(false) => {
            // A deploy/restart request already owns the marker. Monitor that
            // nonce as the process-wide handoff owner: if it commits, the shared
            // fence stays closed; if it is cancelled/removed before commit, this
            // promotion must restore its preflight fence and resume lease retry.
            let Some(existing_nonce) = restart_file_nonce(&root, "restart_pending") else {
                // #5254 D8: fail closed. "Some persisted acknowledgement is
                // present" is not proof that the marker we lost the race to was
                // committed — a nonce-free read cannot attribute a terminal
                // artifact to a request, and the mtime lifetime gate that used
                // to stand in for attribution is retired. Restore the preflight
                // fence and let the retry loop take another lease attempt.
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
