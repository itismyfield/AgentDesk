use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::super::AppState;
use super::review_state_repo::update_card_review_state;
use super::tuning_aggregate::{record_decision_tuning, spawn_aggregate_if_needed_with_pg};

fn legacy_db(state: &AppState) -> &crate::db::Db {
    /* TODO(#1238 / 843g): the runtime is PG-only, so neither the engine nor
    AppState carry a legacy SQLite handle. Every caller in this file is
    guarded by a `pg_pool_ref()` PG-first branch — the value returned here
    is only read in the SQLite fallback used by tests. The static
    placeholder satisfies the existing `&Db` signatures until #1238 ports
    the call sites to PG-only. */
    use std::sync::OnceLock;
    static PLACEHOLDER: OnceLock<crate::db::Db> = OnceLock::new();
    state
        .engine
        .legacy_db()
        .or_else(|| state.legacy_db())
        .unwrap_or_else(|| {
            PLACEHOLDER.get_or_init(super::super::pending_migration_shim_for_callers)
        })
}

/// PG-first wrapper around `crate::kanban::transition_status_with_opts`.
///
/// SQLite-only callers fail with `card not found` after the PG cutover when
/// the card lives only in Postgres. Prefer `transition_status_with_opts_pg`
/// when a PG pool is configured and fall through to the legacy SQLite path
/// only when running without PG.
async fn transition_status_pg_first(
    state: &AppState,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
) -> anyhow::Result<crate::kanban::TransitionResult> {
    if let Some(pool) = state.pg_pool_ref() {
        crate::kanban::transition_status_with_opts_pg_only(
            pool,
            &state.engine,
            card_id,
            new_status,
            source,
            force_intent,
        )
        .await
    } else {
        crate::kanban::transition_status_with_opts(
            legacy_db(state),
            &state.engine,
            card_id,
            new_status,
            source,
            force_intent,
        )
    }
}

fn spawn_review_tuning_aggregate_pg_first(state: &AppState) {
    spawn_aggregate_if_needed_with_pg(state.pg_pool_ref().cloned());
}

// ── Review Decision (agent's response to counter-model review) ──────────────

#[cfg(test)]
fn test_worktree_commit_override_slot() -> &'static std::sync::Mutex<Option<Option<String>>> {
    static OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<Option<String>>>> =
        std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
pub(crate) fn set_test_worktree_commit_override(commit: Option<String>) {
    if let Ok(mut slot) = test_worktree_commit_override_slot().lock() {
        *slot = Some(commit);
    }
}

#[cfg(test)]
pub(crate) fn clear_test_worktree_commit_override() {
    if let Ok(mut slot) = test_worktree_commit_override_slot().lock() {
        *slot = None;
    }
}

async fn current_issue_worktree_commit(
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
    issue_num: i64,
    context: Option<&serde_json::Value>,
) -> Option<String> {
    #[cfg(test)]
    {
        if let Ok(slot) = test_worktree_commit_override_slot().lock() {
            if let Some(override_commit) = slot.clone() {
                return override_commit;
            }
        }
    }

    let Some(pool) = pg_pool else {
        tracing::warn!(
            "[review-decision] current_issue_worktree_commit: card {} issue #{}: postgres pool unavailable",
            card_id,
            issue_num
        );
        return None;
    };

    match crate::dispatch::resolve_card_worktree(pool, card_id, context).await {
        Ok(Some((_worktree_path, _branch, commit))) => Some(commit),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                "[review-decision] current_issue_worktree_commit: card {} issue #{}: {}",
                card_id,
                issue_num,
                err
            );
            None
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ActiveAcceptFollowups {
    review: i64,
    rework: i64,
    review_decision: i64,
}

impl ActiveAcceptFollowups {
    fn has_followup(self) -> bool {
        self.review > 0 || self.rework > 0
    }
}

fn active_accept_followups(db: &crate::db::Db, card_id: &str) -> ActiveAcceptFollowups {
    db.lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0), \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'rework' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0), \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0) \
                 FROM task_dispatches \
                 WHERE kanban_card_id = ?1",
                [card_id],
                |row| {
                    Ok(ActiveAcceptFollowups {
                        review: row.get(0)?,
                        rework: row.get(1)?,
                        review_decision: row.get(2)?,
                    })
                },
            )
            .ok()
        })
        .unwrap_or_default()
}

async fn active_accept_followups_pg_first(
    state: &AppState,
    card_id: &str,
) -> ActiveAcceptFollowups {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)::BIGINT, \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'rework' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)::BIGINT, \
                     COALESCE(SUM(CASE WHEN dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched') THEN 1 ELSE 0 END), 0)::BIGINT \
                 FROM task_dispatches \
                 WHERE kanban_card_id = $1",
            )
            .bind(card_id)
            .fetch_one(pool)
            .await
        {
            Ok((review, rework, review_decision)) => ActiveAcceptFollowups {
                review,
                rework,
                review_decision,
            },
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres accept followups"
                );
                ActiveAcceptFollowups::default()
            }
        };
    }

    active_accept_followups(legacy_db(state), card_id)
}

fn current_card_status(db: &crate::db::Db, card_id: &str) -> Option<String> {
    db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    })
}

async fn current_card_status_pg_first(state: &AppState, card_id: &str) -> Option<String> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, String>(
            "SELECT status FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card status"
                );
                None
            }
        };
    }

    current_card_status(legacy_db(state), card_id)
}

#[derive(Debug, Default)]
struct ReviewDecisionCardContext {
    status: Option<String>,
    repo_id: Option<String>,
    agent_id: Option<String>,
    title: Option<String>,
}

async fn load_review_decision_card_context_pg_first(
    state: &AppState,
    card_id: &str,
) -> ReviewDecisionCardContext {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(
            "SELECT status, repo_id, assigned_agent_id, title
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some((status, repo_id, agent_id, title))) => ReviewDecisionCardContext {
                status,
                repo_id,
                agent_id,
                title,
            },
            Ok(None) => ReviewDecisionCardContext::default(),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card context"
                );
                ReviewDecisionCardContext::default()
            }
        };
    }

    legacy_db(state)
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT status, repo_id, assigned_agent_id, title
                 FROM kanban_cards
                 WHERE id = ?1",
                [card_id],
                |row| {
                    Ok(ReviewDecisionCardContext {
                        status: row.get(0)?,
                        repo_id: row.get(1)?,
                        agent_id: row.get(2)?,
                        title: row.get(3)?,
                    })
                },
            )
            .ok()
        })
        .unwrap_or_default()
}

async fn resolve_effective_pipeline_pg_first(
    state: &AppState,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> crate::pipeline::PipelineConfig {
    crate::pipeline::ensure_loaded();

    if let Some(pool) = state.pg_pool_ref() {
        return crate::pipeline::resolve_for_card_pg(pool, repo_id, agent_id).await;
    }

    match legacy_db(state).lock() {
        Ok(conn) => crate::pipeline::resolve_for_card(&conn, repo_id, agent_id),
        Err(error) => {
            tracing::warn!(
                "[review-decision] failed to lock sqlite while resolving pipeline fallback: {error}"
            );
            crate::pipeline::resolve(None, None)
        }
    }
}

async fn card_exists_pg_first(state: &AppState, card_id: &str) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM kanban_cards WHERE id = $1)",
        )
        .bind(card_id)
        .fetch_one(pool)
        .await
        {
            Ok(exists) => exists,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to check postgres card existence"
                );
                false
            }
        };
    }

    legacy_db(state)
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT 1 FROM kanban_cards WHERE id = ?1",
                [card_id],
                |_row| Ok(()),
            )
            .ok()
        })
        .is_some()
}

async fn pending_review_decision_dispatch_id_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<String> {
    if let Some(pool) = state.pg_pool_ref() {
        match sqlx::query_scalar::<_, String>(
            "SELECT td.id
             FROM task_dispatches td
             JOIN card_review_state crs ON crs.pending_dispatch_id = td.id
             WHERE crs.card_id = $1
               AND td.dispatch_type = 'review-decision'
               AND td.status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(dispatch_id)) => return Some(dispatch_id),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres pending review-decision by review state"
                );
                return None;
            }
        }

        return match sqlx::query_scalar::<_, String>(
            "SELECT td.id
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id
             WHERE kc.id = $1
               AND td.dispatch_type = 'review-decision'
               AND td.status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(dispatch_id) => dispatch_id,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres pending review-decision by latest dispatch"
                );
                None
            }
        };
    }

    legacy_db(state).lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT td.id FROM task_dispatches td \
             JOIN card_review_state crs ON crs.pending_dispatch_id = td.id \
             WHERE crs.card_id = ?1 AND td.dispatch_type = 'review-decision' \
             AND td.status IN ('pending', 'dispatched')",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .or_else(|| {
            conn.query_row(
                "SELECT td.id FROM task_dispatches td \
                 JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id \
                 WHERE kc.id = ?1 AND td.dispatch_type = 'review-decision' \
                 AND td.status IN ('pending', 'dispatched')",
                [card_id],
                |row| row.get(0),
            )
            .ok()
        })
    })
}

async fn emit_card_updated(state: &AppState, card_id: &str) {
    if let Some(pool) = state.pg_pool_ref() {
        match super::super::kanban::load_card_json_pg(pool, card_id).await {
            Ok(Some(card)) => {
                crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
                return;
            }
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card for kanban_card_updated emit"
                );
                return;
            }
        }
    }

    if let Ok(conn) = legacy_db(state).lock() {
        if let Ok(card) = conn.query_row(
            &format!("{} WHERE kc.id = ?1", super::super::kanban::CARD_SELECT),
            [card_id],
            |row| super::super::kanban::card_row_to_json(row),
        ) {
            crate::server::ws::emit_event(&state.broadcast_tx, "kanban_card_updated", card);
        }
    }
}

async fn mark_next_review_round_advance_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<bool, String> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for review round advance".to_string())?;
    let metadata_raw =
        sqlx::query_scalar::<_, Option<String>>("SELECT metadata FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres metadata for {card_id}: {error}"))?
            .flatten();

    let mut metadata = metadata_raw
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({}));
    let object = metadata
        .as_object_mut()
        .expect("review round advance metadata must be an object");
    if object
        .get(crate::engine::ops::ADVANCE_REVIEW_ROUND_HINT_KEY)
        .and_then(|value| value.as_bool())
        == Some(true)
    {
        return Ok(false);
    }

    object.insert(
        crate::engine::ops::ADVANCE_REVIEW_ROUND_HINT_KEY.to_string(),
        serde_json::Value::Bool(true),
    );

    sqlx::query("UPDATE kanban_cards SET metadata = $1, updated_at = NOW() WHERE id = $2")
        .bind(metadata.to_string())
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| format!("update postgres metadata for {card_id}: {error}"))?;
    Ok(true)
}

fn dispatch_status_and_result(
    db: &crate::db::Db,
    dispatch_id: &str,
) -> Option<(String, Option<String>)> {
    db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT status, result FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    })
}

async fn dispatch_status_and_result_pg_first(
    state: &AppState,
    dispatch_id: &str,
) -> Option<(String, Option<String>)> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, result FROM task_dispatches WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    %error,
                    "[review-decision] failed to load postgres dispatch status"
                );
                None
            }
        };
    }

    dispatch_status_and_result(legacy_db(state), dispatch_id)
}

#[derive(Debug, Clone)]
struct ActiveReviewDispatch {
    id: String,
    reviewed_commit: Option<String>,
    target_repo: Option<String>,
}

fn latest_active_review_dispatch(
    db: &crate::db::Db,
    card_id: &str,
) -> Option<ActiveReviewDispatch> {
    db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT id, context FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
             AND status IN ('pending', 'dispatched') \
             ORDER BY rowid DESC LIMIT 1",
            [card_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .ok()
        .map(|(id, context_raw)| {
            let context = context_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
            let target_repo = context
                .as_ref()
                .and_then(|value| {
                    value
                        .get("target_repo")
                        .and_then(|entry| entry.as_str())
                        .map(str::to_string)
                })
                .or_else(|| {
                    context
                        .as_ref()
                        .and_then(|value| value.get("worktree_path"))
                        .and_then(|entry| entry.as_str())
                        .and_then(|path| {
                            crate::services::platform::shell::resolve_repo_dir_for_target(Some(
                                path,
                            ))
                            .ok()
                            .flatten()
                        })
                });
            ActiveReviewDispatch {
                id,
                reviewed_commit: context.as_ref().and_then(|value| {
                    value
                        .get("reviewed_commit")
                        .and_then(|entry| entry.as_str())
                        .map(str::to_string)
                }),
                target_repo,
            }
        })
    })
}

fn build_active_review_dispatch(id: String, context_raw: Option<String>) -> ActiveReviewDispatch {
    let context = context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let target_repo = context
        .as_ref()
        .and_then(|value| {
            value
                .get("target_repo")
                .and_then(|entry| entry.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            context
                .as_ref()
                .and_then(|value| value.get("worktree_path"))
                .and_then(|entry| entry.as_str())
                .and_then(|path| {
                    crate::services::platform::shell::resolve_repo_dir_for_target(Some(path))
                        .ok()
                        .flatten()
                })
        });
    ActiveReviewDispatch {
        id,
        reviewed_commit: context.as_ref().and_then(|value| {
            value
                .get("reviewed_commit")
                .and_then(|entry| entry.as_str())
                .map(str::to_string)
        }),
        target_repo,
    }
}

async fn latest_active_review_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<ActiveReviewDispatch> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')
             ORDER BY updated_at DESC NULLS LAST, created_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row.map(|(id, context_raw)| build_active_review_dispatch(id, context_raw)),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres active review dispatch"
                );
                None
            }
        };
    }

    latest_active_review_dispatch(legacy_db(state), card_id)
}

async fn latest_completed_review_context_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<serde_json::Value> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, Option<String>>(
            "SELECT context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status = 'completed'
             ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(context_raw) => context_raw
                .flatten()
                .and_then(|ctx| serde_json::from_str::<serde_json::Value>(&ctx).ok()),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres completed review context"
                );
                None
            }
        };
    }

    legacy_db(state)
        .lock()
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT context FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
                 AND status = 'completed' \
                 ORDER BY completed_at DESC LIMIT 1",
                [card_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
        .and_then(|ctx_str| serde_json::from_str::<serde_json::Value>(&ctx_str).ok())
}

async fn card_issue_number_pg_first(state: &AppState, card_id: &str) -> Option<i64> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, Option<i64>>(
            "SELECT github_issue_number::BIGINT FROM kanban_cards WHERE id = $1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(issue_number) => issue_number.flatten(),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres card issue number"
                );
                None
            }
        };
    }

    legacy_db(state).lock().ok().and_then(|c| {
        c.query_row(
            "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    })
}

async fn stale_review_dispatch_ids_pg_first(state: &AppState, card_id: &str) -> Vec<String> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_all(pool)
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres stale review dispatches"
                );
                Vec::new()
            }
        };
    }

    legacy_db(state)
        .lock()
        .ok()
        .and_then(|conn| {
            conn.prepare(
                "SELECT id FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
            )
            .ok()
            .map(|mut stmt| {
                stmt.query_map([card_id], |row| row.get::<_, String>(0))
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row.ok())
                    .collect()
            })
        })
        .unwrap_or_default()
}

async fn prepare_dispute_review_entry_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<(), String> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for dispute review-entry".to_string())?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dispute review-entry tx for {card_id}: {error}"))?;
    let dispute_intents = [
        crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: Some("reviewing".to_string()),
        },
        crate::engine::transition::TransitionIntent::SyncReviewState {
            card_id: card_id.to_string(),
            state: "reviewing".to_string(),
        },
    ];
    for intent in &dispute_intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await?;
    }
    sqlx::query("UPDATE kanban_cards SET review_entered_at = NOW() WHERE id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("set review_entered_at for {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit dispute review-entry tx for {card_id}: {error}"))?;
    Ok(())
}

async fn finalize_accept_cleanup_pg_first(
    state: &AppState,
    card_id: &str,
    clear_review_status: bool,
) -> Result<(), String> {
    let pool = state
        .pg_pool_ref()
        .ok_or_else(|| "postgres pool unavailable for accept cleanup".to_string())?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin accept cleanup tx for {card_id}: {error}"))?;
    if clear_review_status {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(
            &mut tx,
            &crate::engine::transition::TransitionIntent::SetReviewStatus {
                card_id: card_id.to_string(),
                review_status: None,
            },
        )
        .await?;
    }
    sqlx::query("UPDATE kanban_cards SET suggestion_pending_at = NULL WHERE id = $1")
        .bind(card_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("clear suggestion_pending_at for {card_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit accept cleanup tx for {card_id}: {error}"))?;
    Ok(())
}

async fn commit_belongs_to_card_issue_pg_first(
    state: &AppState,
    card_id: &str,
    commit_sha: &str,
    target_repo: Option<&str>,
) -> bool {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::commit_belongs_to_card_issue_pg(
            pool,
            card_id,
            commit_sha,
            target_repo,
        )
        .await;
    }

    crate::dispatch::commit_belongs_to_card_issue(
        legacy_db(state),
        card_id,
        commit_sha,
        target_repo,
    )
}

async fn cancel_dispatch_pg_first(
    state: &AppState,
    dispatch_id: &str,
    reason: Option<&str>,
) -> Result<usize, String> {
    if let Some(pool) = state.pg_pool_ref() {
        return crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
            pool,
            dispatch_id,
            reason,
        )
        .await;
    }

    let conn = legacy_db(state)
        .lock()
        .map_err(|error| format!("database lock poisoned: {error}"))?;
    crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(&conn, dispatch_id, reason)
        .map_err(|error| error.to_string())
}

async fn dismiss_review_cleanup_pg_first(state: &AppState, card_id: &str) -> Result<(), String> {
    let Some(pool) = state.pg_pool_ref() else {
        let conn = legacy_db(state)
            .lock()
            .map_err(|error| format!("database lock poisoned: {error}"))?;
        let dispatch_ids: Vec<String> = conn
            .prepare(
                "SELECT id FROM task_dispatches
                 WHERE kanban_card_id = ?1
                   AND status IN ('pending', 'dispatched')
                   AND dispatch_type IN ('review', 'review-decision')",
            )
            .map_err(|error| {
                format!("prepare dismiss cleanup dispatch query for {card_id}: {error}")
            })?
            .query_map([card_id], |row| row.get::<_, String>(0))
            .map_err(|error| format!("load dismiss cleanup dispatches for {card_id}: {error}"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| format!("decode dismiss cleanup dispatches for {card_id}: {error}"))?;

        for dispatch_id in &dispatch_ids {
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(&conn, dispatch_id, None)
                .map_err(|error| error.to_string())?;
        }

        let clear_review_status = crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: None,
        };
        crate::engine::transition::execute_intent_on_conn(&conn, &clear_review_status)
            .map_err(|error| error.to_string())?;

        conn.execute(
            "UPDATE kanban_cards
             SET channel_thread_map = NULL,
                 active_thread_id = NULL
             WHERE id = ?1",
            [card_id],
        )
        .map_err(|error| format!("clear dismiss thread mappings for {card_id}: {error}"))?;
        return Ok(());
    };

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin dismiss cleanup tx for {card_id}: {error}"))?;

    let dispatch_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND dispatch_type IN ('review', 'review-decision')",
    )
    .bind(card_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load dismiss cleanup dispatches for {card_id}: {error}"))?;

    for dispatch_id in &dispatch_ids {
        crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(&mut tx, dispatch_id, None)
            .await?;
    }

    let clear_review_status = crate::engine::transition::TransitionIntent::SetReviewStatus {
        card_id: card_id.to_string(),
        review_status: None,
    };
    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        &mut tx,
        &clear_review_status,
    )
    .await?;

    sqlx::query(
        "UPDATE kanban_cards
         SET channel_thread_map = NULL,
             active_thread_id = NULL
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("clear dismiss thread mappings for {card_id}: {error}"))?;

    tx.commit()
        .await
        .map_err(|error| format!("commit dismiss cleanup tx for {card_id}: {error}"))?;
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct ReviewDecisionBody {
    pub card_id: String,
    pub decision: String, // "accept", "dispute", "dismiss"
    pub comment: Option<String>,
    /// #109: dispatch-scoped targeting — when provided, the server validates
    /// that this dispatch_id matches the pending review-decision dispatch for
    /// the card. Prevents replayed/stale decisions from consuming the wrong
    /// dispatch.
    pub dispatch_id: Option<String>,
}

/// POST /api/review-decision
///
/// Agent's decision on counter-model review feedback.
/// - accept: agent will rework based on review → card to in_progress
/// - dispute: agent disagrees, sends back for re-review → new review dispatch
/// - dismiss: agent ignores review → card to done
pub async fn submit_review_decision(
    State(state): State<AppState>,
    Json(body): Json<ReviewDecisionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let valid = ["accept", "dispute", "dismiss"];
    if !valid.contains(&body.decision.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("decision must be one of: {}", valid.join(", "))})),
        );
    }

    if !card_exists_pg_first(&state, &body.card_id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "card not found"})),
        );
    }

    let pending_rd_id = pending_review_decision_dispatch_id_pg_first(&state, &body.card_id).await;

    if pending_rd_id.is_none() {
        // No pending review-decision dispatch → stale or duplicate call
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "no pending review-decision dispatch for this card",
                "card_id": body.card_id,
            })),
        );
    }

    // #109: When dispatch_id is provided, validate it matches the pending
    // review-decision dispatch. This prevents replayed or stale decisions from
    // consuming a different dispatch than the one they were issued for.
    if let Some(ref submitted_did) = body.dispatch_id {
        if pending_rd_id.as_deref() != Some(submitted_did.as_str()) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "dispatch_id mismatch: submitted {} but pending is {}",
                        submitted_did,
                        pending_rd_id.as_deref().unwrap_or("(none)")
                    ),
                    "card_id": body.card_id,
                })),
            );
        }
    }
    match body.decision.as_str() {
        "accept" => {
            // #195: Agent accepts review feedback — create a rework dispatch so the
            // agent can address the findings. When the rework dispatch completes,
            // OnDispatchCompleted (kanban-rules.js) transitions to review for re-review.
            let card_ctx = load_review_decision_card_context_pg_first(&state, &body.card_id).await;
            let card_status_now = card_ctx.status.clone().unwrap_or_default();
            let card_repo_id = card_ctx.repo_id.clone();
            let card_agent_id = card_ctx.agent_id.clone();
            let card_title = card_ctx.title.clone();
            let effective_pipeline = resolve_effective_pipeline_pg_first(
                &state,
                card_repo_id.as_deref(),
                card_agent_id.as_deref(),
            )
            .await;

            // Guard: terminal card
            if effective_pipeline.is_terminal(&card_status_now) {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "card is terminal, cannot accept review feedback",
                        "card_id": body.card_id,
                    })),
                );
            }

            // Find rework target via review_rework gate (same logic as timeouts.js section E)
            let rework_target = effective_pipeline
                .transitions
                .iter()
                .find(|t| {
                    t.from == card_status_now
                        && t.transition_type == crate::pipeline::TransitionType::Gated
                        && t.gates.iter().any(|g| g == "review_rework")
                })
                .map(|t| t.to.clone())
                .unwrap_or_else(|| {
                    effective_pipeline
                        .dispatchable_states()
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| effective_pipeline.initial_state().to_string())
                });

            // #246: Check if the agent already committed new work during the
            // review-decision turn. If the worktree HEAD differs from the
            // reviewed_commit of the last review, skip rework and go straight
            // to review (the agent already addressed the feedback).
            let skip_rework = {
                let last_review_context =
                    latest_completed_review_context_pg_first(&state, &body.card_id).await;

                let last_reviewed_commit: Option<String> =
                    last_review_context.as_ref().and_then(|v| {
                        v.get("reviewed_commit")
                            .and_then(|c| c.as_str())
                            .map(|s| s.to_string())
                    });

                let issue_number = card_issue_number_pg_first(&state, &body.card_id).await;

                if let (Some(prev_commit), Some(issue_num)) = (&last_reviewed_commit, issue_number)
                {
                    let current_commit = current_issue_worktree_commit(
                        state.engine.pg_pool(),
                        &body.card_id,
                        issue_num,
                        last_review_context.as_ref(),
                    )
                    .await;
                    if let Some(ref cur) = current_commit {
                        let differs = cur != prev_commit;
                        if differs {
                            tracing::info!(
                                "[review-decision] #246 New commit detected for card {}: prev={} cur={} — skipping rework",
                                body.card_id,
                                &prev_commit[..8.min(prev_commit.len())],
                                &cur[..8.min(cur.len())]
                            );
                        }
                        differs
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            let mut accept_failures = Vec::new();
            let mut direct_review_auto_approved = false;

            // #246: If agent already committed new work, skip rework and re-enter
            // review via a two-step transition (rework_target → review) so that
            // OnReviewEnter fires naturally (increments review_round, sets
            // review_status, creates review dispatch via review-automation.js).
            let direct_review_attempted = skip_rework;
            let mut direct_review_created = if skip_rework {
                // Find the review state from the pipeline (gated transition from rework_target)
                let review_state = effective_pipeline
                    .transitions
                    .iter()
                    .find(|t| {
                        t.from == rework_target
                            && t.transition_type == crate::pipeline::TransitionType::Gated
                    })
                    .map(|t| t.to.clone());

                if let Some(ref review_st) = review_state {
                    if let Err(error) =
                        mark_next_review_round_advance_pg_first(&state, &body.card_id).await
                    {
                        accept_failures.push(format!(
                            "failed to mark review round advance before direct review: {error}"
                        ));
                        tracing::warn!(
                            "[review-decision] failed to mark direct-review round advance for card {}: {}",
                            body.card_id,
                            error
                        );
                    }
                    // Step 1: Transition to rework_target (e.g., in_progress)
                    match transition_status_pg_first(
                        &state,
                        &body.card_id,
                        &rework_target,
                        "review_decision_accept_skip_rework_step1",
                        crate::engine::transition::ForceIntent::SystemRecovery,
                    )
                    .await
                    {
                        Ok(_) => {
                            // Step 2: Transition to review — fires OnReviewEnter
                            match transition_status_pg_first(
                                &state,
                                &body.card_id,
                                review_st,
                                "review_decision_accept_skip_rework_step2",
                                crate::engine::transition::ForceIntent::SystemRecovery,
                            )
                            .await
                            {
                                Ok(_) => {
                                    // Materialize any follow-up transitions queued by
                                    // OnReviewEnter (for example, single-provider
                                    // auto-approval to terminal) before checking
                                    // whether a live review dispatch exists.
                                    crate::kanban::drain_hook_side_effects(
                                        legacy_db(&state),
                                        &state.engine,
                                    );
                                    let followups =
                                        active_accept_followups_pg_first(&state, &body.card_id)
                                            .await;
                                    if followups.review > 0 {
                                        tracing::info!(
                                            "[review-decision] #246 Direct review re-entry for card {}: {} → {} → {} (rework skipped)",
                                            body.card_id,
                                            card_status_now,
                                            rework_target,
                                            review_st
                                        );
                                        true
                                    } else if current_card_status_pg_first(&state, &body.card_id)
                                        .await
                                        .as_deref()
                                        .map(|status| effective_pipeline.is_terminal(status))
                                        .unwrap_or(false)
                                    {
                                        direct_review_auto_approved = true;
                                        tracing::info!(
                                            "[review-decision] #483 Direct review re-entry for card {} auto-approved without review dispatch (no alternate reviewer)",
                                            body.card_id
                                        );
                                        false
                                    } else {
                                        accept_failures.push(format!(
                                        "direct review transition reached {} but no active review dispatch was created",
                                        review_st
                                    ));
                                        tracing::warn!(
                                            "[review-decision] #339 Direct review re-entry for card {} reached {} but no active review dispatch exists",
                                            body.card_id,
                                            review_st
                                        );
                                        false
                                    }
                                }
                                Err(e) => {
                                    accept_failures.push(format!(
                                        "direct review step2 transition to {} failed: {e}",
                                        review_st
                                    ));
                                    tracing::warn!(
                                        "[review-decision] #246 Step 2 transition to {} failed for card {}: {e}",
                                        review_st,
                                        body.card_id
                                    );
                                    false
                                }
                            }
                        }
                        Err(e) => {
                            accept_failures.push(format!(
                                "direct review step1 transition to {} failed: {e}",
                                rework_target
                            ));
                            tracing::warn!(
                                "[review-decision] #339 Step 1 transition to {} failed for card {} during direct review: {e}",
                                rework_target,
                                body.card_id
                            );
                            false
                        }
                    }
                } else {
                    accept_failures.push(format!(
                        "skip_rework requested but no review state could be resolved from rework target {}",
                        rework_target
                    ));
                    false
                }
            } else {
                false
            };

            // Create rework dispatch on the normal accept path, or as a fallback when
            // direct review re-entry fails / produces no active review dispatch.
            if !direct_review_created && !direct_review_auto_approved {
                let card_status_before_rework =
                    current_card_status_pg_first(&state, &body.card_id).await;
                let rework_transition_ready = card_status_before_rework.as_deref()
                    == Some(rework_target.as_str())
                    || match transition_status_pg_first(
                        &state,
                        &body.card_id,
                        &rework_target,
                        "review_decision_accept",
                        crate::engine::transition::ForceIntent::SystemRecovery,
                    )
                    .await
                    {
                        Ok(_) => true,
                        Err(e) => {
                            accept_failures.push(format!(
                                "transition to rework target {} failed: {e}",
                                rework_target
                            ));
                            tracing::warn!(
                                "[review-decision] #195 Transition to rework target failed for card {}: {e}",
                                body.card_id
                            );
                            false
                        }
                    };

                if rework_transition_ready {
                    if let Some(ref agent_id) = card_agent_id {
                        let rework_title = format!(
                            "[Rework] {}",
                            card_title.as_deref().unwrap_or(&body.card_id)
                        );
                        match crate::dispatch::create_dispatch_with_options(
                            legacy_db(&state),
                            state.engine.pg_pool(),
                            &state.engine,
                            &body.card_id,
                            agent_id,
                            "rework",
                            &rework_title,
                            &json!({}),
                            crate::dispatch::DispatchCreateOptions::default(),
                        ) {
                            Ok(dispatch) => {
                                let dispatch_id = dispatch
                                    .get("id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("(unknown)");
                                tracing::info!(
                                    "[review-decision] #195 Rework dispatch created: card={} dispatch={}",
                                    body.card_id,
                                    dispatch_id
                                );
                            }
                            Err(e) => {
                                accept_failures
                                    .push(format!("rework dispatch creation failed: {e}"));
                                tracing::warn!(
                                    "[review-decision] #195 Rework dispatch creation failed for card {}: {e}",
                                    body.card_id
                                );
                            }
                        }
                    } else {
                        accept_failures.push(format!(
                            "no assigned agent for rework dispatch on card {}",
                            body.card_id
                        ));
                        tracing::warn!(
                            "[review-decision] #195 No agent assigned to card {} — cannot create rework dispatch",
                            body.card_id
                        );
                    }
                }
            }

            let followups = active_accept_followups_pg_first(&state, &body.card_id).await;
            direct_review_created = followups.review > 0;
            let rework_dispatch_created = followups.rework > 0;
            let terminal_auto_approved = direct_review_attempted
                && (direct_review_auto_approved
                    || (!direct_review_created
                        && !rework_dispatch_created
                        && current_card_status_pg_first(&state, &body.card_id)
                            .await
                            .as_deref()
                            .map(|status| effective_pipeline.is_terminal(status))
                            .unwrap_or(false)));

            if !followups.has_followup() && !terminal_auto_approved {
                let card_status_after = current_card_status_pg_first(&state, &body.card_id).await;
                tracing::error!(
                    card_id = %body.card_id,
                    pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                    card_status_before = %card_status_now,
                    card_status_after = card_status_after.as_deref().unwrap_or("(unknown)"),
                    rework_target = %rework_target,
                    skip_rework,
                    direct_review_attempted,
                    direct_review_created,
                    rework_dispatch_created,
                    active_review = followups.review,
                    active_rework = followups.rework,
                    active_review_decision = followups.review_decision,
                    failures = ?accept_failures,
                    "[review-decision] #339 accept failed closed: no follow-up dispatch created"
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "review-decision accept failed: no follow-up dispatch created",
                        "card_id": body.card_id,
                        "pending_dispatch_id": pending_rd_id,
                        "skip_rework": skip_rework,
                        "card_status_before": card_status_now,
                        "card_status_after": card_status_after,
                        "rework_target": rework_target,
                        "followups": {
                            "review": followups.review,
                            "rework": followups.rework,
                            "review_decision": followups.review_decision,
                        },
                        "failures": accept_failures,
                    })),
                );
            }

            if let Some(ref rd_id) = pending_rd_id {
                match crate::dispatch::mark_dispatch_completed_pg_first(
                    legacy_db(&state),
                    state.pg_pool_ref(),
                    rd_id,
                    &json!({"decision": "accept", "completion_source": "review_decision_api"}),
                ) {
                    Ok(1) => {}
                    Ok(_) => {
                        let dispatch_consumed_by_terminal_cleanup = terminal_auto_approved
                            && dispatch_status_and_result_pg_first(&state, rd_id)
                                .await
                                .map(|(status, result)| {
                                    if status == "completed" {
                                        return true;
                                    }
                                    if status != "cancelled" {
                                        return false;
                                    }
                                    result
                                        .as_deref()
                                        .and_then(|raw| {
                                            serde_json::from_str::<serde_json::Value>(raw).ok()
                                        })
                                        .and_then(|value| {
                                            value
                                                .get("reason")
                                                .and_then(|reason| reason.as_str())
                                                .map(str::to_string)
                                        })
                                        .as_deref()
                                        .is_some_and(|reason| {
                                            reason == "auto_cancelled_on_terminal_card"
                                                || reason == "js_terminal_cleanup"
                                        })
                                })
                                .unwrap_or(false);
                        let dispatch_no_longer_active = terminal_auto_approved
                            && active_accept_followups_pg_first(&state, &body.card_id)
                                .await
                                .review_decision
                                == 0;
                        if dispatch_consumed_by_terminal_cleanup || dispatch_no_longer_active {
                            tracing::info!(
                                "[review-decision] #483 pending review-decision {} for card {} was already consumed by terminal auto-approval",
                                rd_id,
                                body.card_id
                            );
                        } else {
                            let live_dispatches =
                                active_accept_followups_pg_first(&state, &body.card_id).await;
                            tracing::error!(
                                card_id = %body.card_id,
                                pending_rd_id = %rd_id,
                                active_review = live_dispatches.review,
                                active_rework = live_dispatches.rework,
                                active_review_decision = live_dispatches.review_decision,
                                "[review-decision] #339 accept created a follow-up dispatch but failed to finalize the pending review-decision"
                            );
                            return (
                                StatusCode::CONFLICT,
                                Json(json!({
                                    "error": "failed to finalize pending review-decision after follow-up dispatch creation",
                                    "card_id": body.card_id,
                                    "pending_dispatch_id": rd_id,
                                })),
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            active_review = followups.review,
                            active_rework = followups.rework,
                            error = %e,
                            "[review-decision] #339 accept created a follow-up dispatch but mark_dispatch_completed errored"
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!("failed to finalize pending review-decision: {e}"),
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                            })),
                        );
                    }
                }
            };

            // Clear suggestion_pending_at (always) and review_status (rework path only).
            // #266: review_status was left as "suggestion_pending" because the
            // review→in_progress rework transition is non-terminal and
            // ClearTerminalFields never fires.
            // Guard: when direct_review_created, OnReviewEnter already set
            // review_status='reviewing' — clearing it would break the live review.
            finalize_accept_cleanup_pg_first(
                &state,
                &body.card_id,
                !direct_review_created && !terminal_auto_approved,
            )
            .await
            .ok();

            // #119: Record tuning outcome
            record_decision_tuning(
                state.pg_pool_ref(),
                &body.card_id,
                "accept",
                pending_rd_id.as_deref(),
            )
            .await;
            spawn_review_tuning_aggregate_pg_first(&state);

            // #117: Update canonical review state.
            // For direct review: OnReviewEnter already set the state, so skip the
            // rework_pending override that would conflict with the live review dispatch.
            if !direct_review_created && !terminal_auto_approved {
                update_card_review_state(
                    state.pg_pool_ref().is_none().then_some(legacy_db(&state)),
                    state.pg_pool_ref(),
                    &body.card_id,
                    "accept",
                    pending_rd_id.as_deref(),
                );
            }

            emit_card_updated(&state, &body.card_id).await;
            let message = if terminal_auto_approved {
                "Review-decision accepted, review auto-approved (no alternate reviewer)"
            } else if direct_review_created {
                "Review-decision accepted, direct review dispatch created (rework skipped)"
            } else {
                "Review-decision accepted, rework dispatch created"
            };
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "accept",
                    "rework_dispatch_created": rework_dispatch_created,
                    "direct_review_created": direct_review_created,
                    "review_auto_approved": terminal_auto_approved,
                    "message": message,
                })),
            );
        }
        "dispute" => {
            if let Err(error) = prepare_dispute_review_entry_pg_first(&state, &body.card_id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }

            // #119: Record tuning outcome BEFORE OnReviewEnter (which increments review_round)
            record_decision_tuning(
                state.pg_pool_ref(),
                &body.card_id,
                "dispute",
                pending_rd_id.as_deref(),
            )
            .await;
            spawn_review_tuning_aggregate_pg_first(&state);

            // #229: Cancel stale pending/dispatched review dispatches for this card.
            // Without this, the dispatch-core dedup guard blocks
            // OnReviewEnter from creating a fresh review dispatch after dispute.
            let stale_ids = stale_review_dispatch_ids_pg_first(&state, &body.card_id).await;
            let mut cancelled = 0usize;
            for stale_id in &stale_ids {
                if cancel_dispatch_pg_first(
                    &state,
                    stale_id,
                    Some("superseded_by_dispute_re_review"),
                )
                .await
                .unwrap_or(0)
                    > 0
                {
                    cancelled += 1;
                }
            }
            if cancelled > 0 {
                tracing::info!(
                    "[review-decision] #229 Cancelled {} stale review dispatch(es) for card {} before dispute re-review",
                    cancelled,
                    body.card_id
                );
            }

            // Fire on_enter hooks for current state (should be a review-like state with OnReviewEnter)
            let dispute_status = current_card_status_pg_first(&state, &body.card_id)
                .await
                .unwrap_or_else(|| "review".to_string());
            crate::kanban::fire_enter_hooks(
                legacy_db(&state),
                &state.engine,
                &body.card_id,
                &dispute_status,
            );

            // #108: Drain all pending intents and transitions from OnReviewEnter hooks.
            // drain_hook_side_effects handles both transition processing (e.g. setStatus
            // for review/manual-intervention follow-up on max rounds) and Discord notifications for any
            // dispatches created by the hooks, eliminating the previous manual drain loop
            // that only handled transitions and missed dispatch notifications.
            crate::kanban::drain_hook_side_effects(legacy_db(&state), &state.engine);

            // #229: Safety net — if card is still in a review-like state but no
            // pending review dispatch exists (OnReviewEnter hook may have failed
            // due to lock contention or JS error), re-fire with blocking lock.
            {
                let card_ctx =
                    load_review_decision_card_context_pg_first(&state, &body.card_id).await;
                let has_review_dispatch = if let Some(pool) = state.pg_pool_ref() {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT COUNT(*) > 0
                         FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type IN ('review', 'review-decision')
                           AND status IN ('pending', 'dispatched')",
                    )
                    .bind(&body.card_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(false)
                } else {
                    legacy_db(&state)
                        .lock()
                        .ok()
                        .and_then(|conn| {
                            conn.query_row(
                                "SELECT COUNT(*) > 0 FROM task_dispatches \
                                 WHERE kanban_card_id = ?1 AND dispatch_type IN ('review', 'review-decision') \
                                 AND status IN ('pending', 'dispatched')",
                                [&body.card_id],
                                |row| row.get(0),
                            )
                            .ok()
                        })
                        .unwrap_or(false)
                };
                let effective_pipeline = resolve_effective_pipeline_pg_first(
                    &state,
                    card_ctx.repo_id.as_deref(),
                    card_ctx.agent_id.as_deref(),
                )
                .await;
                let needs_review = card_ctx.status.as_deref().is_some_and(|status| {
                    effective_pipeline
                        .hooks_for_state(status)
                        .is_some_and(|hooks| {
                            hooks.on_enter.iter().any(|name| name == "OnReviewEnter")
                        })
                }) && !has_review_dispatch;

                if needs_review {
                    tracing::warn!(
                        "[review-decision] Card {} in review state but no review dispatch after dispute — re-firing OnReviewEnter (#229)",
                        body.card_id
                    );
                    let _ = state.engine.fire_hook_by_name_blocking(
                        "OnReviewEnter",
                        json!({ "card_id": body.card_id }),
                    );
                    crate::kanban::drain_hook_side_effects(legacy_db(&state), &state.engine);
                }
            }

            let live_review = match latest_active_review_dispatch_pg_first(&state, &body.card_id)
                .await
            {
                Some(dispatch) => dispatch,
                None => {
                    tracing::error!(
                        card_id = %body.card_id,
                        pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                        "[review-decision] #491 dispute failed closed: no live review dispatch after re-review entry"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": "review-decision dispute failed: no follow-up review dispatch created",
                            "card_id": body.card_id,
                            "pending_dispatch_id": pending_rd_id,
                        })),
                    );
                }
            };

            if let Some(ref reviewed_commit) = live_review.reviewed_commit {
                if !commit_belongs_to_card_issue_pg_first(
                    &state,
                    &body.card_id,
                    reviewed_commit,
                    live_review.target_repo.as_deref(),
                )
                .await
                {
                    let _ = cancel_dispatch_pg_first(
                        &state,
                        &live_review.id,
                        Some("invalid_dispute_rereview_target"),
                    )
                    .await;
                    tracing::error!(
                        card_id = %body.card_id,
                        pending_rd_id = pending_rd_id.as_deref().unwrap_or(""),
                        review_dispatch_id = %live_review.id,
                        reviewed_commit = %reviewed_commit,
                        "[review-decision] #491 dispute failed closed: re-review target does not belong to the card issue"
                    );
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": "review-decision dispute failed: re-review target is stale or unrelated to the card issue",
                            "card_id": body.card_id,
                            "pending_dispatch_id": pending_rd_id,
                            "review_dispatch_id": live_review.id,
                            "reviewed_commit": reviewed_commit,
                        })),
                    );
                }
            }

            if let Some(ref rd_id) = pending_rd_id {
                match crate::dispatch::mark_dispatch_completed_pg_first(
                    legacy_db(&state),
                    state.pg_pool_ref(),
                    rd_id,
                    &json!({"decision": "dispute", "completion_source": "review_decision_api"}),
                ) {
                    Ok(1) => {}
                    Ok(_) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %live_review.id,
                            "[review-decision] #491 dispute created a follow-up review dispatch but failed to finalize the pending review-decision"
                        );
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({
                                "error": "failed to finalize pending review-decision after re-review dispatch creation",
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": live_review.id,
                            })),
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            card_id = %body.card_id,
                            pending_rd_id = %rd_id,
                            review_dispatch_id = %live_review.id,
                            error = %e,
                            "[review-decision] #491 dispute created a follow-up review dispatch but mark_dispatch_completed errored"
                        );
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "error": format!("failed to finalize pending review-decision: {e}"),
                                "card_id": body.card_id,
                                "pending_dispatch_id": rd_id,
                                "review_dispatch_id": live_review.id,
                            })),
                        );
                    }
                }
            }

            // #117: Update canonical review state before returning
            update_card_review_state(
                state.pg_pool_ref().is_none().then_some(legacy_db(&state)),
                state.pg_pool_ref(),
                &body.card_id,
                "dispute",
                pending_rd_id.as_deref(),
            );

            emit_card_updated(&state, &body.card_id).await;
            return (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "card_id": body.card_id,
                    "decision": "dispute",
                    "review_dispatch_id": live_review.id,
                    "reviewed_commit": live_review.reviewed_commit,
                    "message": "Re-review dispatched to counter-model",
                })),
            );
        }
        "dismiss" => {
            // Agent dismisses review → transition to terminal state, then clean up stale state.
            // Order matters: transition_status requires an active dispatch, so we must
            // transition BEFORE cancelling pending dispatches.
            let card_ctx = load_review_decision_card_context_pg_first(&state, &body.card_id).await;
            let effective_pipeline = resolve_effective_pipeline_pg_first(
                &state,
                card_ctx.repo_id.as_deref(),
                card_ctx.agent_id.as_deref(),
            )
            .await;
            let terminal_state = effective_pipeline
                .states
                .iter()
                .find(|state| state.terminal)
                .map(|state| state.id.clone())
                .unwrap_or_else(|| "done".to_string());
            let _ = transition_status_pg_first(
                &state,
                &body.card_id,
                &terminal_state,
                "dismiss",
                crate::engine::transition::ForceIntent::SystemRecovery, // dismiss bypasses review_passed gate
            )
            .await;

            // Post-transition cleanup: cancel remaining pending review dispatches to prevent
            // stale dispatches from re-triggering review loops after dismiss.
            if let Err(error) = dismiss_review_cleanup_pg_first(&state, &body.card_id).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        }
        _ => {}
    }

    // #117: Update canonical review state for all decision paths
    update_card_review_state(
        state.pg_pool_ref().is_none().then_some(legacy_db(&state)),
        state.pg_pool_ref(),
        &body.card_id,
        &body.decision,
        pending_rd_id.as_deref(),
    );
    // #119: Record tuning outcome (dismiss falls through here; accept/dispute call helper before returning)
    record_decision_tuning(
        state.pg_pool_ref(),
        &body.card_id,
        &body.decision,
        pending_rd_id.as_deref(),
    )
    .await;
    spawn_review_tuning_aggregate_pg_first(&state);

    emit_card_updated(&state, &body.card_id).await;

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "card_id": body.card_id,
            "decision": body.decision,
        })),
    )
}
