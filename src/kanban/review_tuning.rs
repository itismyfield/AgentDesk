//! Review tuning outcome recording for kanban lifecycle events.

use crate::db::Db;
use sqlx::Row as SqlxRow;

/// #119: When a card reaches done after a review pass verdict, record a true_negative
/// tuning outcome. This confirms the review was correct in not finding issues.
/// Returns true if a TN was actually inserted.
// reason: review-tuning TN recorder (#119) called from hooks; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub(super) fn record_true_negative_if_pass(
    db: &Db,
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
) -> bool {
    record_true_negative_if_pass_with_backends(Some(db), pg_pool, card_id)
}

pub(super) fn record_true_negative_if_pass_with_backends(
    _db: Option<&Db>,
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
) -> bool {
    if let Some(pool) = pg_pool {
        let card_id = card_id.to_string();
        return crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                let last_verdict = sqlx::query_scalar::<_, Option<String>>(
                    "SELECT last_verdict
                     FROM card_review_state
                     WHERE card_id = $1",
                )
                .bind(&card_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres review verdict for {card_id}: {error}"))?
                .flatten();

                let Some(last_verdict) = last_verdict else {
                    return Ok(false);
                };
                if !matches!(last_verdict.as_str(), "pass" | "approved") {
                    return Ok(false);
                }

                // `card_review_state.review_round` is BIGINT (0008_int4_to_bigint_audit.sql).
                // Decoding as `i32` raises `ColumnDecode: mismatched types`, which silently
                // aborted this whole true_negative recording path.
                let review_round = sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT review_round
                     FROM card_review_state
                     WHERE card_id = $1",
                )
                .bind(&card_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres review round for {card_id}: {error}"))?
                .flatten();
                // `review_tuning_outcomes.review_round` is still INTEGER (not in the
                // 0008 bigint audit). Downcast is safe: review rounds are bounded small.
                let review_round_i32 = review_round.map(|v| v as i32);

                let review_results = sqlx::query(
                    "SELECT result
                     FROM task_dispatches
                     WHERE kanban_card_id = $1
                       AND dispatch_type = 'review'
                       AND status = 'completed'
                     ORDER BY COALESCE(completed_at, updated_at, created_at) DESC, id DESC",
                )
                .bind(&card_id)
                .fetch_all(&pool)
                .await
                .map_err(|error| format!("load postgres review dispatches for {card_id}: {error}"))?;

                let finding_cats = review_results.into_iter().find_map(|row| {
                    row.try_get::<Option<String>, _>("result")
                        .ok()
                        .flatten()
                        .and_then(|result_str| serde_json::from_str::<serde_json::Value>(&result_str).ok())
                        .and_then(|value| {
                            value["items"].as_array().and_then(|items| {
                                let cats: Vec<String> = items
                                    .iter()
                                    .filter_map(|item| item["category"].as_str().map(str::to_string))
                                    .collect();
                                if cats.is_empty() {
                                    None
                                } else {
                                    serde_json::to_string(&cats).ok()
                                }
                            })
                        })
                });

                let inserted = sqlx::query(
                    "INSERT INTO review_tuning_outcomes (
                        card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories
                     )
                     VALUES ($1, NULL, $2, $3, 'done', 'true_negative', $4)",
                )
                .bind(&card_id)
                .bind(review_round_i32)
                .bind(&last_verdict)
                .bind(finding_cats)
                .execute(&pool)
                .await
                .map(|result| result.rows_affected() > 0)
                .map_err(|error| {
                    format!("insert postgres true_negative review tuning for {card_id}: {error}")
                })?;

                if inserted {
                    tracing::info!(
                        "[review-tuning] #119 recorded true_negative: card={card_id} (pass -> done)"
                    );
                }
                Ok(inserted)
            },
            |error| error,
        )
        .unwrap_or(false);
    }

    false
}

/// #119: When a card is reopened after reaching done with a pass verdict,
/// correct any true_negative outcomes to false_negative: the review missed a real bug.
///
/// Also backfills finding_categories if the TN record had empty categories.
/// TN is typically recorded using categories from the last completed review dispatch,
/// which is the pass/approved dispatch with empty items. On reopen we look for the
/// most recent review dispatch that actually reported findings (non-empty items array)
/// to carry those categories forward into the FN record.
pub fn correct_tn_to_fn_on_reopen(_db: Option<&Db>, pg_pool: Option<&sqlx::PgPool>, card_id: &str) {
    if let Some(pool) = pg_pool {
        let card_id = card_id.to_string();
        let log_card_id = card_id.clone();
        let updated = crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                let updated = sqlx::query(
                    "UPDATE review_tuning_outcomes
                     SET outcome = 'false_negative'
                     WHERE card_id = $1
                       AND outcome = 'true_negative'
                       AND review_round = (
                           SELECT MAX(review_round)
                           FROM review_tuning_outcomes
                           WHERE card_id = $1
                             AND outcome = 'true_negative'
                       )",
                )
                .bind(&card_id)
                .execute(&pool)
                .await
                .map_err(|error| format!("correct postgres TN->FN for {card_id}: {error}"))?
                .rows_affected();
                if updated == 0 {
                    return Ok(0_u64);
                }

                let needs_backfill = sqlx::query_scalar::<_, bool>(
                    "SELECT COALESCE(
                         finding_categories IS NULL
                         OR finding_categories = ''
                         OR finding_categories = '[]',
                         false
                     )
                     FROM review_tuning_outcomes
                     WHERE card_id = $1
                       AND outcome = 'false_negative'
                     ORDER BY id DESC
                     LIMIT 1",
                )
                .bind(&card_id)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load postgres FN backfill flag for {card_id}: {error}"))?
                .unwrap_or(false);

                if needs_backfill {
                    let review_results = sqlx::query(
                        "SELECT result
                         FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type = 'review'
                           AND status = 'completed'
                         ORDER BY COALESCE(completed_at, updated_at, created_at) DESC, id DESC",
                    )
                    .bind(&card_id)
                    .fetch_all(&pool)
                    .await
                    .map_err(|error| format!("load postgres review dispatches for {card_id}: {error}"))?;

                    let finding_cats = review_results.into_iter().find_map(|row| {
                        row.try_get::<Option<String>, _>("result")
                            .ok()
                            .flatten()
                            .and_then(|result_str| serde_json::from_str::<serde_json::Value>(&result_str).ok())
                            .and_then(|value| {
                                value["items"].as_array().and_then(|items| {
                                    if items.is_empty() {
                                        return None;
                                    }
                                    let cats: Vec<String> = items
                                        .iter()
                                        .filter_map(|item| item["category"].as_str().map(str::to_string))
                                        .collect();
                                    if cats.is_empty() {
                                        None
                                    } else {
                                        serde_json::to_string(&cats).ok()
                                    }
                                })
                            })
                    });

                    if let Some(cats) = finding_cats {
                        let backfilled = sqlx::query(
                            "UPDATE review_tuning_outcomes
                             SET finding_categories = $1
                             WHERE card_id = $2
                               AND outcome = 'false_negative'
                               AND (finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]')",
                        )
                        .bind(&cats)
                        .bind(&card_id)
                        .execute(&pool)
                        .await
                        .map_err(|error| {
                            format!("backfill postgres FN finding_categories for {card_id}: {error}")
                        })?
                        .rows_affected();
                        if backfilled > 0 {
                            tracing::info!(
                                "[review-tuning] #119 backfilled {backfilled} FN finding_categories: card={card_id} categories={cats}"
                            );
                        }
                    }
                }

                Ok(updated)
            },
            |error| error,
        )
        .unwrap_or(0);
        if updated > 0 {
            tracing::info!(
                "[review-tuning] #119 corrected {updated} true_negative -> false_negative: card={log_card_id} (reopen, latest round only)"
            );
        }
        return;
    }
}
