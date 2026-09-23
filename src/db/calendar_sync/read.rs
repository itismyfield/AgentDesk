//! Consistent, bounded projections shared by managed-event detail and list.
use super::{CalendarDbError, PgPool, Row, Uuid, Value};
use serde_json::json;

// Keep operator history responses bounded even after repeated event revisions.
const MAX_OPERATION_HISTORY: i64 = 200;

pub(crate) async fn event_accounts(
    pool: &PgPool,
    event: Uuid,
) -> Result<Vec<String>, CalendarDbError> {
    Ok(sqlx::query_scalar("SELECT b.account_id FROM kakao_calendar_targets t JOIN kakao_calendar_bindings b USING(binding_id) WHERE t.event_id=$1 ORDER BY b.account_id")
        .bind(event).fetch_all(pool).await?)
}

pub(crate) async fn account_checks(
    pool: &PgPool,
    accounts: &[String],
) -> Result<Vec<Value>, CalendarDbError> {
    let rows = sqlx::query(
        "SELECT a.account_id,b.checked_at FROM unnest($1::text[]) AS a(account_id)
        LEFT JOIN kakao_calendar_bindings b USING(account_id) ORDER BY a.account_id",
    )
    .bind(accounts)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "accountId":row.get::<String,_>("account_id"),
                "configured":true,"credentialVerified":false,
                "lastCheckedAt":row.get::<Option<chrono::DateTime<chrono::Utc>>,_>("checked_at"),
                "nextAction":"POST account check for current identity and consent verification"
            })
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    revision: i64,
    content: Value,
    deleted: bool,
    targets: sqlx::types::Json<Vec<Value>>,
}

impl EventRow {
    fn into_json(self) -> Value {
        let targets = self.targets.0;
        let applied = targets
            .iter()
            .filter(|target| {
                target["status"] == "applied" && target["appliedRevision"] == self.revision
            })
            .count();
        let status = if targets
            .iter()
            .any(|target| target["status"] == "needs_reconcile")
        {
            "unknown"
        } else if !targets.is_empty() && applied == targets.len() {
            "success"
        } else if applied > 0 {
            "partial_success"
        } else if !targets.is_empty()
            && targets
                .iter()
                .all(|target| target["status"] == "blocked" || target["status"] == "rejected")
        {
            "failed"
        } else {
            "accepted"
        };
        json!({"eventId":self.id,"revision":self.revision,"content":self.content,"deleted":self.deleted,"status":status,"targets":targets})
    }
}

async fn read_events(
    pool: &PgPool,
    event: Option<Uuid>,
    accounts: Option<&[String]>,
    before: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Value>, CalendarDbError> {
    // A single statement sees one MVCC snapshot. List also avoids per-event round trips.
    let rows = sqlx::query_as::<_, EventRow>(
        "WITH selected AS (
            SELECT e.* FROM kakao_calendar_events e
            WHERE ($1::uuid IS NULL OR e.id=$1)
              AND ($2::text[] IS NULL OR NOT EXISTS (
                SELECT 1 FROM kakao_calendar_targets t
                JOIN kakao_calendar_bindings b USING(binding_id)
                WHERE t.event_id=e.id AND NOT(b.account_id=ANY($2))))
              AND ($3::uuid IS NULL OR e.id < $3)
            ORDER BY e.id DESC LIMIT $4
        )
        SELECT e.id,e.revision,e.content,e.deleted,
            COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'accountId',b.account_id,'appliedRevision',t.applied_revision,
                    'remoteKnown',t.remote_id IS NOT NULL,'status',o.status,
                    'errorCode',o.error_code,'operationId',o.id) ORDER BY b.account_id)
                FROM kakao_calendar_targets t JOIN kakao_calendar_bindings b USING(binding_id)
                JOIN LATERAL (
                    SELECT id,status,error_code FROM kakao_calendar_operations
                    WHERE target_id=t.id
                    ORDER BY (status IN ('needs_reconcile','dispatching','blocked','rejected')) DESC,
                        revision DESC LIMIT 1
                ) o ON TRUE WHERE t.event_id=e.id
            ),'[]'::jsonb) AS targets
        FROM selected e ORDER BY e.id DESC"
    ).bind(event).bind(accounts).bind(before).bind(limit).fetch_all(pool).await?;
    Ok(rows.into_iter().map(EventRow::into_json).collect())
}

pub(crate) async fn get(pool: &PgPool, event: Uuid) -> Result<Value, CalendarDbError> {
    read_events(pool, Some(event), None, None, 1)
        .await?
        .pop()
        .ok_or(CalendarDbError::NotFound)
}

pub(crate) async fn list(
    pool: &PgPool,
    accounts: &[String],
    before: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Value>, CalendarDbError> {
    read_events(pool, None, Some(accounts), before, limit).await
}

pub(crate) async fn operations(pool: &PgPool, event: Uuid) -> Result<Vec<Value>, CalendarDbError> {
    let rows = sqlx::query("SELECT o.id,o.revision,o.action,o.status,o.error_code,o.dispatched_at,o.attempts,o.recovery_note,b.account_id
        FROM kakao_calendar_operations o JOIN kakao_calendar_targets t ON t.id=o.target_id JOIN kakao_calendar_bindings b USING(binding_id)
        WHERE t.event_id=$1 ORDER BY o.revision DESC,b.account_id LIMIT $2").bind(event).bind(MAX_OPERATION_HISTORY).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| json!({"operationId":r.get::<Uuid,_>("id"),"revision":r.get::<i64,_>("revision"),"action":r.get::<String,_>("action"),"status":r.get::<String,_>("status"),"accountId":r.get::<String,_>("account_id"),"errorCode":r.get::<Option<String>,_>("error_code"),"dispatchedAt":r.get::<Option<chrono::DateTime<chrono::Utc>>,_>("dispatched_at"),"attempts":r.get::<i32,_>("attempts"),"recoveryNote":r.get::<Option<String>,_>("recovery_note")})).collect())
}
