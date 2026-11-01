//! Owns PR tracking merges and legacy imports; lifecycle callers supply their transaction.

use rquickjs::{Ctx, Function, Object, Result as JsResult, function::MutFn};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

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
    ImportLegacyForCard {
        card_id: String,
        fallback_state: Option<String>,
    },
    ImportLegacyOnce {
        fallback_state: Option<String>,
    },
}

fn new_store<'js>(ctx: &Ctx<'js>, pool: Option<PgPool>) -> JsResult<Object<'js>> {
    let store = Object::new(ctx.clone())?;
    let mut scan_stamp: Option<String> = None;
    store.set(
        "__callRaw",
        Function::new(
            ctx.clone(),
            MutFn::from(move |raw: String| {
                let result = (|| {
                    let request: Request = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
                    let pool = pool
                        .as_ref()
                        .ok_or("postgres backend is required for prTracking")?;
                    let previous = scan_stamp.clone();
                    let (value, next_stamp) = crate::utils::async_bridge::block_on_pg_result(
                        pool,
                        move |pool| async move {
                            match request {
                                Request::Upsert(observation) => {
                                    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
                                    observe_on_pg_tx(&mut tx, &observation).await?;
                                    tx.commit().await.map_err(|e| e.to_string())?;
                                    Ok((Value::Null, previous))
                                }
                                Request::ImportLegacyForCard {
                                    card_id,
                                    fallback_state,
                                } => {
                                    let value =
                                        import_legacy(&pool, &card_id, fallback_state.as_deref())
                                            .await?;
                                    Ok((value, previous))
                                }
                                Request::ImportLegacyOnce { fallback_state } => {
                                    let next = import_legacy_once(
                                        &pool,
                                        previous,
                                        fallback_state.as_deref(),
                                    )
                                    .await?;
                                    Ok((Value::Null, Some(next)))
                                }
                            }
                        },
                        |e| e,
                    )?;
                    scan_stamp = next_stamp;
                    Ok::<_, String>(json!({"value": value}))
                })();
                match result {
                    Ok(value) => value.to_string(),
                    Err(error) => json!({"error": error}).to_string(),
                }
            }),
        )?,
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
            store.importLegacyForCard = function(cardId, fallbackState) {
                return call({op:'importLegacyForCard', card_id:cardId, fallback_state:fallbackState || null});
            };
            store.importLegacyOnce = function(fallbackState) {
                call({op:'importLegacyOnce', fallback_state:fallbackState || null});
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

async fn canonical(pool: &PgPool, card_id: &str) -> Result<Option<Value>, String> {
    sqlx::query_scalar(
        "SELECT to_jsonb(p) FROM (
            SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error,
                   dispatch_generation, review_round, retry_count
            FROM pr_tracking WHERE card_id = $1
         ) p",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|n| n != 0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}

fn first(values: &[&Value]) -> Value {
    values
        .iter()
        .find(|value| truthy(value))
        .map_or(Value::Null, |value| (*value).clone())
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Object(_) => "[object Object]".to_string(),
        Value::Array(values) => values
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => wire_text(value).unwrap_or_else(|| "null".to_string()),
    }
}

async fn import_legacy(
    pool: &PgPool,
    card_id: &str,
    fallback: Option<&str>,
) -> Result<Value, String> {
    if let Some(existing) = canonical(pool, card_id).await? {
        return Ok(existing);
    }
    let cached: Option<String> = sqlx::query_scalar(
        "SELECT value FROM kv_meta WHERE key = $1 AND (expires_at IS NULL OR expires_at > NOW())",
    )
    .bind(format!("pr:{card_id}"))
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?
    .flatten();
    let info: Value = cached
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(Value::Null);
    if ![
        &info["number"],
        &info["pr_number"],
        &info["branch"],
        &info["headRefName"],
    ]
    .iter()
    .any(|v| truthy(v))
    {
        return Ok(Value::Null);
    }
    let card = sqlx::query(
        "SELECT blocked_reason, repo_id, github_issue_url FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    let (blocked, card_repo, url): (Option<String>, Option<String>, Option<String>) = match card {
        Some(card) => (
            card.try_get("blocked_reason").map_err(|e| e.to_string())?,
            card.try_get("repo_id").map_err(|e| e.to_string())?,
            card.try_get("github_issue_url")
                .map_err(|e| e.to_string())?,
        ),
        None => (None, None, None),
    };
    let repo_pattern =
        regex::Regex::new(r"github\.com/([^/]+/[^/]+)").map_err(|e| e.to_string())?;
    let url_repo = url
        .as_deref()
        .and_then(|url| repo_pattern.captures(url))
        .map(|captures| captures[1].to_string());
    let number = first(&[&info["number"], &info["pr_number"]]);
    let state = if truthy(&info["state"]) {
        js_string(&info["state"])
    } else if let Some(fallback) = fallback.filter(|s| !s.is_empty()) {
        fallback.to_string()
    } else if blocked.as_deref().is_some_and(|s| s.starts_with("ci:")) {
        "wait-ci".to_string()
    } else if truthy(&number) {
        "merge".to_string()
    } else {
        "create-pr".to_string()
    };
    let observation = Observation {
        card_id: card_id.to_string(),
        repo_id: first(&[&info["repo"], &json!(card_repo), &json!(url_repo)]),
        worktree_path: Value::Null,
        branch: first(&[&info["branch"], &info["headRefName"]]),
        pr_number: number,
        head_sha: first(&[&info["sha"], &info["head_sha"]]),
        state: json!(state),
        last_error: Value::Null,
    };
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    observe_on_pg_tx(&mut tx, &observation).await?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(canonical(pool, card_id).await?.unwrap_or(Value::Null))
}

async fn import_legacy_once(
    pool: &PgPool,
    previous: Option<String>,
    fallback: Option<&str>,
) -> Result<String, String> {
    let stamp: (i64, String, String) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(MIN(key), ''), COALESCE(MAX(key), '') FROM kv_meta
         WHERE key LIKE 'pr:%' AND (expires_at IS NULL OR expires_at > NOW())",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;
    let stamp = format!("{}:{}:{}", stamp.0, stamp.1, stamp.2);
    if previous.as_ref() == Some(&stamp) {
        return Ok(stamp);
    }
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT key FROM kv_meta WHERE key LIKE 'pr:%' AND (expires_at IS NULL OR expires_at > NOW())",
    ).fetch_all(pool).await.map_err(|e| e.to_string())?;
    for key in keys {
        let card_id = key.strip_prefix("pr:").unwrap_or(&key);
        if !card_id.is_empty() && canonical(pool, card_id).await?.is_none() {
            import_legacy(pool, card_id, fallback).await?;
        }
    }
    Ok(stamp)
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
