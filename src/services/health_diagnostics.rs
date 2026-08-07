//! DB-backed health diagnostics shared by the health API routes.
//!
//! #5147: every Postgres await reachable from the public `GET /api/health` — the
//! endpoint the self-watchdog probes — is bracketed with
//! [`observe_db`](crate::services::hang_forensics::observe_db), so a kill line
//! can say whether the handler was inside one. A stall here reads to the
//! watchdog as an unresponsive runtime.
//!
//! **How these are counted**, because an earlier draft of this table was short
//! by two and nobody could tell from the text: one acquire is **one `sqlx`
//! executor call that takes the pool** — `.fetch_one` / `.fetch_optional` /
//! `.fetch_all` / `.execute` on a `&PgPool`. Nothing here shares a connection
//! or a transaction, so each such call independently waits on the pool's
//! `acquire_timeout`. Count executor calls, not functions: a helper that issues
//! two queries contributes two, and a helper in *another module* that the
//! health path calls (`auto_queue::cleanup_tasks::…`) contributes its own.
//! Re-derive with `rg '\.(fetch_one|fetch_optional|fetch_all|execute)\(' ` over
//! the call graph below rather than trusting this table.
//!
//! | await site                              | acquires | reached when                                    |
//! |-----------------------------------------|----------|-------------------------------------------------|
//! | `probe_server_up`                       | 1        | always                                          |
//! | `load_dispatch_outbox_stats_pg`         | 4        | always                                          |
//! | `load_auto_queue_cleanup_backlog_pg`    | 2        | always (one of them in `auto_queue::cleanup_tasks`) |
//! | `load_config_audit_report_pg`           | 1        | always                                          |
//! | `load_pipeline_override_report_pg`      | 1        | always                                          |
//! | `load_dispatch_gate_runtime_overrides`  | 1        | a `health_registry` is attached                 |
//! | `is_recent_cluster_worker`              | 1        | …and the node is a cluster standby with none    |
//!
//! That is **9 unconditional sequential acquires**, a 10th whenever the handler
//! has a `health_registry` (`server::routes::health_api`'s `if let Some(ref
//! registry) = state.health_registry` — always true for the `dcserver` runtime
//! the watchdog probes, false for the standalone server), and an 11th when a
//! cluster-standby node reports no providers. Each can block for the pool's
//! `acquire_timeout` (10s), so the worst case is 90s / 100s / 110s against a 5s
//! probe timeout — which is why a merely slow database, not a deadlock, is the
//! leading explanation for these kills.
//!
//! The `load_auto_queue_cleanup_backlog_pg` row is the correction: #5224 added
//! that read and #5142 restored its field to `public_health_json`, and an
//! earlier draft of this table said "seven / eighth / ninth" without them. Two
//! unconditional raw acquires sat on the probed path with `db_in_flight` blind
//! to both — the exact misreading this module exists to prevent, reintroduced
//! by a landing elsewhere. A count that is not re-derived rots.
//!//!
//! Every health-path function here shadows its `Option<&PgPool>` parameter with
//! a [`ProbedPool`](crate::services::hang_forensics::ProbedPool); the three
//! that take a pool directly (`load_dispatch_outbox_stats_pg`,
//! `load_auto_queue_cleanup_backlog_pg`, `load_active_session_audit_rows`) take
//! one in their signature. In those bodies there is no `&PgPool` binding left
//! to hand to `.fetch_one`, so adding an await the normal way stops compiling.
//! That is an accident-stopper, not a capability boundary — `ProbedPool`'s own
//! docs say what it does not cover, including that the handle is extractable
//! from `probe`'s closure. The routes deliberately *not* probes
//! (`load_channel_session_state`, `mark_channel_sessions_disconnected`,
//! `load_failed_dispatch_outbox_rows`,
//! `acknowledge_failed_dispatch_outbox_rows`) keep the raw handle, which is how
//! they stay readable as exemptions.

use serde::Serialize;
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::services::hang_forensics::ProbedPool;

use crate::services::health_active_session_audit::{
    ActiveSessionAuditReport, ActiveSessionAuditSettings, RawSessionRow,
    classify_active_session_audit,
};

pub const OUTBOX_AGE_DEGRADED_SECS: i64 = 60;

pub(crate) const ACTIVE_SESSION_AUDIT_QUERY: &str =
    "SELECT session_key, provider, status, active_dispatch_id, last_heartbeat,
                thread_channel_id, channel_id
           FROM sessions
          WHERE parent_session_id IS NULL
            AND (NULLIF($2, '') IS NULL OR instance_id IS NULL OR instance_id = $2)
            AND (
                status IN ('turn_active', 'working')
                OR COALESCE(btrim(active_dispatch_id), '') <> ''
            )
          ORDER BY last_heartbeat ASC NULLS FIRST, id ASC
          LIMIT $1";

#[derive(Debug, Clone, Serialize)]
pub struct DispatchOutboxStats {
    pub pending: i64,
    pub retrying: i64,
    pub permanent_failures: i64,
    pub oldest_pending_age: i64,
}

/// #5142: standing backlog of the auto-queue post-commit cleanup outbox
/// (`auto_queue_run_cleanup_tasks`).
///
/// `dead_lettered` is the number the module exists for. A cleanup row that burns
/// through `MAX_CLEANUP_ATTEMPTS` (~13–17 minutes of failing retries) is parked
/// permanently: it leaves both drain queries and nothing retries it again, so
/// its run's slot token and residual provider session id stay on disk. Until
/// this gauge existed no counter, query or endpoint read `dead_lettered_at` at
/// all — the only trace was one `tracing::warn!` in the policy tick, which is
/// gone the moment the log rotates. A non-zero value here is an operator action
/// item, not a statistic.
///
/// Surfaced on `/api/health/detail` in full and projected count-only onto the
/// credential-free `/api/health`. It does NOT follow [`DispatchOutboxStats`],
/// which is detail-only: that block has a row-level list endpoint, an ack
/// endpoint and an `agentdesk doctor` Core check behind it, and this one has no
/// reader at all — so hiding it behind the protected router would leave a
/// permanently stalled cleanup outbox reporting `ok: true` and nothing else.
/// `public_auto_queue_cleanup_backlog_is_served_on_the_unauthenticated_endpoint`
/// pins the public half against a real HTTP response.
///
/// `pending` is the live half (rows still owed and still retrying) and is
/// included so the two can be told apart at a glance.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AutoQueueCleanupBacklog {
    pub pending: i64,
    pub dead_lettered: i64,
}

/// Test-only injection point for [`load_auto_queue_cleanup_backlog`].
///
/// The backlog is a PostgreSQL read, so in a unit test the health handler can
/// only ever produce `None` and every claim about *where the block surfaces*
/// would have to be a source-text guard. #5142 r5 deleted two such guards —
/// each defeated by a single adjacent line — and replaced them with
/// router-level tests that inject a backlog here and then assert on the real
/// HTTP body of `/health` and `/health/detail`.
///
/// [`inject`] holds a process-wide lock for the lifetime of its guard, so the
/// injecting tests serialize against each other and leave nothing behind for
/// the rest of the binary.
#[cfg(test)]
pub(crate) mod backlog_probe {
    use super::AutoQueueCleanupBacklog;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static SERIALIZE: OnceLock<Mutex<()>> = OnceLock::new();
    static VALUE: Mutex<Option<AutoQueueCleanupBacklog>> = Mutex::new(None);

    /// Clears the injected value and releases the serialization lock on drop.
    pub(crate) struct InjectedBacklog {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for InjectedBacklog {
        fn drop(&mut self) {
            *VALUE.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
        }
    }

    pub(crate) fn inject(backlog: AutoQueueCleanupBacklog) -> InjectedBacklog {
        let lock = SERIALIZE
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *VALUE.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(backlog);
        InjectedBacklog { _lock: lock }
    }

    pub(crate) fn injected() -> Option<AutoQueueCleanupBacklog> {
        *VALUE.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Load the auto-queue cleanup backlog. `None` when there is no pool or the
/// query fails, which keeps a health probe from turning a diagnostics read into
/// an outage.
pub async fn load_auto_queue_cleanup_backlog(
    pg_pool: Option<&PgPool>,
) -> Option<AutoQueueCleanupBacklog> {
    #[cfg(test)]
    if let Some(injected) = backlog_probe::injected() {
        return Some(injected);
    }
    // #5147 sites 6-7/9: reached unconditionally from `health_api`'s
    // `health_response`, so these two acquires are on the public probe path
    // exactly like the four `dispatch_outbox` counts above. The wrap shadows
    // the raw `&PgPool` out of scope before either of them.
    let pg_pool = ProbedPool::wrap(pg_pool)?;
    match load_auto_queue_cleanup_backlog_pg(pg_pool).await {
        Ok(backlog) => Some(backlog),
        Err(error) => {
            tracing::warn!("[health] failed to load auto_queue_run_cleanup_tasks backlog: {error}");
            None
        }
    }
}

pub(crate) async fn load_auto_queue_cleanup_backlog_pg(
    probed: ProbedPool<'_>,
) -> Result<AutoQueueCleanupBacklog, String> {
    let dead_lettered = probed
        .probe(
            |pool| {
                crate::services::auto_queue::cleanup_tasks::dead_lettered_run_cleanup_task_count_pg(
                    pool,
                )
            },
            Result::is_ok,
        )
        .await?;
    let pending = probed
        .probe(
            |pool| {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*)::BIGINT FROM auto_queue_run_cleanup_tasks
         WHERE dead_lettered_at IS NULL",
                )
                .fetch_one(pool)
            },
            Result::is_ok,
        )
        .await
        .map_err(|error| format!("count pending auto-queue run cleanup tasks: {error}"))?;
    Ok(AutoQueueCleanupBacklog {
        pending,
        dead_lettered,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ChannelSessionState {
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub active_dispatch_id: Option<String>,
    pub thread_channel_id: Option<String>,
}

pub async fn probe_server_up(pg_pool: Option<&PgPool>) -> bool {
    // #5147 site 1/7: the first thing `GET /api/health` awaits. It has no
    // timeout of its own, so it can block for the pool's `acquire_timeout`
    // (10s) — twice the watchdog's 5s read timeout. The wrap shadows the raw
    // `&PgPool` out of scope, so the await below cannot skip the bracket.
    let Some(pg_pool) = ProbedPool::wrap(pg_pool) else {
        return false;
    };
    pg_pool
        .probe(
            |pool| sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(pool),
            Result::is_ok,
        )
        .await
        .is_ok()
}

pub async fn load_config_audit_report_pg(pg_pool: Option<&PgPool>) -> Option<serde_json::Value> {
    // #5147 site 6/7.
    let pg_pool = ProbedPool::wrap(pg_pool)?;
    let raw = pg_pool
        .probe(
            |pool| {
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                    .bind("config_audit_report")
                    .fetch_optional(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

pub async fn load_pipeline_override_report_pg(
    pg_pool: Option<&PgPool>,
) -> Option<serde_json::Value> {
    // #5147 site 7/7 — the last unconditional one.
    let pg_pool = ProbedPool::wrap(pg_pool)?;
    let raw = pg_pool
        .probe(
            |pool| {
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                    .bind("pipeline_override_health_report")
                    .fetch_optional(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

pub async fn load_dispatch_gate_runtime_overrides(
    pg_pool: Option<&PgPool>,
) -> (Option<bool>, Option<u64>) {
    // #5147 conditional 8th site: `health_api::health_response` reaches this
    // only inside `if let Some(ref registry) = state.health_registry`. Always
    // true for the dcserver runtime the watchdog probes; false for the
    // standalone server, which is why the unconditional count is 7 and not 8.
    let Some(pg_pool) = ProbedPool::wrap(pg_pool) else {
        return (None, None);
    };
    let runtime_config = pg_pool
        .probe(
            |pool| {
                sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                    .bind("runtime-config")
                    .fetch_optional(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let (enabled, danger, _stale) =
        crate::services::dispatch_gate::persisted_runtime_overrides(runtime_config.as_ref());
    (enabled, danger)
}

pub async fn is_recent_cluster_worker(
    pg_pool: Option<&PgPool>,
    instance_id: &str,
    lease_ttl_secs: u64,
) -> bool {
    let Some(pg_pool) = ProbedPool::wrap(pg_pool) else {
        return false;
    };
    let instance_id = instance_id.trim();
    if instance_id.is_empty() {
        return false;
    }
    let ttl_secs = lease_ttl_secs.max(1) as f64;
    // #5147 conditional 9th site: reached only when a cluster node with a
    // `health_registry` reports no providers, but it awaits the same pool as
    // the other eight.
    pg_pool
        .probe(
            |pool| {
                sqlx::query_scalar::<_, String>(
                    r#"
        SELECT effective_role
          FROM worker_nodes
         WHERE instance_id = $1
           AND last_heartbeat_at >= NOW() - ($2::double precision * INTERVAL '1 second')
        "#,
                )
                .bind(instance_id)
                .bind(ttl_secs)
                .fetch_optional(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()
        .flatten()
        .as_deref()
        == Some("worker")
}

pub async fn load_channel_session_state(
    pg_pool: Option<&PgPool>,
    channel_id: u64,
) -> Option<ChannelSessionState> {
    let channel_id = channel_id.to_string();
    if let Some(pool) = pg_pool {
        let row = sqlx::query(
            "SELECT agent_id, provider, status, active_dispatch_id, thread_channel_id
               FROM sessions
              WHERE thread_channel_id = $1
              ORDER BY last_heartbeat DESC NULLS LAST, id DESC
              LIMIT 1",
        )
        .bind(&channel_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
        return Some(ChannelSessionState {
            agent_id: row.try_get("agent_id").ok(),
            provider: row.try_get("provider").ok(),
            status: row.try_get("status").ok(),
            active_dispatch_id: row.try_get("active_dispatch_id").ok(),
            thread_channel_id: row.try_get("thread_channel_id").ok(),
        });
    }
    None
}

/// #2049 Finding 16: match the handler-layer definition of "no live work".
pub async fn mark_channel_sessions_disconnected(
    pg_pool: Option<&PgPool>,
    channel_id: u64,
) -> Result<usize, String> {
    let channel_id = channel_id.to_string();
    if let Some(pool) = pg_pool {
        return sqlx::query(
            "UPDATE sessions
                SET status = 'disconnected',
                    active_dispatch_id = NULL
              WHERE thread_channel_id = $1
                AND status IN ('turn_active', 'working')
                AND COALESCE(btrim(active_dispatch_id), '') = ''",
        )
        .bind(&channel_id)
        .execute(pool)
        .await
        .map(|result| result.rows_affected() as usize)
        .map_err(|error| format!("mark postgres sessions disconnected: {error}"));
    }
    Err("postgres pool unavailable".to_string())
}

pub async fn enrich_mailbox_session_state(json: &mut serde_json::Value, pg_pool: Option<&PgPool>) {
    let Some(mailboxes) = json
        .get_mut("mailboxes")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for mailbox in mailboxes {
        let Some(channel_id) = mailbox
            .get("channel_id")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        if let Some(session) = load_channel_session_state(pg_pool, channel_id).await {
            let active_dispatch_present = session
                .active_dispatch_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty());
            mailbox["session_record_present"] = serde_json::json!(true);
            mailbox["session_agent_id"] = serde_json::json!(session.agent_id);
            mailbox["session_provider"] = serde_json::json!(session.provider);
            mailbox["session_status"] = serde_json::json!(session.status);
            mailbox["session_active_dispatch_id"] = serde_json::json!(session.active_dispatch_id);
            mailbox["session_thread_channel_id"] = serde_json::json!(session.thread_channel_id);
            if active_dispatch_present {
                mailbox["active_dispatch_present"] = serde_json::json!(true);
            }
        } else {
            mailbox["session_record_present"] = serde_json::json!(false);
            mailbox["session_status"] = serde_json::Value::Null;
            mailbox["session_active_dispatch_id"] = serde_json::Value::Null;
        }
    }
}

pub async fn build_active_session_audit(
    pg_pool: Option<&PgPool>,
    local_instance_id: Option<&str>,
) -> ActiveSessionAuditReport {
    let runtime = crate::config_live_reload::current().map(|cfg| {
        (
            cfg.runtime.active_session_audit_enabled,
            cfg.runtime.active_session_audit_stale_secs,
            cfg.runtime.active_session_audit_max_candidates,
        )
    });
    let (enabled_override, stale_override, cap_override) = runtime.unwrap_or((None, None, None));
    let settings =
        ActiveSessionAuditSettings::from_overrides(enabled_override, stale_override, cap_override);

    if !settings.enabled {
        return ActiveSessionAuditReport::disabled(settings.stale_secs);
    }

    let Some(pg_pool) = ProbedPool::wrap(pg_pool) else {
        return ActiveSessionAuditReport::disabled(settings.stale_secs);
    };

    let (rows, raw_matches_total) =
        load_active_session_audit_rows(pg_pool, settings.max_candidates, local_instance_id).await;
    let mut resolver = crate::services::session_activity::SessionActivityResolver::new();
    classify_active_session_audit(
        &rows,
        &mut resolver,
        settings,
        raw_matches_total,
        chrono::Utc::now(),
    )
}

async fn load_active_session_audit_rows(
    pool: ProbedPool<'_>,
    max_candidates: u64,
    local_instance_id: Option<&str>,
) -> (Vec<RawSessionRow>, usize) {
    let capped = max_candidates.min(i64::MAX as u64) as usize;
    let limit = max_candidates.saturating_add(1).min(i64::MAX as u64) as i64;
    let local_instance_id = local_instance_id.map(str::trim).unwrap_or("");
    // #5147: `/api/health/detail` only, so not on the watchdog's probe path —
    // bracketed anyway because it is health-exclusive and shares the pool the
    // public probe has to acquire from.
    let rows = match pool
        .probe(
            |pool| {
                sqlx::query(ACTIVE_SESSION_AUDIT_QUERY)
                    .bind(limit)
                    .bind(local_instance_id)
                    .fetch_all(pool)
            },
            Result::is_ok,
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::debug!(
                event = "active_session_audit_query_failed",
                error = %error,
                "active-session audit query failed; emitting empty candidate set"
            );
            return (Vec::new(), 0);
        }
    };
    let raw_matches_seen = rows.len();
    let mapped: Vec<RawSessionRow> = rows
        .iter()
        .take(capped)
        .map(|row| RawSessionRow {
            session_key: row.try_get("session_key").ok(),
            provider: row.try_get("provider").ok(),
            status: row.try_get("status").ok(),
            active_dispatch_id: row.try_get("active_dispatch_id").ok(),
            last_heartbeat: pg_timestamp_to_rfc3339(row, "last_heartbeat"),
            thread_channel_id: row.try_get("thread_channel_id").ok(),
            channel_id: row.try_get("channel_id").ok(),
        })
        .collect();
    (mapped, raw_matches_seen)
}

fn pg_timestamp_to_rfc3339(row: &PgRow, column: &str) -> Option<String> {
    if let Ok(Some(ts)) = row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(column) {
        return Some(ts.to_rfc3339());
    }
    if let Ok(Some(naive)) = row.try_get::<Option<chrono::NaiveDateTime>, _>(column) {
        return Some(naive.format("%Y-%m-%d %H:%M:%S").to_string());
    }
    row.try_get::<Option<String>, _>(column).ok().flatten()
}

pub async fn load_dispatch_outbox_stats(pg_pool: Option<&PgPool>) -> Option<DispatchOutboxStats> {
    if let Some(pg_pool) = ProbedPool::wrap(pg_pool) {
        if let Some(stats) = load_dispatch_outbox_stats_pg(pg_pool).await {
            return Some(stats);
        }
        tracing::warn!("[health] failed to load dispatch_outbox stats from PostgreSQL");
    }
    None
}

/// #5147 sites 2–5/7: four sequential pool acquires, the largest single block
/// of database work on the public health path and therefore the most likely
/// place for the handler to be sitting when the watchdog gives up. An
/// unbracketed await here would report `db_in_flight=0` at kill time and clear
/// the database of the stall it was causing.
async fn load_dispatch_outbox_stats_pg(probed: ProbedPool<'_>) -> Option<DispatchOutboxStats> {
    let pending = probed
        .probe(
            |pool| {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending'",
                )
                .fetch_one(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()?;
    let retrying = probed
        .probe(
            |pool| {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending' AND retry_count > 0",
                )
                .fetch_one(pool)
            },
            Result::is_ok,
        )
        .await
    .ok()?;
    let failed = probed
        .probe(
            |pool| {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'failed'",
                )
                .fetch_one(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()?;
    let oldest_pending_age = probed
        .probe(
            |pool| {
                sqlx::query_scalar::<_, i64>(
                    "SELECT COALESCE(
                     CAST(
                         EXTRACT(
                             EPOCH FROM (NOW() - MIN(COALESCE(next_attempt_at, created_at)))
                         ) AS BIGINT
                     ),
                     0
                 )
                 FROM dispatch_outbox
                 WHERE status = 'pending'
                   AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())",
                )
                .fetch_one(pool)
            },
            Result::is_ok,
        )
        .await
        .ok()?;

    Some(DispatchOutboxStats {
        pending,
        retrying,
        permanent_failures: failed,
        oldest_pending_age,
    })
}

pub async fn load_failed_dispatch_outbox_rows(
    pool: &PgPool,
    ids: Option<&[i64]>,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = if let Some(ids) = ids {
        if ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(
                "SELECT o.id,
                        o.dispatch_id,
                        o.action,
                        o.agent_id,
                        o.card_id,
                        o.title,
                        o.retry_count,
                        o.error,
                        o.delivery_status,
                        o.delivery_result,
                        o.created_at,
                        o.processed_at,
                        td.status AS dispatch_status
                   FROM dispatch_outbox o
              LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
                  WHERE o.status = 'failed'
                    AND o.id = ANY($1)
               ORDER BY o.processed_at DESC NULLS LAST, o.id DESC",
            )
            .bind(ids)
            .fetch_all(pool)
            .await?
        }
    } else {
        sqlx::query(
            "SELECT o.id,
                    o.dispatch_id,
                    o.action,
                    o.agent_id,
                    o.card_id,
                    o.title,
                    o.retry_count,
                    o.error,
                    o.delivery_status,
                    o.delivery_result,
                    o.created_at,
                    o.processed_at,
                    td.status AS dispatch_status
               FROM dispatch_outbox o
          LEFT JOIN task_dispatches td ON td.id = o.dispatch_id
              WHERE o.status = 'failed'
           ORDER BY o.processed_at DESC NULLS LAST, o.id DESC
              LIMIT 100",
        )
        .fetch_all(pool)
        .await?
    };

    rows.into_iter()
        .map(dispatch_outbox_failure_row_json)
        .collect()
}

fn dispatch_outbox_failure_row_json(row: PgRow) -> Result<serde_json::Value, sqlx::Error> {
    Ok(serde_json::json!({
        "id": row.try_get::<i64, _>("id")?,
        "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id")?,
        "action": row.try_get::<String, _>("action")?,
        "agent_id": row.try_get::<Option<String>, _>("agent_id")?,
        "card_id": row.try_get::<Option<String>, _>("card_id")?,
        "title": row.try_get::<Option<String>, _>("title")?,
        "retry_count": row.try_get::<i64, _>("retry_count")?,
        "error": row.try_get::<Option<String>, _>("error")?,
        "delivery_status": row.try_get::<Option<String>, _>("delivery_status")?,
        "delivery_result": row.try_get::<Option<serde_json::Value>, _>("delivery_result")?,
        "created_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("created_at")?,
        "processed_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("processed_at")?,
        "dispatch_status": row.try_get::<Option<String>, _>("dispatch_status")?,
    }))
}

pub async fn acknowledge_failed_dispatch_outbox_rows(
    pool: &PgPool,
    ids: &[i64],
    reason: &str,
) -> Result<Vec<i64>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "UPDATE dispatch_outbox
            SET status = 'acknowledged',
                delivery_status = 'acknowledged',
                delivery_result = jsonb_build_object(
                    'acknowledged_at', NOW(),
                    'reason', $2::TEXT,
                    'previous_delivery_status', delivery_status,
                    'previous_delivery_result', delivery_result
                ),
                claimed_at = NULL,
                claim_owner = NULL
          WHERE status = 'failed'
            AND id = ANY($1)
      RETURNING id",
    )
    .bind(ids)
    .bind(reason)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::{ACTIVE_SESSION_AUDIT_QUERY, AutoQueueCleanupBacklog, DispatchOutboxStats};
    use crate::services::hang_forensics;
    use serde_json::json;

    /// A pool that resolves but can never connect, so every await reaches the
    /// bracket and then fails fast. Port 1 is numeric (no DNS) and refuses
    /// instantly; the short `acquire_timeout` bounds sqlx's retry window.
    ///
    /// This is what makes the instrumentation testable without a database: the
    /// counters must move for a *failed* round trip exactly as for a successful
    /// one, because a stalled probe is precisely the case they exist to report.
    fn unreachable_pool() -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(150))
            .connect_lazy("postgres://127.0.0.1:1/agentdesk_forensics_probe")
            .expect("a lazy pool never connects at construction")
    }

    /// #5147: the breadcrumbs are only trustworthy if *every* health-path
    /// await is bracketed. One that is not reports `db_in_flight=0` while the
    /// handler is stuck inside it — the exact misreading this PR exists to
    /// prevent. `ProbedPool` makes an unbracketed health-path await a type
    /// error rather than a test failure; this pins the other half — that the
    /// bracket which *is* there actually runs and actually records, once per
    /// await, with no leaked in-flight slot.
    #[tokio::test]
    async fn every_health_path_database_await_is_bracketed() {
        let _serial = hang_forensics::counter_test_lock();
        let pool = unreachable_pool();

        macro_rules! assert_bracketed {
            ($label:expr, $expected:expr, $call:expr) => {{
                let before = hang_forensics::snapshot();
                let _ = $call.await;
                let after = hang_forensics::snapshot();
                assert_eq!(
                    after.db_probes_started - before.db_probes_started,
                    $expected,
                    "{} must bracket exactly {} database await(s)",
                    $label,
                    $expected
                );
                assert_eq!(
                    after.db_probes_failed - before.db_probes_failed,
                    $expected,
                    "{} awaits an unreachable database, so every bracketed await \
                     must be recorded as a failed probe",
                    $label
                );
                assert_eq!(
                    after.db_in_flight, before.db_in_flight,
                    "{} must not leak an in-flight slot",
                    $label
                );
            }};
        }

        // The public `GET /api/health` path in handler order, plus the
        // conditional cluster-standby probe.
        assert_bracketed!("probe_server_up", 1, super::probe_server_up(Some(&pool)));
        // 1, not 4: `load_dispatch_outbox_stats_pg` short-circuits on `.ok()?`,
        // so an unreachable database only ever reaches its first await. That
        // makes this assertion cover the first bracket only; the other three
        // are covered by construction — that function is handed a `ProbedPool`
        // and never sees a `&PgPool`, so an unbracketed await in it does not
        // compile.
        assert_bracketed!(
            "load_dispatch_outbox_stats",
            1,
            super::load_dispatch_outbox_stats(Some(&pool))
        );
        // #5224 added this read and #5142 published its field again; both
        // landed while this test existed and neither was covered by it, so the
        // two acquires sat unbracketed on the probed path. 1, not 2: the first
        // `?` short-circuits against an unreachable database.
        assert_bracketed!(
            "load_auto_queue_cleanup_backlog",
            1,
            super::load_auto_queue_cleanup_backlog(Some(&pool))
        );
        assert_bracketed!(
            "load_config_audit_report_pg",
            1,
            super::load_config_audit_report_pg(Some(&pool))
        );
        assert_bracketed!(
            "load_pipeline_override_report_pg",
            1,
            super::load_pipeline_override_report_pg(Some(&pool))
        );
        assert_bracketed!(
            "load_dispatch_gate_runtime_overrides",
            1,
            super::load_dispatch_gate_runtime_overrides(Some(&pool))
        );
        assert_bracketed!(
            "is_recent_cluster_worker",
            1,
            super::is_recent_cluster_worker(Some(&pool), "node-1", 30)
        );
        // Detail-only, bracketed because it is health-exclusive.
        assert_bracketed!(
            "load_active_session_audit_rows",
            1,
            super::load_active_session_audit_rows(
                hang_forensics::ProbedPool::wrap(Some(&pool)).expect("a Some pool wraps"),
                10,
                Some("node-1")
            )
        );
    }

    /// #5142's router tests inject a backlog here and assert on the real HTTP
    /// body, which only works if the injection short-circuits *before* the
    /// database. Bracketing those awaits must not move that early return: an
    /// injected backlog has to cost zero probes and touch no pool.
    #[tokio::test]
    async fn an_injected_backlog_short_circuits_before_any_probe() {
        let _serial = hang_forensics::counter_test_lock();
        let pool = unreachable_pool();
        let _injected = super::backlog_probe::inject(AutoQueueCleanupBacklog {
            pending: 7,
            dead_lettered: 3,
        });

        let before = hang_forensics::snapshot();
        let backlog = super::load_auto_queue_cleanup_backlog(Some(&pool))
            .await
            .expect("an injected backlog is returned"); // agentdesk-audit: allow-unwrap — test assertion
        let after = hang_forensics::snapshot();

        assert_eq!((backlog.pending, backlog.dead_lettered), (7, 3));
        assert_eq!(
            after.db_probes_started, before.db_probes_started,
            "the injected path must not reach the database at all"
        );
        assert_eq!(after.db_in_flight, before.db_in_flight);
    }

    /// A stuck probe must be *visible while it is stuck* — a bracket that only
    /// records on completion would leave `db_in_flight=0` for the whole stall.
    #[tokio::test]
    async fn a_pending_health_await_is_visible_as_in_flight() {
        let _serial = hang_forensics::counter_test_lock();
        let pool = unreachable_pool();

        let before = hang_forensics::snapshot();
        // Boxed, not `pin!`ed: the point of the test is to drop the future
        // itself, and dropping a `Pin<&mut _>` would drop only the pointer.
        let mut probe = Box::pin(super::probe_server_up(Some(&pool)));
        tokio::select! {
            biased;
            _ = &mut probe => panic!("an unreachable pool must not answer within 20ms"),
            () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }

        let during = hang_forensics::snapshot();
        assert_eq!(
            during.db_in_flight,
            before.db_in_flight + 1,
            "a health await that has not returned must read as in flight"
        );

        // Cancelling mid-query must release the slot — this is the shape of a
        // watchdog-killed request, and a leak here would make every later kill
        // line overstate the database.
        drop(probe);
        let after = hang_forensics::snapshot();
        assert_eq!(
            after.db_in_flight, before.db_in_flight,
            "a cancelled health await must not leak an in-flight slot"
        );
    }

    #[test]
    fn active_session_audit_query_filters_foreign_and_background_rows() {
        assert!(ACTIVE_SESSION_AUDIT_QUERY.contains("parent_session_id IS NULL"));
        assert!(ACTIVE_SESSION_AUDIT_QUERY.contains("instance_id IS NULL OR instance_id = $2"));
        assert!(ACTIVE_SESSION_AUDIT_QUERY.contains("thread_channel_id, channel_id"));
        assert!(!ACTIVE_SESSION_AUDIT_QUERY.contains("COALESCE(thread_channel_id, channel_id)"));
    }

    #[test]
    fn dispatch_outbox_stats_json_contract_keeps_field_names() {
        let stats = DispatchOutboxStats {
            pending: 2,
            retrying: 1,
            permanent_failures: 3,
            oldest_pending_age: 60,
        };

        assert_eq!(
            serde_json::to_value(stats).unwrap(),
            json!({
                "pending": 2,
                "retrying": 1,
                "permanent_failures": 3,
                "oldest_pending_age": 60,
            })
        );
    }

    /// #5142: the sibling of `dispatch_outbox_stats_json_contract_keeps_field_names`.
    /// `/api/health/detail` consumers key off these exact names, so a rename is a
    /// silent contract break rather than a compile error.
    #[test]
    fn auto_queue_cleanup_backlog_json_contract_keeps_field_names() {
        let backlog = AutoQueueCleanupBacklog {
            pending: 4,
            dead_lettered: 2,
        };

        assert_eq!(
            serde_json::to_value(backlog).unwrap(),
            json!({
                "pending": 4,
                "dead_lettered": 2,
            })
        );
    }

    #[tokio::test]
    async fn diagnostics_without_pg_pool_stay_safe() {
        assert!(super::load_dispatch_outbox_stats(None).await.is_none());
        assert!(!super::probe_server_up(None).await);
        assert!(!super::is_recent_cluster_worker(None, "node-1", 30).await);

        let audit = super::build_active_session_audit(None, Some("node-1")).await;
        assert!(!audit.enabled);
        assert_eq!(audit.candidate_count, 0);
        assert_eq!(audit.high_confidence_count, 0);
        assert!(audit.candidates.is_empty());
    }
}
