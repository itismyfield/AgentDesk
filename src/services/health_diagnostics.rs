//! DB-backed health diagnostics shared by the health API routes.

use serde::Serialize;
use sqlx::{PgPool, Row, postgres::PgRow};

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

#[derive(Clone, Debug, Serialize)]
pub struct ChannelSessionState {
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub status: Option<String>,
    pub active_dispatch_id: Option<String>,
    pub thread_channel_id: Option<String>,
}

pub async fn probe_server_up(pg_pool: Option<&PgPool>) -> bool {
    if let Some(pool) = pg_pool {
        return sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(pool)
            .await
            .is_ok();
    }
    false
}

pub async fn load_config_audit_report_pg(pg_pool: Option<&PgPool>) -> Option<serde_json::Value> {
    let pool = pg_pool?;
    let raw = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind("config_audit_report")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

pub async fn load_pipeline_override_report_pg(
    pg_pool: Option<&PgPool>,
) -> Option<serde_json::Value> {
    let pool = pg_pool?;
    let raw = sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind("pipeline_override_health_report")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
    serde_json::from_str(&raw).ok()
}

pub async fn load_dispatch_gate_runtime_overrides(
    pg_pool: Option<&PgPool>,
) -> (Option<bool>, Option<u64>) {
    let Some(pool) = pg_pool else {
        return (None, None);
    };
    let runtime_config =
        sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
            .bind("runtime-config")
            .fetch_optional(pool)
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
    let Some(pool) = pg_pool else {
        return false;
    };
    let instance_id = instance_id.trim();
    if instance_id.is_empty() {
        return false;
    }
    let ttl_secs = lease_ttl_secs.max(1) as f64;
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

    let Some(pool) = pg_pool else {
        return ActiveSessionAuditReport::disabled(settings.stale_secs);
    };

    let (rows, raw_matches_total) =
        load_active_session_audit_rows(pool, settings.max_candidates, local_instance_id).await;
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
    pool: &PgPool,
    max_candidates: u64,
    local_instance_id: Option<&str>,
) -> (Vec<RawSessionRow>, usize) {
    let capped = max_candidates.min(i64::MAX as u64) as usize;
    let limit = max_candidates.saturating_add(1).min(i64::MAX as u64) as i64;
    let local_instance_id = local_instance_id.map(str::trim).unwrap_or("");
    let query = sqlx::query(ACTIVE_SESSION_AUDIT_QUERY)
        .bind(limit)
        .bind(local_instance_id);
    let rows = match query.fetch_all(pool).await {
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
    if let Some(pool) = pg_pool {
        if let Some(stats) = load_dispatch_outbox_stats_pg(pool).await {
            return Some(stats);
        }
        tracing::warn!("[health] failed to load dispatch_outbox stats from PostgreSQL");
    }
    None
}

async fn load_dispatch_outbox_stats_pg(pool: &PgPool) -> Option<DispatchOutboxStats> {
    let pending = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending'",
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let retrying = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'pending' AND retry_count > 0",
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let failed = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE status = 'failed'",
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let oldest_pending_age = sqlx::query_scalar::<_, i64>(
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
    use super::{ACTIVE_SESSION_AUDIT_QUERY, DispatchOutboxStats};
    use serde_json::json;

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
