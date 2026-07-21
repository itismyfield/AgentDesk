use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;
use sqlx::Row;

use crate::app_state::AppState;

/// Minimum total outcomes required before generating any guidance.
/// Prevents misleading guidance from tiny sample sizes.
const MIN_OUTCOMES_FOR_GUIDANCE: i64 = 5;

/// Minimum outcomes per category before including it in guidance.
const MIN_CATEGORY_OUTCOMES: i64 = 3;

/// #119: Convenience wrapper — queries review state and records a tuning outcome.
/// Called from each decision branch (accept, dispute, dismiss) to avoid
/// relying on code after the match block that early-returning branches skip.
pub(super) async fn record_decision_tuning(
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
    decision: &str,
    dispatch_id: Option<&str>,
) -> Result<(), String> {
    let Some(pool) = pg_pool else {
        tracing::warn!(
            card_id,
            "[review-tuning] postgres pool unavailable; skipping tuning outcome record"
        );
        return Ok(());
    };
    record_decision_tuning_pg(pool, card_id, decision, dispatch_id).await
}

async fn record_decision_tuning_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    decision: &str,
    dispatch_id: Option<&str>,
) -> Result<(), String> {
    let (review_round, last_verdict, finding_cats) = match load_review_tuning_context_pg(
        pool, card_id,
    )
    .await
    {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(
                card_id,
                error = %error,
                "[review-tuning] failed to load postgres review tuning context; recording fallback outcome"
            );
            (None, None, None)
        }
    };

    let outcome = match decision {
        "accept" => "true_positive",
        "dismiss" => "false_positive",
        "dispute" => "disputed",
        _ => "unknown",
    };

    record_tuning_outcome_pg(
        pool,
        card_id,
        dispatch_id,
        review_round,
        last_verdict.as_deref().unwrap_or("unknown"),
        Some(decision),
        outcome,
        finding_cats.as_deref(),
    )
    .await
    .map_err(|error| format!("record postgres tuning outcome for {card_id}: {error}"))?;
    Ok(())
}

async fn load_review_tuning_context_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<(Option<i64>, Option<String>, Option<String>), String> {
    let review_state = sqlx::query(
        "SELECT review_round::BIGINT AS review_round, last_verdict
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review state: {error}"))?;

    let review_round = review_state
        .as_ref()
        .and_then(|row| row.try_get::<Option<i64>, _>("review_round").ok())
        .flatten();
    let last_verdict = review_state
        .as_ref()
        .and_then(|row| row.try_get::<Option<String>, _>("last_verdict").ok())
        .flatten();

    let finding_cats = sqlx::query(
        "SELECT result
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status = 'completed'
         ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST, created_at DESC NULLS LAST, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review dispatch result: {error}"))?
    .and_then(|row| row.try_get::<Option<String>, _>("result").ok())
    .flatten()
    .and_then(|raw| finding_categories_from_dispatch_result(&raw));

    Ok((review_round, last_verdict, finding_cats))
}

fn finding_categories_from_dispatch_result(result: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|value| {
            value["items"].as_array().map(|items| {
                let cats: Vec<String> = items
                    .iter()
                    .filter_map(|item| item["category"].as_str().map(|s| s.to_string()))
                    .collect();
                serde_json::to_string(&cats).unwrap_or_default()
            })
        })
}

/// #119: Record a review tuning outcome for FP/FN aggregation.
#[allow(clippy::too_many_arguments)]
async fn record_tuning_outcome_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    dispatch_id: Option<&str>,
    review_round: Option<i64>,
    verdict: &str,
    decision: Option<&str>,
    outcome: &str,
    finding_categories: Option<&str>,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO review_tuning_outcomes
         (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(card_id)
    .bind(dispatch_id)
    .bind(review_round)
    .bind(verdict)
    .bind(decision)
    .bind(outcome)
    .bind(finding_categories)
    .execute(pool)
    .await
    .map_err(|error| format!("insert review_tuning_outcomes: {error}"))?;

    tracing::info!(
        "[review-tuning] #119 recorded outcome: card={card_id} verdict={verdict} decision={} outcome={outcome}",
        decision.unwrap_or("none")
    );
    Ok(())
}

/// Spawn a background task to re-aggregate review tuning data.
/// Debounce: skips if the max outcome rowid hasn't changed since the last aggregation.
/// This avoids the old mtime-based debounce that could miss outcomes inserted
/// shortly after the previous aggregate (e.g. a 5th sample crossing the threshold
/// 10s after a 4-sample aggregate).
pub fn spawn_aggregate_if_needed_with_pg(pg_pool: Option<sqlx::PgPool>) {
    let Some(pool) = pg_pool else {
        return;
    };
    let aggregate = async move {
        let max_outcome_id = sqlx::query(
            "SELECT COALESCE(MAX(id), 0)::BIGINT AS max_outcome_id
             FROM review_tuning_outcomes",
        )
        .fetch_one(&pool)
        .await
        .ok()
        .and_then(|row| row.try_get::<i64, _>("max_outcome_id").ok())
        .unwrap_or(0);

        let last_aggregated_outcome_id = sqlx::query(
            "SELECT value
             FROM kv_meta
             WHERE key = $1
             LIMIT 1",
        )
        .bind("review_tuning_last_aggregated_rowid")
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<Option<String>, _>("value").ok())
        .flatten()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);

        if max_outcome_id <= last_aggregated_outcome_id {
            return;
        }

        let _ = aggregate_review_tuning_core_pg(&pool).await;
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(aggregate);
        return;
    }

    let _ = std::thread::Builder::new()
        .name("review-tuning-aggregate".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "[review-tuning] failed to build aggregate runtime"
                    );
                    return;
                }
            };
            runtime.block_on(aggregate);
        });
}

async fn aggregate_review_tuning_core_pg(pool: &sqlx::PgPool) -> (i64, i64, i64, i64, i64, usize) {
    let snapshot_max_outcome_id = sqlx::query(
        "SELECT COALESCE(MAX(id), 0)::BIGINT AS max_outcome_id
         FROM review_tuning_outcomes",
    )
    .fetch_one(pool)
    .await
    .ok()
    .and_then(|row| row.try_get::<i64, _>("max_outcome_id").ok())
    .unwrap_or(0);

    let rows = match sqlx::query(
        "SELECT outcome, finding_categories
         FROM review_tuning_outcomes
         WHERE created_at > NOW() - INTERVAL '30 days'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|row| {
                (
                    row.try_get::<String, _>("outcome").unwrap_or_default(),
                    row.try_get::<Option<String>, _>("finding_categories")
                        .ok()
                        .flatten(),
                )
            })
            .collect::<Vec<_>>(),
        Err(_) => return (0, 0, 0, 0, 0, 0),
    };

    let (total_tp, total_fp, total_tn, total_fn, total_disputed, guidance_lines) =
        summarize_review_tuning_rows(&rows);
    let guidance = if guidance_lines.is_empty() {
        String::new()
    } else {
        guidance_lines.join("\n")
    };

    persist_review_tuning_guidance_pg(pool, &guidance, snapshot_max_outcome_id).await;
    write_review_tuning_guidance_file(&guidance);

    let lines = guidance_lines.len();
    tracing::info!(
        "[review-tuning] #119 aggregation: tp={total_tp} fp={total_fp} tn={total_tn} fn={total_fn} disputed={total_disputed}, {lines} guidance lines → {}",
        review_tuning_guidance_path().display()
    );

    (
        total_tp,
        total_fp,
        total_tn,
        total_fn,
        total_disputed,
        lines,
    )
}

fn summarize_review_tuning_rows(
    rows: &[(String, Option<String>)],
) -> (i64, i64, i64, i64, i64, Vec<String>) {
    let mut total_tp = 0i64;
    let mut total_fp = 0i64;
    let mut total_tn = 0i64;
    let mut total_fn = 0i64;
    let mut total_disputed = 0i64;
    let mut fp_categories: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut tp_categories: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut fn_categories: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();

    for (outcome, cats_json) in rows {
        match outcome.as_str() {
            "true_positive" => total_tp += 1,
            "false_positive" => total_fp += 1,
            "true_negative" => total_tn += 1,
            "false_negative" => total_fn += 1,
            "disputed" => total_disputed += 1,
            _ => {}
        }
        if let Some(cats) = cats_json {
            if let Ok(cats_arr) = serde_json::from_str::<Vec<String>>(cats) {
                let target = match outcome.as_str() {
                    "false_positive" => Some(&mut fp_categories),
                    "true_positive" => Some(&mut tp_categories),
                    "false_negative" => Some(&mut fn_categories),
                    _ => None,
                };
                if let Some(map) = target {
                    for cat in cats_arr {
                        *map.entry(cat).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    let total = total_tp + total_fp + total_tn + total_fn + total_disputed;
    let mut guidance_lines: Vec<String> = Vec::new();
    if total >= MIN_OUTCOMES_FOR_GUIDANCE {
        let actionable = total_tp + total_fp;
        let fp_rate = if actionable > 0 {
            total_fp as f64 / actionable as f64
        } else {
            0.0
        };

        guidance_lines.push(format!(
            "지난 30일 리뷰 통계: 전체 {}건 (정탐 {}건, 오탐 {}건, 정상 {}건, 미탐 {}건, 반박 {}건, 오탐률 {:.0}%)",
            total, total_tp, total_fp, total_tn, total_fn, total_disputed, fp_rate * 100.0
        ));

        let mut fp_sorted: Vec<_> = fp_categories.iter().collect();
        fp_sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (cat, count) in fp_sorted.iter().take(5) {
            let tp_count = tp_categories.get(*cat).copied().unwrap_or(0);
            let cat_total = *count + tp_count;
            if cat_total >= MIN_CATEGORY_OUTCOMES && **count as f64 / cat_total as f64 > 0.5 {
                guidance_lines.push(format!(
                    "- 과도 지적 카테고리 '{}': 오탐 {}건/전체 {}건 — 이 유형은 엄격도를 낮춰라",
                    cat, count, cat_total
                ));
            }
        }

        let mut tp_sorted: Vec<_> = tp_categories.iter().collect();
        tp_sorted.sort_by(|a, b| b.1.cmp(a.1));
        for (cat, count) in tp_sorted.iter().take(3) {
            let fp_count = fp_categories.get(*cat).copied().unwrap_or(0);
            let cat_total = *count + fp_count;
            if cat_total >= MIN_CATEGORY_OUTCOMES && **count as f64 / cat_total as f64 > 0.7 {
                guidance_lines.push(format!(
                    "- 정탐 빈출 카테고리 '{}': 정탐 {}건/전체 {}건 — 이 유형은 계속 주의 깊게 확인하라",
                    cat, count, cat_total
                ));
            }
        }

        if total_fn > 0 {
            let mut fn_sorted: Vec<_> = fn_categories.iter().collect();
            fn_sorted.sort_by(|a, b| b.1.cmp(a.1));
            for (cat, count) in fn_sorted.iter().take(3) {
                guidance_lines.push(format!(
                    "- 미탐 카테고리 '{}': {}건 — 이 패턴은 리뷰에서 놓쳤다, 반드시 확인하라",
                    cat, count
                ));
            }
        }
    }

    (
        total_tp,
        total_fp,
        total_tn,
        total_fn,
        total_disputed,
        guidance_lines,
    )
}

async fn persist_review_tuning_guidance_pg(
    pool: &sqlx::PgPool,
    guidance: &str,
    snapshot_max_outcome_id: i64,
) {
    let _ = sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("review_tuning_guidance")
    .bind(guidance)
    .execute(pool)
    .await;

    let _ = sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("review_tuning_last_aggregated_rowid")
    .bind(snapshot_max_outcome_id.to_string())
    .execute(pool)
    .await;
}

fn write_review_tuning_guidance_file(guidance: &str) {
    let guidance_path = review_tuning_guidance_path();
    if let Some(parent) = guidance_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&guidance_path, guidance);
}

/// POST /api/reviews/tuning/aggregate
///
/// Aggregates review tuning outcomes (FP/FN rates per finding category)
/// and writes tuning guidance to kv_meta + a file for prompt injection.
pub async fn aggregate_review_tuning(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pg_pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool not configured"})),
        );
    };
    let (total_tp, total_fp, total_tn, total_fn, total_disputed, guidance_lines) =
        aggregate_review_tuning_core_pg(pg_pool).await;
    let total = total_tp + total_fp + total_tn + total_fn + total_disputed;
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "total": total,
            "true_positive": total_tp,
            "false_positive": total_fp,
            "true_negative": total_tn,
            "false_negative": total_fn,
            "disputed": total_disputed,
            "guidance_lines": guidance_lines,
        })),
    )
}

/// Well-known path for review tuning guidance file.
pub fn review_tuning_guidance_path() -> std::path::PathBuf {
    let root = crate::config::runtime_root().unwrap_or_else(|| std::path::PathBuf::from("."));
    root.join("runtime").join("review-tuning-guidance.txt")
}
