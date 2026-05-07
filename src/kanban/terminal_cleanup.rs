//! Terminal-card and managed-worktree cleanup helpers for kanban transitions.

use crate::db::Db;
use anyhow::Result;
use sqlx::Row as SqlxRow;

pub(super) const TERMINAL_DISPATCH_CLEANUP_REASON: &str = "auto_cancelled_on_terminal_card";

const STALE_WORKTREE_KEYS: &[&str] = &[
    "worktree_path",
    "worktree_branch",
    "completed_worktree_path",
    "completed_branch",
];

pub(super) fn sync_terminal_card_state(db: &Db, card_id: &str) {
    sync_terminal_card_state_with_scope(db, card_id, true);
}

pub(super) fn sync_terminal_transition_followups(db: &Db, card_id: &str) {
    sync_terminal_card_state_with_scope(db, card_id, false);
}

fn sync_terminal_card_state_with_scope(db: &Db, card_id: &str, cancel_implementation: bool) {
    #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
    {
        let _ = (db, card_id, cancel_implementation);
        return;
    }

    #[cfg(all(test, feature = "legacy-sqlite-tests"))]
    {
        let Ok(conn) = db.lock() else {
            return;
        };

        let dispatch_types = if cancel_implementation {
            "'implementation', 'review-decision', 'rework'"
        } else {
            "'review-decision', 'rework'"
        };

        let pending_followups: Vec<String> = conn
            .prepare(&format!(
                "SELECT id FROM task_dispatches \
                 WHERE kanban_card_id = ?1 AND dispatch_type IN ({dispatch_types}) \
                 AND status IN ('pending', 'dispatched')"
            ))
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([card_id], |row| row.get::<_, String>(0))
                    .ok()
                    .map(|rows| rows.filter_map(|row| row.ok()).collect())
            })
            .unwrap_or_default();

        let mut cancelled = 0usize;
        for dispatch_id in pending_followups {
            cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
                &conn,
                &dispatch_id,
                Some(TERMINAL_DISPATCH_CLEANUP_REASON),
            )
            .unwrap_or(0);
        }

        if cancelled > 0 {
            tracing::info!(
                "[kanban] Cancelled {} pending terminal follow-up dispatch(es) for card {}",
                cancelled,
                card_id
            );
        }
    }
}

pub(super) async fn sync_terminal_transition_followups_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<usize> {
    crate::github::sync::sync_auto_queue_terminal_on_pg(tx, card_id)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let dispatch_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('review-decision', 'rework')
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres terminal follow-up dispatches {card_id}: {error}")
    })?;

    let mut cancelled = 0usize;
    for dispatch_id in dispatch_ids {
        cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            tx,
            &dispatch_id,
            Some(TERMINAL_DISPATCH_CLEANUP_REASON),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    Ok(cancelled)
}

/// #800: Strip recorded worktree metadata from every `task_dispatches` row that
/// belongs to the given card while preserving unrelated audit payload fields.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn strip_stale_worktree_metadata_from_dispatches_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> Result<()> {
    let mut stmt =
        conn.prepare("SELECT id, context, result FROM task_dispatches WHERE kanban_card_id = ?1")?;
    let rows: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([card_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .filter_map(|row| row.ok())
        .collect();
    drop(stmt);

    for (dispatch_id, context_raw, result_raw) in rows {
        let new_context =
            scrub_worktree_keys_from_json(context_raw.as_deref(), STALE_WORKTREE_KEYS);
        let new_result = scrub_worktree_keys_from_json(result_raw.as_deref(), STALE_WORKTREE_KEYS);

        if new_context.is_none() && new_result.is_none() {
            continue;
        }

        let context_value: Option<String> = new_context.or(context_raw);
        let result_value: Option<String> = new_result.or(result_raw);

        conn.execute(
            "UPDATE task_dispatches SET context = ?1, result = ?2, updated_at = datetime('now') WHERE id = ?3",
            sqlite_test::params![context_value, result_value, dispatch_id],
        )?;
    }
    Ok(())
}

pub(super) async fn strip_stale_worktree_metadata_from_dispatches_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, context::text AS context, result::text AS result
         FROM task_dispatches
         WHERE kanban_card_id = $1",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load postgres dispatch cleanup rows for {card_id}: {error}")
    })?;

    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch id for {card_id}: {error}")
        })?;
        let context_raw: Option<String> = row.try_get("context").map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch context for {dispatch_id}: {error}")
        })?;
        let result_raw: Option<String> = row.try_get("result").map_err(|error| {
            anyhow::anyhow!("decode postgres dispatch result for {dispatch_id}: {error}")
        })?;

        let new_context =
            scrub_worktree_keys_from_json(context_raw.as_deref(), STALE_WORKTREE_KEYS);
        let new_result = scrub_worktree_keys_from_json(result_raw.as_deref(), STALE_WORKTREE_KEYS);

        if new_context.is_none() && new_result.is_none() {
            continue;
        }

        let context_value: Option<String> = new_context.or(context_raw);
        let result_value: Option<String> = new_result.or(result_raw);

        sqlx::query(
            "UPDATE task_dispatches
             SET context = $1::jsonb,
                 result = $2::jsonb,
                 updated_at = NOW()
             WHERE id = $3",
        )
        .bind(context_value)
        .bind(result_value)
        .bind(&dispatch_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!("save postgres dispatch cleanup row {dispatch_id}: {error}")
        })?;
    }

    Ok(())
}

pub(super) async fn cleanup_terminal_managed_worktrees_pg(
    pg_pool: &sqlx::PgPool,
    card_id: &str,
) -> Result<crate::services::platform::shell::ManagedWorktreeCleanup> {
    let mut summary = crate::services::platform::shell::ManagedWorktreeCleanup::default();
    let repo_id: Option<String> =
        sqlx::query_scalar("SELECT repo_id FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_optional(pg_pool)
            .await
            .map_err(|error| {
                anyhow::anyhow!("load card repo for managed worktree cleanup {card_id}: {error}")
            })?
            .flatten();
    let repo_dir =
        match crate::services::platform::shell::resolve_repo_dir_for_target(repo_id.as_deref()) {
            Ok(Some(path)) => path,
            Ok(None) => return Ok(summary),
            Err(error) => {
                tracing::warn!(
                    "[kanban] managed worktree cleanup skipped for {}: {}",
                    card_id,
                    error
                );
                return Ok(summary);
            }
        };

    let rows = sqlx::query(
        "SELECT context::text AS context, result::text AS result
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type IN ('implementation', 'rework')
           AND status = 'completed'",
    )
    .bind(card_id)
    .fetch_all(pg_pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!("load managed worktree cleanup dispatches {card_id}: {error}")
    })?;

    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let context_raw: Option<String> = row.try_get("context").map_err(|error| {
            anyhow::anyhow!("decode managed worktree cleanup context for {card_id}: {error}")
        })?;
        let result_raw: Option<String> = row.try_get("result").map_err(|error| {
            anyhow::anyhow!("decode managed worktree cleanup result for {card_id}: {error}")
        })?;
        let context_json = context_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let result_json = result_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let managed = context_json
            .as_ref()
            .is_some_and(|value| json_bool_field(value, "managed_worktree"));
        let cleanup_on_terminal = context_json
            .as_ref()
            .and_then(|value| json_string_field(value, "managed_worktree_cleanup"))
            .as_deref()
            .unwrap_or("terminal")
            == "terminal";
        if !managed || !cleanup_on_terminal {
            continue;
        }
        let worktree_path = context_json
            .as_ref()
            .and_then(|value| json_string_field(value, "worktree_path"))
            .or_else(|| {
                result_json
                    .as_ref()
                    .and_then(|value| json_string_field(value, "completed_worktree_path"))
            });
        let Some(worktree_path) = worktree_path else {
            continue;
        };
        if !seen.insert(worktree_path.clone()) {
            continue;
        }
        let item =
            crate::services::platform::shell::cleanup_managed_worktree(&repo_dir, &worktree_path);
        summary.removed += item.removed;
        summary.skipped_dirty += item.skipped_dirty;
        summary.skipped_unmerged += item.skipped_unmerged;
        summary.skipped_unmanaged += item.skipped_unmanaged;
        summary.failed += item.failed;
    }

    Ok(summary)
}

/// Returns `Some(serialized)` when at least one of `keys` was present in the
/// parsed JSON object, with those keys removed; otherwise returns `None` to
/// signal "no rewrite needed". `None` input or non-object payloads are passed
/// through as `None` so the caller leaves the column untouched.
fn scrub_worktree_keys_from_json(raw: Option<&str>, keys: &[&str]) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object_mut()?;
    let mut changed = false;
    for key in keys {
        if obj.remove(*key).is_some() {
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    serde_json::to_string(&value).ok()
}

fn json_string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(str::to_string)
}

fn json_bool_field(value: &serde_json::Value, key: &str) -> bool {
    value.get(key).and_then(|field| field.as_bool()) == Some(true)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::*;

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
}
