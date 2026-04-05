use axum::{Json, extract::State, http::StatusCode};
use serde_json::json;

use super::super::AppState;

/// Minimum total outcomes required before generating any guidance.
/// Prevents misleading guidance from tiny sample sizes.
const MIN_OUTCOMES_FOR_GUIDANCE: i64 = 5;

/// Minimum outcomes per category before including it in guidance.
const MIN_CATEGORY_OUTCOMES: i64 = 3;

/// #119: Convenience wrapper — queries review state and records a tuning outcome.
/// Called from each decision branch (accept, dispute, dismiss) to avoid
/// relying on code after the match block that early-returning branches skip.
pub(super) fn record_decision_tuning(
    db: &crate::db::Db,
    card_id: &str,
    decision: &str,
    dispatch_id: Option<&str>,
) {
    let (review_round, last_verdict, finding_cats) = db
        .lock()
        .ok()
        .map(|conn| {
            let round: Option<i64> = conn
                .query_row(
                    "SELECT review_round FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok();
            let verdict: Option<String> = conn
                .query_row(
                    "SELECT last_verdict FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            let cats: Option<String> = conn
                .query_row(
                    "SELECT td.result FROM task_dispatches td \
                     WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                     AND td.status = 'completed' ORDER BY td.rowid DESC LIMIT 1",
                    [card_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .and_then(|r| {
                    serde_json::from_str::<serde_json::Value>(&r)
                        .ok()
                        .and_then(|v| {
                            v["items"].as_array().map(|items| {
                                let cats: Vec<String> = items
                                    .iter()
                                    .filter_map(|it| it["category"].as_str().map(|s| s.to_string()))
                                    .collect();
                                serde_json::to_string(&cats).unwrap_or_default()
                            })
                        })
                });
            (round, verdict, cats)
        })
        .unwrap_or((None, None, None));

    let outcome = match decision {
        "accept" => "true_positive",
        "dismiss" => "false_positive",
        "dispute" => "disputed",
        _ => "unknown",
    };
    record_tuning_outcome(
        db,
        card_id,
        dispatch_id,
        review_round,
        last_verdict.as_deref().unwrap_or("unknown"),
        Some(decision),
        outcome,
        finding_cats.as_deref(),
    );
}

/// #119: Record a review tuning outcome for FP/FN aggregation.
fn record_tuning_outcome(
    db: &crate::db::Db,
    card_id: &str,
    dispatch_id: Option<&str>,
    review_round: Option<i64>,
    verdict: &str,
    decision: Option<&str>,
    outcome: &str,
    finding_categories: Option<&str>,
) {
    if let Ok(conn) = db.lock() {
        conn.execute(
            "INSERT INTO review_tuning_outcomes \
             (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                card_id,
                dispatch_id,
                review_round,
                verdict,
                decision,
                outcome,
                finding_categories,
            ],
        )
        .ok();
        tracing::info!(
            "[review-tuning] #119 recorded outcome: card={card_id} verdict={verdict} decision={} outcome={outcome}",
            decision.unwrap_or("none")
        );
    }
}

/// Spawn a background task to re-aggregate review tuning data.
/// Debounce: skips if the max outcome rowid hasn't changed since the last aggregation.
/// This avoids the old mtime-based debounce that could miss outcomes inserted
/// shortly after the previous aggregate (e.g. a 5th sample crossing the threshold
/// 10s after a 4-sample aggregate).
pub fn spawn_aggregate_if_needed(db: &crate::db::Db) {
    let db = db.clone();
    tokio::spawn(async move {
        // Debounce: compare latest outcome rowid against last aggregated rowid
        if let Ok(conn) = db.lock() {
            let max_rowid: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(rowid), 0) FROM review_tuning_outcomes",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let last_aggregated_rowid: i64 = conn
                .query_row(
                    "SELECT CAST(COALESCE(value, '0') AS INTEGER) FROM kv_meta WHERE key = 'review_tuning_last_aggregated_rowid'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if max_rowid <= last_aggregated_rowid {
                return; // no new outcomes since last aggregation, skip
            }
        }
        aggregate_review_tuning_core(&db);
    });
}

/// Core aggregation logic shared by the HTTP endpoint and background trigger.
fn aggregate_review_tuning_core(db: &crate::db::Db) -> (i64, i64, i64, i64, i64, usize) {
    let conn = match db.lock() {
        Ok(c) => c,
        Err(_) => return (0, 0, 0, 0, 0, 0),
    };

    // Snapshot the current max rowid BEFORE reading outcomes.
    // This is stored in kv_meta after aggregation for rowid-based debounce.
    let snapshot_max_rowid: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM review_tuning_outcomes",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

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

    {
        let mut stmt = match conn.prepare(
            "SELECT outcome, finding_categories \
             FROM review_tuning_outcomes \
             WHERE created_at > datetime('now', '-30 days')",
        ) {
            Ok(s) => s,
            Err(_) => return (0, 0, 0, 0, 0, 0),
        };

        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .ok()
            .into_iter()
            .flat_map(|r| r.flatten())
            .collect();

        for (outcome, cats_json) in &rows {
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
    }

    let total = total_tp + total_fp + total_tn + total_fn + total_disputed;
    let mut guidance_lines: Vec<String> = Vec::new();

    // Only generate guidance when we have enough data to be meaningful
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

        // High FP categories (min sample guard)
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

        // High TP categories (min sample guard)
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

        // FN categories — patterns the reviewer missed (reopen after pass)
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

    let guidance = if guidance_lines.is_empty() {
        String::new()
    } else {
        guidance_lines.join("\n")
    };

    // Store in kv_meta
    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('review_tuning_guidance', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [&guidance],
    )
    .ok();

    // Write to file for prompt_builder to read
    let guidance_path = review_tuning_guidance_path();
    if let Some(parent) = guidance_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&guidance_path, &guidance);

    // #119: Store the snapshot rowid so the debounce check in spawn_aggregate_if_needed
    // can skip re-aggregation until new outcomes arrive.
    conn.execute(
        "INSERT INTO kv_meta (key, value) VALUES ('review_tuning_last_aggregated_rowid', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [&snapshot_max_rowid.to_string()],
    )
    .ok();

    let lines = guidance_lines.len();
    tracing::info!(
        "[review-tuning] #119 aggregation: tp={total_tp} fp={total_fp} tn={total_tn} fn={total_fn} disputed={total_disputed}, {lines} guidance lines → {}",
        guidance_path.display()
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

/// POST /api/review-tuning/aggregate
///
/// Aggregates review tuning outcomes (FP/FN rates per finding category)
/// and writes tuning guidance to kv_meta + a file for prompt injection.
pub async fn aggregate_review_tuning(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (total_tp, total_fp, total_tn, total_fn, total_disputed, guidance_lines) =
        aggregate_review_tuning_core(&state.db);
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
