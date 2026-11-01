//! Owns PR tracking writes; lifecycle callers supply their transaction.

use rquickjs::{Ctx, Function, Object, Result as JsResult};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

const HANDED_OFF_PREFIX: &str = "handed-off:";
const CREATE_PR_STATES: [&str; 2] = ["create-pr", "escalated"];

#[derive(Clone, Debug, Deserialize)]
pub(super) struct HandoffPayload {
    pub(super) card_id: String,
    pub(super) repo_id: String,
    pub(super) worktree_path: Option<String>,
    pub(super) branch: String,
    pub(super) head_sha: Option<String>,
    pub(super) agent_id: String,
    pub(super) title: String,
}

#[derive(Debug, Deserialize)]
struct Observation {
    card_id: String,
    #[serde(default)]
    repo_id: Value,
    #[serde(default)]
    worktree_path: Value,
    #[serde(default)]
    branch: Value,
    #[serde(default)]
    pr_number: Value,
    #[serde(default)]
    head_sha: Value,
    #[serde(default)]
    state: Value,
    #[serde(default)]
    last_error: Value,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum Request {
    Upsert(Observation),
    ListStrandedCreatePr { limit: i64 },
    MarkHandedOff { card_id: String, error_msg: String },
    TouchGeneration { card_id: String, generation: String },
}

fn new_store<'js>(ctx: &Ctx<'js>, pool: Option<PgPool>) -> JsResult<Object<'js>> {
    let store = Object::new(ctx.clone())?;
    store.set(
        "__callRaw",
        Function::new(ctx.clone(), move |raw: String| {
            let result = (|| {
                let request: Request = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
                let pool = pool
                    .as_ref()
                    .ok_or("postgres backend is required for prTracking")?;
                let value = crate::utils::async_bridge::block_on_pg_result(
                    pool,
                    move |pool| async move {
                        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
                        let value = match request {
                            Request::Upsert(observation) => {
                                observe_on_pg_tx(&mut tx, &observation).await?;
                                Value::Null
                            }
                            Request::ListStrandedCreatePr { limit } => {
                                let rows = sqlx::query_scalar::<_, Value>(
                                    "SELECT jsonb_build_object('card_id', card_id, 'last_error', last_error,
                                         'retry_count', retry_count, 'dispatch_generation', dispatch_generation)
                                     FROM pr_tracking WHERE state = ANY($1) AND retry_count > 0
                                       AND COALESCE(last_error, '') NOT LIKE $2
                                       AND updated_at > NOW() - INTERVAL '30 days'
                                     ORDER BY updated_at LIMIT $3",
                                )
                                .bind(&CREATE_PR_STATES[..])
                                .bind(format!("{HANDED_OFF_PREFIX}%"))
                                .bind(limit)
                                .fetch_all(&mut *tx)
                                .await
                                .map_err(|e| e.to_string())?;
                                Value::Array(rows)
                            }
                            Request::MarkHandedOff { card_id, error_msg } => {
                                sqlx::query(
                                    "UPDATE pr_tracking SET last_error = $1, updated_at = NOW()
                                     WHERE card_id = $2 AND state = ANY($3)",
                                )
                                .bind(format!("{HANDED_OFF_PREFIX}{error_msg}"))
                                .bind(card_id)
                                .bind(&CREATE_PR_STATES[..])
                                .execute(&mut *tx)
                                .await
                                .map_err(|e| e.to_string())?;
                                Value::Null
                            }
                            Request::TouchGeneration {
                                card_id,
                                generation,
                            } => {
                                sqlx::query(
                                    "UPDATE pr_tracking SET updated_at = NOW()
                                     WHERE card_id = $1 AND dispatch_generation = $2",
                                )
                                .bind(card_id)
                                .bind(generation)
                                .execute(&mut *tx)
                                .await
                                .map_err(|e| e.to_string())?;
                                Value::Null
                            }
                        };
                        tx.commit().await.map_err(|e| e.to_string())?;
                        Ok(value)
                    },
                    |e| e,
                )?;
                Ok::<_, String>(json!({"value": value}))
            })();
            match result {
                Ok(value) => value.to_string(),
                Err(error) => json!({"error": error}).to_string(),
            }
        })?,
    )?;
    Ok(store)
}

pub(super) fn register_pr_tracking_ops<'js>(ctx: &Ctx<'js>, pool: Option<PgPool>) -> JsResult<()> {
    let ad: Object<'_> = ctx.globals().get("agentdesk")?;
    ad.set(
        "__newPrTrackingStoreRaw",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| {
            new_store(&ctx, pool.clone())
        })?,
    )?;
    ctx.eval::<(), _>(r#"
        agentdesk.prTrackingStore = {
          create: function() {
            var store = agentdesk.__newPrTrackingStoreRaw();
            function call(payload) {
                var result = JSON.parse(store.__callRaw(JSON.stringify(payload)));
                if (result.error) throw new Error(result.error);
                return result.value;
            }
            store.upsert = function(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
                call({op:'upsert', card_id:cardId, repo_id:repoId, worktree_path:worktreePath,
                      branch:branch, pr_number:prNumber, head_sha:headSha, state:state, last_error:lastError});
            };
            store.listStrandedCreatePr = function(limit) {
                return call({op:'listStrandedCreatePr', limit:limit});
            };
            store.markHandedOff = function(cardId, errorMsg) {
                call({op:'markHandedOff', card_id:cardId, error_msg:errorMsg});
            };
            store.touchGeneration = function(cardId, generation) {
                call({op:'touchGeneration', card_id:cardId, generation:generation});
            };
            return store;
          }
        };
    "#)?;
    Ok(())
}

fn wire_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        Value::Number(value) if value.as_i64().is_some() || value.as_u64().is_some() => {
            Some(value.to_string())
        }
        Value::Number(value) if value.as_f64().is_some_and(|n| n.fract() == 0.0) => {
            Some(format!("{:.0}", value.as_f64()?))
        }
        _ => Some(value.to_string()),
    }
}

// Observations refresh metadata without starting a new create-pr generation.
async fn observe_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    value: &Observation,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5::text::bigint, $6, $7, $8, NOW(), NOW())
         ON CONFLICT (card_id) DO UPDATE
         SET repo_id = COALESCE(EXCLUDED.repo_id, pr_tracking.repo_id),
             worktree_path = COALESCE(EXCLUDED.worktree_path, pr_tracking.worktree_path),
             branch = EXCLUDED.branch,
             pr_number = COALESCE(EXCLUDED.pr_number, pr_tracking.pr_number),
             head_sha = EXCLUDED.head_sha,
             state = EXCLUDED.state,
             last_error = EXCLUDED.last_error,
             updated_at = NOW()",
    )
    .bind(&value.card_id)
    .bind(wire_text(&value.repo_id))
    .bind(wire_text(&value.worktree_path))
    .bind(wire_text(&value.branch))
    .bind(wire_text(&value.pr_number))
    .bind(wire_text(&value.head_sha))
    .bind(wire_text(&value.state))
    .bind(wire_text(&value.last_error))
    .execute(&mut **tx).await.map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) async fn seed_pg_pr_tracking_handoff_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: &HandoffPayload,
    generation: &str,
    current_round: i64,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO pr_tracking (
            card_id,
            repo_id,
            worktree_path,
            branch,
            head_sha,
            state,
            last_error,
            dispatch_generation,
            review_round,
            retry_count,
            created_at,
            updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, 'create-pr', NULL, $6, $7, 0, NOW(), NOW()
         )
         ON CONFLICT (card_id) DO UPDATE
         SET repo_id = EXCLUDED.repo_id,
             worktree_path = EXCLUDED.worktree_path,
             branch = EXCLUDED.branch,
             head_sha = EXCLUDED.head_sha,
             state = 'create-pr',
             last_error = NULL,
             dispatch_generation = EXCLUDED.dispatch_generation,
             review_round = EXCLUDED.review_round,
             retry_count = 0,
             updated_at = NOW()",
    )
    .bind(&payload.card_id)
    .bind(&payload.repo_id)
    .bind(payload.worktree_path.as_deref())
    .bind(&payload.branch)
    .bind(payload.head_sha.as_deref())
    .bind(generation)
    .bind(current_round)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("upsert postgres pr_tracking for {}: {e}", payload.card_id))?;

    Ok(())
}

pub(super) async fn refresh_pg_pr_tracking_reuse_state_if_active(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: &HandoffPayload,
    dispatch_id: &str,
    generation: &str,
    current_round: i64,
) -> Result<bool, String> {
    let refreshed = sqlx::query(
        "WITH active_dispatch AS (
             SELECT id
             FROM task_dispatches
             WHERE id = $8
               AND status IN ('pending', 'dispatched')
             FOR UPDATE
         ),
         upsert_tracking AS (
             INSERT INTO pr_tracking (
                 card_id,
                 repo_id,
                 worktree_path,
                 branch,
                 head_sha,
                 state,
                 last_error,
                 dispatch_generation,
                 review_round,
                 retry_count,
                 created_at,
                 updated_at
             )
             SELECT
                 $1,
                 $2,
                 $3,
                 $4,
                 $5,
                 'create-pr',
                 NULL,
                 $6,
                 $7,
                 0,
                 NOW(),
                 NOW()
             FROM active_dispatch
             ON CONFLICT (card_id) DO UPDATE
             SET state = 'create-pr',
                 last_error = NULL,
                 dispatch_generation = EXCLUDED.dispatch_generation,
                 review_round = EXCLUDED.review_round,
                 retry_count = 0,
                 updated_at = NOW()
             RETURNING 1
         ),
         update_card AS (
             UPDATE kanban_cards
             SET blocked_reason = 'pr:creating',
                 updated_at = NOW()
             WHERE id = $1
               AND EXISTS (SELECT 1 FROM active_dispatch)
             RETURNING 1
         )
         SELECT
             EXISTS (SELECT 1 FROM active_dispatch) AS dispatch_active,
             EXISTS (SELECT 1 FROM upsert_tracking) AS tracking_updated,
             EXISTS (SELECT 1 FROM update_card) AS card_updated",
    )
    .bind(&payload.card_id)
    .bind(&payload.repo_id)
    .bind(payload.worktree_path.as_deref())
    .bind(&payload.branch)
    .bind(payload.head_sha.as_deref())
    .bind(generation)
    .bind(current_round)
    .bind(dispatch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| {
        format!(
            "refresh postgres reused pr_tracking for {}: {e}",
            payload.card_id
        )
    })?;

    let dispatch_active = refreshed
        .try_get::<bool, _>("dispatch_active")
        .map_err(|e| {
            format!(
                "decode postgres reuse dispatch_active for {}: {e}",
                payload.card_id
            )
        })?;
    let tracking_updated = refreshed
        .try_get::<bool, _>("tracking_updated")
        .map_err(|e| {
            format!(
                "decode postgres reuse tracking_updated for {}: {e}",
                payload.card_id
            )
        })?;
    let card_updated = refreshed.try_get::<bool, _>("card_updated").map_err(|e| {
        format!(
            "decode postgres reuse card_updated for {}: {e}",
            payload.card_id
        )
    })?;

    Ok(dispatch_active && tracking_updated && card_updated)
}

pub(super) async fn record_failure_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    error: &str,
    stamp_gen: &str,
) -> Result<Value, String> {
    if !stamp_gen.is_empty() {
        let current_gen = sqlx::query_scalar::<_, String>(
            "SELECT dispatch_generation
             FROM pr_tracking
             WHERE card_id = $1",
        )
        .bind(card_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| format!("load postgres pr_tracking generation for {card_id}: {e}"))?;
        if let Some(cur) = current_gen.as_deref()
            && !cur.is_empty()
            && cur != stamp_gen
        {
            return Ok(json!({
                "ok": true,
                "noop": true,
                "reason": "stale_generation",
            }));
        }
    }

    let updated = sqlx::query(
        "UPDATE pr_tracking
         SET retry_count = retry_count + 1,
             last_error = $1,
             updated_at = NOW()
         WHERE card_id = $2",
    )
    .bind(error)
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("update postgres pr_tracking failure for {card_id}: {e}"))?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id,
                state,
                last_error,
                dispatch_generation,
                review_round,
                retry_count,
                created_at,
                updated_at
             ) VALUES (
                $1, 'create-pr', $2, '', 0, 1, NOW(), NOW()
             )
             ON CONFLICT (card_id) DO UPDATE
             SET retry_count = pr_tracking.retry_count + 1,
                 last_error = EXCLUDED.last_error,
                 updated_at = NOW()",
        )
        .bind(card_id)
        .bind(error)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("insert postgres pr_tracking failure row for {card_id}: {e}"))?;
    }

    let retry_count = sqlx::query_scalar::<_, i64>(
        "SELECT retry_count::BIGINT
         FROM pr_tracking
         WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| format!("load postgres retry_count for {card_id}: {e}"))?;
    let escalated = retry_count >= 3;
    if escalated {
        sqlx::query(
            "UPDATE pr_tracking
             SET state = 'escalated',
                 updated_at = NOW()
             WHERE card_id = $1",
        )
        .bind(card_id)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("escalate postgres pr_tracking for {card_id}: {e}"))?;
    }

    Ok(json!({
        "ok": true,
        "retry_count": retry_count,
        "escalated": escalated,
    }))
}

pub(super) async fn reseed_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    generation: &str,
    current_round: i64,
    latest_head: Option<&str>,
) -> Result<(), String> {
    let updated = sqlx::query(
        "UPDATE pr_tracking
         SET dispatch_generation = $1,
             review_round = $2,
             head_sha = COALESCE($3, head_sha),
             state = 'create-pr',
             retry_count = 0,
             last_error = NULL,
             updated_at = NOW()
         WHERE card_id = $4",
    )
    .bind(generation)
    .bind(current_round)
    .bind(latest_head)
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|e| format!("update postgres reseeded pr_tracking for {card_id}: {e}"))?
    .rows_affected();
    if updated == 0 {
        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id,
                head_sha,
                state,
                dispatch_generation,
                review_round,
                retry_count,
                created_at,
                updated_at
             ) VALUES (
                $1, $2, 'create-pr', $3, $4, 0, NOW(), NOW()
             )",
        )
        .bind(card_id)
        .bind(latest_head)
        .bind(generation)
        .bind(current_round)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("insert postgres reseeded pr_tracking for {card_id}: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::wire_text;
    use serde_json::json;

    #[test]
    fn wire_text_preserves_integer_precision() {
        for value in [
            json!(9007199254740993_i64),
            json!(i64::MIN),
            json!(u64::MAX),
        ] {
            assert_eq!(wire_text(&value), Some(value.to_string()));
        }
        assert_eq!(wire_text(&json!(71.0)), Some("71".into()));
        assert_eq!(wire_text(&json!(71.5)), Some("71.5".into()));
        assert_eq!(wire_text(&json!(null)), None);
    }
}
