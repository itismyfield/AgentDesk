//! Central kanban state machine.
//!
//! ALL card status transitions MUST go through the Postgres transition helpers.
//! This ensures hooks fire, auto-queue syncs, and notifications are sent.
//!
//! ## Pipeline-Driven Transitions (#106 P5)
//!
//! All transition rules, gates, hooks, clocks, and timeouts are defined in
//! `policies/default-pipeline.yaml`. No hardcoded state names exist in this module.
//! See the YAML file for the complete state machine specification.
//!
//! Custom pipelines can override the default via repo or agent-level overrides
//! (3-level inheritance: default → repo → agent).

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::terminal_cleanup::{TERMINAL_DISPATCH_CLEANUP_REASON, sync_terminal_card_state};
use crate::db::Db;
use anyhow::Result;
use sqlx::Row as SqlxRow;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn log_audit_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    log_audit(conn, card_id, from, to, source, result);
}

pub(crate) async fn resolve_pipeline_with_pg(
    pg_pool: &sqlx::PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig> {
    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM github_repos
             WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| anyhow::anyhow!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM agents
             WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("load agent pipeline override for {agent_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| anyhow::anyhow!("parse agent pipeline override for {agent_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}

pub(super) async fn github_sync_on_transition_pg(
    pg_pool: &sqlx::PgPool,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let Some((repo_id, issue_number)) = github_sync_target_for_card_pg(pg_pool, card_id).await
    else {
        return;
    };

    if is_terminal {
        if let Err(error) = crate::github::close_issue(&repo_id, issue_number) {
            tracing::warn!(
                "[kanban] failed to close issue {repo_id}#{issue_number} for terminal card {card_id}: {error}"
            );
        }
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::comment_issue(&repo_id, issue_number, comment);
    }
}

async fn github_sync_target_for_card_pg(
    pg_pool: &sqlx::PgPool,
    card_id: &str,
) -> Option<(String, i64)> {
    let row = sqlx::query(
        "SELECT
            COALESCE(repo_id, '') AS repo_id,
            COALESCE(github_issue_url, '') AS github_issue_url,
            github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pg_pool)
    .await
    .ok()??;

    let repo_id: String = row.try_get("repo_id").ok()?;
    let issue_url: String = row.try_get("github_issue_url").ok()?;
    let issue_number: Option<i64> = row.try_get("github_issue_number").ok()?;
    if repo_id.is_empty() || issue_url.is_empty() {
        return None;
    }

    let issue_repo = issue_url
        .strip_prefix("https://github.com/")
        .and_then(|value| value.find("/issues/").map(|index| &value[..index]))?;
    if issue_repo != repo_id {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: issue URL repo {issue_repo} does not match card repo_id {repo_id}"
        );
        return None;
    }

    let repo_registered = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM github_repos
            WHERE id = $1
              AND COALESCE(sync_enabled, TRUE) = TRUE
         )",
    )
    .bind(&repo_id)
    .fetch_one(pg_pool)
    .await
    .ok()
    .unwrap_or(false);
    if !repo_registered {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: repo_id {repo_id} is not a registered sync-enabled repo"
        );
        return None;
    }

    issue_number.map(|number| (repo_id, number))
}

/// Sync GitHub issue state when kanban card transitions (pipeline-driven).
/// Terminal states → close issue. States with OnReviewEnter hook → comment.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn github_sync_on_transition(
    db: &Db,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let Some((repo_id, num)) = github_sync_target_for_card(db, card_id) else {
        return;
    };

    if is_terminal {
        if let Err(error) = crate::github::close_issue(&repo_id, num) {
            tracing::warn!(
                "[kanban] failed to close issue {repo_id}#{num} for terminal card {card_id}: {error}"
            );
        }
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::comment_issue(&repo_id, num, comment);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn github_sync_target_for_card(db: &Db, card_id: &str) -> Option<(String, i64)> {
    let info: Option<(String, String, Option<i64>)> = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(repo_id, ''), COALESCE(github_issue_url, ''), github_issue_number FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()
        });

    let Some((repo_id, issue_url, issue_number)) = info else {
        return None;
    };
    if repo_id.is_empty() || issue_url.is_empty() {
        return None;
    }

    let issue_repo = match issue_url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.find("/issues/").map(|i| &s[..i]))
    {
        Some(r) => r,
        None => return None,
    };
    if issue_repo != repo_id {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: issue URL repo {issue_repo} does not match card repo_id {repo_id}"
        );
        return None;
    }

    let repo_registered = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM github_repos WHERE id = ?1 AND COALESCE(sync_enabled, 1) = 1)",
                [&repo_id],
                |row| row.get::<_, bool>(0),
            )
            .ok()
        })
        .unwrap_or(false);
    if !repo_registered {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: repo_id {repo_id} is not a registered sync-enabled repo"
        );
        return None;
    }

    issue_number.map(|num| (repo_id, num))
}

/// Log a kanban state transition to audit_logs table.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn log_audit(
    conn: &sqlite_test::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kanban_audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT,
            from_status TEXT,
            to_status TEXT,
            source TEXT,
            result TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) VALUES (?1, ?2, ?3, ?4, ?5)",
        sqlite_test::params![card_id, from, to, source, result],
    )
    .ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT,
            entity_id   TEXT,
            action      TEXT,
            timestamp   DATETIME DEFAULT CURRENT_TIMESTAMP,
            actor       TEXT
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('kanban_card', ?1, ?2, ?3)",
        sqlite_test::params![card_id, format!("{from}->{to} ({result})"), source],
    )
    .ok();
}

/// #119: When a card reaches done after a review pass verdict, record a true_negative
/// tuning outcome. This confirms the review was correct in not finding issues.
/// Returns true if a TN was actually inserted.
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
                // 0008 bigint audit). Downcast is safe — review rounds are bounded small.
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
                        "[review-tuning] #119 recorded true_negative: card={card_id} (pass → done)"
                    );
                }
                Ok(inserted)
            },
            |error| error,
        )
        .unwrap_or(false);
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        if let Some(db) = _db
            && let Ok(conn) = db.lock()
        {
            // Check if the card's last review verdict was "pass" or "approved"
            let last_verdict: Option<String> = conn
                .query_row(
                    "SELECT last_verdict FROM card_review_state WHERE card_id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            match last_verdict.as_deref() {
                Some("pass") | Some("approved") => {
                    let review_round: Option<i64> = conn
                        .query_row(
                            "SELECT review_round FROM card_review_state WHERE card_id = ?1",
                            [card_id],
                            |row| row.get(0),
                        )
                        .ok();

                    // Carry forward finding_categories from the review dispatch that found issues.
                    // The most recent review dispatch is typically the pass/approved one with
                    // empty items, so we walk backwards to find one with actual findings.
                    // This ensures that if TN is later corrected to FN on reopen, categories
                    // are already present.
                    let finding_cats: Option<String> = conn
                        .prepare(
                            "SELECT td.result FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                         AND td.status = 'completed' ORDER BY td.rowid DESC",
                        )
                        .ok()
                        .and_then(|mut stmt| {
                            let rows = stmt
                                .query_map([card_id], |row| row.get::<_, Option<String>>(0))
                                .ok()?;
                            for row_result in rows {
                                if let Ok(Some(result_str)) = row_result {
                                    if let Ok(v) =
                                        serde_json::from_str::<serde_json::Value>(&result_str)
                                    {
                                        if let Some(items) = v["items"].as_array() {
                                            let cats: Vec<String> = items
                                                .iter()
                                                .filter_map(|it| {
                                                    it["category"].as_str().map(|s| s.to_string())
                                                })
                                                .collect();
                                            if !cats.is_empty() {
                                                return serde_json::to_string(&cats).ok();
                                            }
                                        }
                                    }
                                }
                            }
                            None
                        });

                    let inserted = conn.execute(
                    "INSERT INTO review_tuning_outcomes \
                     (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) \
                     VALUES (?1, NULL, ?2, ?3, 'done', 'true_negative', ?4)",
                    sqlite_test::params![card_id, review_round, last_verdict.as_deref().unwrap_or("pass"), finding_cats],
                )
                .map(|n| n > 0)
                .unwrap_or(false);
                    if inserted {
                        tracing::info!(
                            "[review-tuning] #119 recorded true_negative: card={card_id} (pass → done)"
                        );
                    }
                    return inserted;
                }
                _ => {} // No review or non-pass verdict — nothing to record
            }
        }
    }
    false
}

/// #119: When a card is reopened after reaching done with a pass verdict,
/// correct any true_negative outcomes to false_negative — the review missed a real bug.
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
                "[review-tuning] #119 corrected {updated} true_negative → false_negative: card={log_card_id} (reopen, latest round only)"
            );
        }
        return;
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        let Some(db) = _db else {
            return;
        };

        if let Ok(conn) = db.lock() {
            // Only correct the most recent TN (latest review_round) to avoid
            // corrupting historical TN records from earlier rounds
            let updated = conn
            .execute(
                "UPDATE review_tuning_outcomes SET outcome = 'false_negative' \
                 WHERE card_id = ?1 AND outcome = 'true_negative' \
                 AND review_round = (SELECT MAX(review_round) FROM review_tuning_outcomes WHERE card_id = ?1 AND outcome = 'true_negative')",
                [card_id],
            )
            .unwrap_or(0);
            if updated > 0 {
                tracing::info!(
                    "[review-tuning] #119 corrected {updated} true_negative → false_negative: card={card_id} (reopen, latest round only)"
                );

                // Backfill finding_categories if empty. The TN was recorded using the
                // last review dispatch (the pass/approved one with empty items). Look
                // for an earlier review dispatch that actually found issues.
                let needs_backfill: bool = conn
                .query_row(
                    "SELECT finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]' \
                     FROM review_tuning_outcomes \
                     WHERE card_id = ?1 AND outcome = 'false_negative' \
                     ORDER BY rowid DESC LIMIT 1",
                    [card_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);

                if needs_backfill {
                    // Walk through review dispatches (most recent first) to find
                    // one with a non-empty items array containing categories
                    let finding_cats: Option<String> = conn
                        .prepare(
                            "SELECT td.result FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                         AND td.status = 'completed' \
                         ORDER BY td.rowid DESC",
                        )
                        .ok()
                        .and_then(|mut stmt| {
                            let rows = stmt
                                .query_map([card_id], |row| row.get::<_, Option<String>>(0))
                                .ok()?;
                            for row_result in rows {
                                if let Ok(Some(result_str)) = row_result {
                                    if let Ok(v) =
                                        serde_json::from_str::<serde_json::Value>(&result_str)
                                    {
                                        if let Some(items) = v["items"].as_array() {
                                            if !items.is_empty() {
                                                let cats: Vec<String> = items
                                                    .iter()
                                                    .filter_map(|it| {
                                                        it["category"]
                                                            .as_str()
                                                            .map(|s| s.to_string())
                                                    })
                                                    .collect();
                                                if !cats.is_empty() {
                                                    return serde_json::to_string(&cats).ok();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            None
                        });

                    if let Some(ref cats) = finding_cats {
                        let backfilled = conn
                        .execute(
                            "UPDATE review_tuning_outcomes SET finding_categories = ?1 \
                             WHERE card_id = ?2 AND outcome = 'false_negative' \
                             AND (finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]')",
                            sqlite_test::params![cats, card_id],
                        )
                        .unwrap_or(0);
                        if backfilled > 0 {
                            tracing::info!(
                                "[review-tuning] #119 backfilled {backfilled} FN finding_categories: card={card_id} categories={cats}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::*;
    use crate::kanban::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn completed_dispatch_only_does_not_authorize_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-completed", "requested").await;
        seed_dispatch_pg(&pool, "card-completed", "completed").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-completed",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_err(),
            "completed dispatch should NOT authorize transition"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("active dispatch"),
            "error should mention active dispatch"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn pending_dispatch_authorizes_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-pending", "requested").await;
        seed_dispatch_pg(&pool, "card-pending", "pending").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-pending",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "pending dispatch should authorize transition"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn dispatched_status_authorizes_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-dispatched", "requested").await;
        seed_dispatch_pg(&pool, "card-dispatched", "dispatched").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-dispatched",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "dispatched status should authorize transition"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn no_dispatch_blocks_non_free_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-none", "requested").await;
        // No dispatch at all

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-none",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(result.is_err(), "no dispatch should block transition");
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn free_transition_works_without_dispatch_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-free", "backlog").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-free",
            "ready",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "backlog → ready should work without dispatch"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn force_overrides_dispatch_check() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-force", "requested").await;
        // No dispatch, but force=true

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-force",
            "in_progress",
            "pmd",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "force=true should bypass dispatch check");
        pg_db.close_pool_and_drop(pool).await;
    }

    #[test]
    fn sync_terminal_card_state_cancels_pending_implementation_dispatch() {
        let db = test_db();
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-terminal-sync", "done");
        seed_dispatch_with_type(
            &db,
            "dispatch-card-terminal-sync-pending",
            "card-terminal-sync",
            "implementation",
            "pending",
        );

        sync_terminal_card_state(&db, "card-terminal-sync");

        let status: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-card-terminal-sync-pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    #[tokio::test]
    async fn stale_completed_review_verdict_does_not_open_current_done_gate() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-stale-review-pass", "review").await;

        sqlx::query(
            "UPDATE kanban_cards
             SET review_entered_at = NOW()
             WHERE id = 'card-stale-review-pass'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                'review-stale-pass', 'card-stale-review-pass', 'agent-1', 'review', 'completed',
                'stale pass', $1::jsonb,
                NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes'
             )",
        )
        .bind(json!({"verdict": "pass"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-stale-review-pass",
            "done",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_err(),
            "completed review verdicts from older rounds must not satisfy the current review_passed gate"
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM kanban_cards WHERE id = 'card-stale-review-pass'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "review",
            "stale review verdict must leave the card in review"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn legacy_review_without_review_entered_at_keeps_latest_pass_behavior() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-legacy-review-pass", "review").await;

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                'review-legacy-pass', 'card-legacy-review-pass', 'agent-1', 'review', 'completed',
                'legacy pass', $1::jsonb,
                NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
             )",
        )
        .bind(json!({"verdict": "pass"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-legacy-review-pass",
            "done",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "cards without review_entered_at must preserve the legacy pass verdict behavior"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn transition_status_with_on_conn_rolls_back_on_cleanup_error_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-force-rollback", "requested").await;
        seed_dispatch_pg(&pool, "card-force-rollback", "pending").await;

        let result = transition_status_with_opts_and_allowed_cleanup_pg_only(
            &pool,
            &engine,
            "card-force-rollback",
            "in_progress",
            "pmd",
            crate::engine::transition::ForceIntent::OperatorOverride,
            AllowedOnConnMutation::TestOnlyRollbackGuard,
        )
        .await;
        assert!(result.is_err(), "cleanup failure must abort the transition");

        let status: String =
            sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-force-rollback'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status, "requested",
            "cleanup failure must roll back the card status change"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[test]
    fn drain_hook_side_effects_materializes_tick_dispatch_intents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-dispatch.js"),
            r#"
            var policy = {
                name: "tick-dispatch",
                priority: 1,
                onTick30s: function() {
                    agentdesk.dispatch.create(
                        "card-tick",
                        "agent-1",
                        "rework",
                        "Tick Rework"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-tick", "requested");

        engine
            .try_fire_hook_by_name("onTick30s", json!({}))
            .unwrap();
        drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-tick' AND dispatch_type = 'rework'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "tick hook dispatch intent should be persisted");
    }

    /// Regression test for #274: status transitions fire custom state hooks
    /// through try_fire_hook_by_name(), and dispatch.create() in that path must
    /// return with the dispatch row + notify outbox already materialized.
    #[tokio::test]
    async fn transition_status_custom_on_enter_hook_materializes_dispatch_outbox_pg() {
        let dir = TempDir::new().unwrap();
        let worktree_path_json =
            serde_json::to_string(dir.path().to_string_lossy().as_ref()).unwrap();
        let hook_source = r#"
            var policy = {
                name: "ready-enter-hook",
                priority: 1,
                onCustomReadyEnter: function(payload) {
                    agentdesk.dispatch.create(
                        payload.card_id,
                        "agent-1",
                        "implementation",
                        "Ready Hook Dispatch",
                        {
                            worktree_path: __WORKTREE_PATH__,
                            worktree_branch: "test-ready-hook"
                        }
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#
        .replace("__WORKTREE_PATH__", &worktree_path_json);
        std::fs::write(dir.path().join("ready-enter-hook.js"), hook_source).unwrap();

        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg_and_dir(pool.clone(), dir.path());
        seed_card_pg(&pool, "card-ready-hook", "backlog").await;

        sqlx::query("UPDATE agents SET pipeline_config = $1::jsonb WHERE id = 'agent-1'")
            .bind(
                json!({
                    "hooks": {
                        "ready": {
                            "on_enter": ["onCustomReadyEnter"],
                            "on_exit": []
                        }
                    }
                })
                .to_string(),
            )
            .execute(&pool)
            .await
            .unwrap();

        transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-ready-hook",
            "ready",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await
        .unwrap();

        let (dispatch_id, title): (String, String) = sqlx::query_as(
            "SELECT id, title FROM task_dispatches WHERE kanban_card_id = 'card-ready-hook'",
        )
        .fetch_one(&pool)
        .await
        .expect("custom ready on_enter hook should create a dispatch");
        assert_eq!(title, "Ready Hook Dispatch");

        let notify_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("dispatch outbox query should succeed");
        assert_eq!(
            notify_count, 1,
            "custom transition hook dispatch must enqueue exactly one notify outbox row"
        );

        let (card_status, latest_dispatch_id): (String, String) = sqlx::query_as(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-ready-hook'",
        )
        .fetch_one(&pool)
        .await
        .expect("card should be updated by dispatch.create()");
        assert_eq!(card_status, "in_progress");
        assert_eq!(latest_dispatch_id, dispatch_id);
        pg_db.close_pool_and_drop(pool).await;
    }

    /// Regression guard for the known-hook path: try_fire_hook_by_name() must
    /// return with dispatch.create() side-effects already visible, even without
    /// an extra drain_hook_side_effects() call at the caller.
    #[test]
    fn try_fire_hook_drains_dispatch_intents_without_explicit_drain() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-intent.js"),
            r#"
            var policy = {
                name: "tick-intent",
                priority: 1,
                onTick1min: function() {
                    agentdesk.dispatch.create(
                        "card-intent-test",
                        "agent-1",
                        "implementation",
                        "Intent Drain Test"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-intent-test", "requested");

        // Fire tick hook — do NOT call drain_hook_side_effects afterwards.
        // The intent should still be drained by try_fire_hook's internal drain.
        engine
            .try_fire_hook_by_name("OnTick1min", json!({}))
            .unwrap();

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-intent-test' AND dispatch_type = 'implementation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "#202: tick hook dispatch intent must be persisted by try_fire_hook's internal drain"
        );
    }

    #[test]
    fn fire_transition_hooks_terminal_cleanup_cancels_review_followups_with_reason() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-terminal-cleanup", "review");
        seed_dispatch_with_type(
            &db,
            "dispatch-rd-cleanup",
            "card-terminal-cleanup",
            "review-decision",
            "pending",
        );
        seed_dispatch_with_type(
            &db,
            "dispatch-rw-cleanup",
            "card-terminal-cleanup",
            "rework",
            "dispatched",
        );
        seed_dispatch_with_type(
            &db,
            "dispatch-review-keep",
            "card-terminal-cleanup",
            "review",
            "pending",
        );

        fire_transition_hooks(&db, &engine, "card-terminal-cleanup", "review", "done");

        let conn = db.lock().unwrap();
        let (rd_status, rd_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, json_extract(result, '$.reason') FROM task_dispatches WHERE id = 'dispatch-rd-cleanup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let (rw_status, rw_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, json_extract(result, '$.reason') FROM task_dispatches WHERE id = 'dispatch-rw-cleanup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let review_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-review-keep'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(rd_status, "cancelled");
        assert_eq!(rd_reason.as_deref(), Some(TERMINAL_DISPATCH_CLEANUP_REASON));
        assert_eq!(rw_status, "cancelled");
        assert_eq!(rw_reason.as_deref(), Some(TERMINAL_DISPATCH_CLEANUP_REASON));
        assert_eq!(
            review_status, "pending",
            "terminal cleanup must not cancel pending review dispatches"
        );
    }

    // ── Pipeline / auto-queue regression tests (#110) ──────────────

    /// #110: Pipeline stage should NOT advance on implementation dispatch completion alone.
    /// The onDispatchCompleted in pipeline.js is now a no-op — advancement happens
    /// only through review-automation processVerdict after review passes.
    #[test]
    fn pipeline_no_auto_advance_on_dispatch_complete() {
        let db = test_db();
        let engine = test_engine(&db);

        seed_card_with_repo(&db, "card-pipe", "in_progress", "repo-1");
        let (stage1, _stage2) = seed_pipeline_stages(&db, "repo-1");

        // Assign pipeline stage (use integer id)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET pipeline_stage_id = ?1 WHERE id = 'card-pipe'",
                [stage1],
            )
            .unwrap();
        }

        // Create and complete an implementation dispatch
        seed_dispatch(&db, "card-pipe", "pending");
        let dispatch_id = "dispatch-card-pipe-pending";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'completed', result = '{}' WHERE id = ?1",
                [dispatch_id],
            )
            .unwrap();
        }

        // Fire OnDispatchCompleted — should NOT create a new dispatch for stage-2
        let _ = engine
            .try_fire_hook_by_name("OnDispatchCompleted", json!({ "dispatch_id": dispatch_id }));

        // Verify: pipeline_stage_id should still be stage-1 (not advanced)
        // pipeline_stage_id is TEXT, pipeline_stages.id is INTEGER AUTOINCREMENT
        let stage_id: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT pipeline_stage_id FROM kanban_cards WHERE id = 'card-pipe'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            stage_id.as_deref(),
            Some(stage1.to_string().as_str()),
            "pipeline_stage_id must NOT advance on dispatch completion alone"
        );

        // Verify: no new pending dispatch was created for stage-2
        let new_dispatches: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-pipe' AND status = 'pending'",
                [],
                |row| row.get(0),
            ).unwrap()
        };
        assert_eq!(
            new_dispatches, 0,
            "no new dispatch should be created by pipeline.js onDispatchCompleted"
        );
    }

    /// #110: Rust transition_status marks auto_queue_entries as done,
    /// and this single update is sufficient (no JS triple-update).
    #[tokio::test]
    async fn transition_to_done_marks_auto_queue_entry_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        // Seed cards for the queue
        seed_card_pg(&pool, "card-q1", "review").await;
        seed_card_pg(&pool, "card-q2", "ready").await;
        seed_dispatch_pg(&pool, "card-q1", "pending").await;
        let (_run_id, entry_a, _entry_b) = seed_auto_queue_run_pg(&pool, "agent-1").await;

        // Transition card-q1 to done
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-q1",
            "done",
            "review",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "transition to done should succeed");

        // Verify: entry_a should be 'done' (set by Rust transition_status)
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            entry_status, "done",
            "Rust must mark auto_queue_entry as done"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn run_completion_waits_for_phase_gate_then_enqueues_notify_to_main_channel() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine_with_pg(pool.clone());

        seed_card_with_repo_pg(&pool, "card-notify", "review", "repo-1").await;
        seed_dispatch_pg(&pool, "card-notify", "pending").await;

        sqlx::query(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread, unified_thread_id, thread_group_count, created_at
             )
             VALUES ($1, $2, $3, 'active', TRUE, $4::jsonb, 1, NOW())",
        )
        .bind("run-notify")
        .bind("repo-1")
        .bind("agent-1")
        .bind(r#"{"123":"thread-999"}"#)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, created_at
             )
             VALUES ($1, $2, $3, $4, 'dispatched', $5, 1, NOW())",
        )
        .bind("entry-notify")
        .bind("run-notify")
        .bind("card-notify")
        .bind("agent-1")
        .bind("dispatch-card-notify-pending")
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-notify",
            "done",
            "review",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "transition to done should succeed");

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-notify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            run_status, "paused",
            "single-phase terminal completion must pause for a phase gate"
        );

        let phase_gate_dispatch_id: String = sqlx::query_scalar(
            "SELECT id FROM task_dispatches
             WHERE kanban_card_id = 'card-notify' AND dispatch_type = 'phase-gate'
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let queued_notifications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            queued_notifications, 0,
            "completion notify must wait for the phase gate to pass"
        );

        let completed = crate::dispatch::complete_dispatch(
            &db,
            &engine,
            &phase_gate_dispatch_id,
            &json!({
                "verdict": "phase_gate_passed",
                "summary": "phase gate approved"
            }),
        )
        .expect("phase gate completion should succeed");
        assert_eq!(completed["status"], "completed");

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-notify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(run_status, "completed");

        let (target, bot, content): (String, String, String) = sqlx::query_as(
            "SELECT target, bot, content FROM message_outbox ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(bot, "notify");
        assert!(
            content.contains("자동큐 완료: repo-1 / run run-noti / 1개"),
            "notify message should summarize the completed run"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// #110: non-terminal manual recovery transitions must not complete auto-queue entries.
    #[tokio::test]
    async fn requested_force_transition_does_not_complete_auto_queue_entry_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        seed_card_pg(&pool, "card-pd", "review").await;
        seed_dispatch_pg(&pool, "card-pd", "pending").await;

        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status, agent_id, created_at)
             VALUES ('run-pd', 'active', 'agent-1', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
             )
             VALUES ('entry-pd', 'run-pd', 'card-pd', 'agent-1', 'dispatched', 1, NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Transition to requested (NOT done)
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-pd",
            "requested",
            "pm-gate",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok());

        // Verify: entry should still be 'dispatched' (not done)
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-pd'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            entry_status, "dispatched",
            "requested must NOT mark auto_queue_entry as done"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// #128: started_at must reset on every in_progress re-entry (rework/resume).
    /// YAML pipeline uses `mode: coalesce` for in_progress clock, which preserves
    /// the original started_at on rework re-entry. This prevents losing the original
    /// start timestamp. Timeouts.js handles rework re-entry by checking the current
    /// dispatch's created_at rather than started_at.
    #[tokio::test]
    async fn started_at_coalesces_on_in_progress_reentry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '123', '456')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, started_at, created_at, updated_at
             )
             VALUES ('card-rework', 'Test', 'review', 'agent-1', NOW() - INTERVAL '3 hours', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Add dispatch to authorize transition
        seed_dispatch_pg(&pool, "card-rework", "pending").await;

        // Transition back to in_progress (simulates rework)
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-rework",
            "in_progress",
            "pm-decision",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok(), "rework transition should succeed");

        // Verify started_at was PRESERVED (coalesce mode: original timestamp kept)
        let age_seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - started_at))::bigint
             FROM kanban_cards
             WHERE id = 'card-rework'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            age_seconds > 3500,
            "started_at should be preserved (coalesce mode), but was only {} seconds ago",
            age_seconds
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// When started_at is NULL (first-time entry), coalesce mode sets it to now.
    #[tokio::test]
    async fn started_at_set_on_first_in_progress_entry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        seed_card_pg(&pool, "card-first", "requested").await;

        seed_dispatch_pg(&pool, "card-first", "pending").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-first",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok());

        let age_seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - started_at))::bigint
             FROM kanban_cards
             WHERE id = 'card-first'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            age_seconds < 60,
            "started_at should be set to now on first entry, but was {} seconds ago",
            age_seconds
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// #800: `reset_full=true` reopens must scrub recorded worktree metadata
    /// from `task_dispatches.context` / `task_dispatches.result` so a follow-up
    /// `latest_completed_work_dispatch_target` call cannot silently re-inject
    /// the stale path into the new dispatch context.
    #[test]
    fn cleanup_force_transition_revert_fields_strips_dispatch_worktree_metadata() {
        let db = test_db();
        seed_card(&db, "card-800-strip-wt", "in_progress");

        let conn = db.lock().unwrap();
        // Two dispatches on the same card, one completed implementation that
        // recorded both context-side and result-side wt metadata, plus a
        // pending dispatch with only context-side wt metadata. We assert that
        // ALL wt-locating keys are removed but unrelated fields survive.
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-completed', 'card-800-strip-wt', 'agent-1', 'implementation', 'completed',
                'Old impl', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-stale",
                    "worktree_branch": "wt/800-old",
                    "preserve_me": "context_value"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": "/tmp/agentdesk-800-stale",
                    "completed_branch": "wt/800-old",
                    "completed_commit": "deadbeefcafebabe",
                    "preserve_me_too": "result_value"
                })
                .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-pending', 'card-800-strip-wt', 'agent-1', 'implementation', 'pending',
                'New impl', ?1, NULL, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-also-stale",
                    "worktree_branch": "wt/800-also-old",
                    "title_hint": "redispatch"
                })
                .to_string(),
            ],
        )
        .unwrap();
        // A second card's dispatch must be untouched by the scoped cleanup.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-800-other', 'Other', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-other-card', 'card-800-other', 'agent-1', 'implementation', 'completed',
                'Other impl', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-other-keep",
                    "worktree_branch": "wt/800-other-keep"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": "/tmp/agentdesk-800-other-keep",
                    "completed_branch": "wt/800-other-keep"
                })
                .to_string(),
            ],
        )
        .unwrap();

        cleanup_force_transition_revert_fields_on_conn(&conn, "card-800-strip-wt").unwrap();

        // Helper to read a dispatch JSON column back as a serde value.
        let load_json = |dispatch_id: &str, column: &str| -> Option<serde_json::Value> {
            let raw: Option<String> = conn
                .query_row(
                    &format!("SELECT {column} FROM task_dispatches WHERE id = ?1"),
                    [dispatch_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            raw.and_then(|s| serde_json::from_str(&s).ok())
        };

        let completed_ctx = load_json("d-800-completed", "context").unwrap();
        assert!(
            completed_ctx.get("worktree_path").is_none(),
            "context.worktree_path must be removed, got {completed_ctx:?}"
        );
        assert!(
            completed_ctx.get("worktree_branch").is_none(),
            "context.worktree_branch must be removed, got {completed_ctx:?}"
        );
        assert_eq!(
            completed_ctx["preserve_me"].as_str(),
            Some("context_value"),
            "unrelated context fields must be preserved"
        );

        let completed_result = load_json("d-800-completed", "result").unwrap();
        assert!(
            completed_result.get("completed_worktree_path").is_none(),
            "result.completed_worktree_path must be removed, got {completed_result:?}"
        );
        assert!(
            completed_result.get("completed_branch").is_none(),
            "result.completed_branch must be removed, got {completed_result:?}"
        );
        assert_eq!(
            completed_result["completed_commit"].as_str(),
            Some("deadbeefcafebabe"),
            "completion evidence (completed_commit) must be preserved as audit history"
        );
        assert_eq!(
            completed_result["preserve_me_too"].as_str(),
            Some("result_value")
        );

        let pending_ctx = load_json("d-800-pending", "context").unwrap();
        assert!(pending_ctx.get("worktree_path").is_none());
        assert!(pending_ctx.get("worktree_branch").is_none());
        assert_eq!(pending_ctx["title_hint"].as_str(), Some("redispatch"));

        // Other card untouched — both wt-locating keys must still be present.
        let other_ctx = load_json("d-800-other-card", "context").unwrap();
        assert_eq!(
            other_ctx["worktree_path"].as_str(),
            Some("/tmp/agentdesk-800-other-keep"),
            "cleanup must be card-scoped and not touch unrelated cards"
        );
        let other_result = load_json("d-800-other-card", "result").unwrap();
        assert_eq!(
            other_result["completed_worktree_path"].as_str(),
            Some("/tmp/agentdesk-800-other-keep")
        );
    }

    #[test]
    fn github_sync_target_requires_registered_repo_and_matching_issue_repo() {
        let db = test_db();
        seed_card(&db, "card-github-sync-guard", "review");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET repo_id = 'owner/allowed',
                     github_issue_url = 'https://github.com/owner/other/issues/101',
                     github_issue_number = 101
                 WHERE id = 'card-github-sync-guard'",
                [],
            )
            .unwrap();
        }

        // Mismatched URL repo must be rejected.
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            None
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET github_issue_url = 'https://github.com/owner/allowed/issues/101'
                 WHERE id = 'card-github-sync-guard'",
                [],
            )
            .unwrap();
        }
        // Matching repo but not registered must still be rejected.
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            None
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES ('owner/allowed', 'Allowed Repo', 1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            Some(("owner/allowed".to_string(), 101))
        );
    }

    /// #821 (5): `onDispatchCompleted` (kanban-rules.js) must skip cancelled
    /// dispatches. A race can fire the hook after the user cancels a
    /// dispatch; without the guard the policy would force-transition the
    /// card to `review` and the terminal sweep would then push it to `done`,
    /// overriding the user's explicit stop. #815 added the guard —
    /// `if (dispatch.status === "cancelled") return;` — and this test locks
    /// the behaviour.
    #[test]
    fn cancelled_dispatch_does_not_enter_review() {
        let db = test_db();
        let engine = test_engine(&db);

        // Seed a card currently in `in_progress` with a cancelled
        // implementation dispatch. Absent the #815 guard the policy would
        // drive the card into `review` on hook fan-out.
        seed_card(&db, "card-821-no-review", "in_progress");
        let dispatch_id = "dispatch-821-no-review";
        seed_dispatch_with_type(
            &db,
            dispatch_id,
            "card-821-no-review",
            "implementation",
            "cancelled",
        );

        // Fire the hook the same way the real runtime would.
        engine
            .try_fire_hook_by_name("OnDispatchCompleted", json!({ "dispatch_id": dispatch_id }))
            .expect("fire OnDispatchCompleted");

        // The card must remain in its prior status — NOT `review`, NOT `done`.
        let status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-821-no-review'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            status, "in_progress",
            "kanban-rules.onDispatchCompleted must skip cancelled dispatches"
        );
    }
}
