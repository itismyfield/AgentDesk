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

const WORKTREE_PATH_REFERENCE_KEYS: &[&str] = &["worktree_path", "completed_worktree_path"];

// reason: terminal-card-state sync entry; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub(super) fn sync_terminal_card_state(db: &Db, card_id: &str) {
    sync_terminal_card_state_with_scope(db, card_id, true);
}

// reason: terminal-transition follow-up sync called from hooks; lib-build callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
pub(super) fn sync_terminal_transition_followups(db: &Db, card_id: &str) {
    sync_terminal_card_state_with_scope(db, card_id, false);
}

// reason: scoped impl of the terminal-cleanup sync (sqlite/non-sqlite cfg arms); callers are cfg/test-gated. See #3034.
#[allow(dead_code)]
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
        "SELECT id, context::text AS context, result::text AS result
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
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            anyhow::anyhow!("decode managed worktree cleanup dispatch id for {card_id}: {error}")
        })?;
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

        let active_refs = active_worktree_refs_pg(pg_pool, &dispatch_id, &worktree_path).await?;
        if !active_refs.is_empty() {
            tracing::warn!(
                "[kanban] terminal managed worktree cleanup for {} skipped {} because active worktree reference(s) still point at it: {}",
                card_id,
                worktree_path,
                active_refs.join(", ")
            );
            continue;
        }

        let item =
            crate::services::platform::shell::cleanup_managed_worktree(&repo_dir, &worktree_path);
        tracing::info!(
            "[kanban] terminal managed worktree cleanup result for {} path {}: removed={}, dirty={}, unmerged={}, unmanaged={}, failed={}",
            card_id,
            worktree_path,
            item.removed,
            item.skipped_dirty,
            item.skipped_unmerged,
            item.skipped_unmanaged,
            item.failed
        );
        summary.removed += item.removed;
        summary.skipped_dirty += item.skipped_dirty;
        summary.skipped_unmerged += item.skipped_unmerged;
        summary.skipped_unmanaged += item.skipped_unmanaged;
        summary.failed += item.failed;
    }

    Ok(summary)
}

async fn active_worktree_refs_pg(
    pg_pool: &sqlx::PgPool,
    cleanup_dispatch_id: &str,
    worktree_path: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT id, kanban_card_id, context::text AS context, result::text AS result
         FROM task_dispatches
         WHERE id <> $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(cleanup_dispatch_id)
    .fetch_all(pg_pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!(
            "load active worktree dispatch references for {cleanup_dispatch_id}: {error}"
        )
    })?;

    let mut refs = Vec::new();
    for row in rows {
        let dispatch_id: String = row.try_get("id").map_err(|error| {
            anyhow::anyhow!("decode active worktree dispatch id for {cleanup_dispatch_id}: {error}")
        })?;
        let card_id: Option<String> = row.try_get("kanban_card_id").map_err(|error| {
            anyhow::anyhow!("decode active worktree card id for {dispatch_id}: {error}")
        })?;
        let context_raw: Option<String> = row.try_get("context").map_err(|error| {
            anyhow::anyhow!("decode active worktree context for {dispatch_id}: {error}")
        })?;
        let result_raw: Option<String> = row.try_get("result").map_err(|error| {
            anyhow::anyhow!("decode active worktree result for {dispatch_id}: {error}")
        })?;

        let context_json = context_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let result_json = result_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let context_matches = context_json
            .as_ref()
            .is_some_and(|value| json_references_worktree_path(value, worktree_path));
        let result_matches = result_json
            .as_ref()
            .is_some_and(|value| json_references_worktree_path(value, worktree_path));

        if context_matches || result_matches {
            let card_label = card_id.as_deref().unwrap_or("<null-card>");
            refs.push(format!("{card_label}/{dispatch_id}"));
        }
    }

    let rows = sqlx::query(
        "SELECT card_id, worktree_path
         FROM pr_tracking
         WHERE NULLIF(BTRIM(worktree_path), '') IS NOT NULL",
    )
    .fetch_all(pg_pool)
    .await
    .map_err(|error| {
        anyhow::anyhow!(
            "load active pr_tracking worktree references for {cleanup_dispatch_id}: {error}"
        )
    })?;

    for row in rows {
        let card_id: String = row.try_get("card_id").map_err(|error| {
            anyhow::anyhow!("decode pr_tracking card id for {cleanup_dispatch_id}: {error}")
        })?;
        let tracked_path: String = row.try_get("worktree_path").map_err(|error| {
            anyhow::anyhow!("decode pr_tracking worktree path for {card_id}: {error}")
        })?;
        if paths_match(&tracked_path, worktree_path) {
            refs.push(format!("pr_tracking/{card_id}"));
        }
    }

    Ok(refs)
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

fn json_references_worktree_path(value: &serde_json::Value, worktree_path: &str) -> bool {
    WORKTREE_PATH_REFERENCE_KEYS.iter().any(|key| {
        json_string_field(value, key)
            .as_deref()
            .is_some_and(|candidate| paths_match(candidate, worktree_path))
    })
}

fn paths_match(left: &str, right: &str) -> bool {
    let left = left.trim().trim_end_matches('/');
    let right = right.trim().trim_end_matches('/');
    if left == right {
        return true;
    }

    let left_canonical = std::fs::canonicalize(left).ok();
    let right_canonical = std::fs::canonicalize(right).ok();
    matches!(
        (left_canonical, right_canonical),
        (Some(left), Some(right)) if left == right
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

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

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_managed_worktree_cleanup_skips_path_referenced_by_other_active_card() {
        let fixture = setup_managed_worktree_cleanup_fixture(
            "card-terminal-owner",
            "dispatch-terminal-owner",
            "shared-worktree",
        )
        .await;
        seed_card_pg(&fixture.pool, "card-active-owner", "in_progress").await;

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES (
                'dispatch-active-shared', 'card-active-owner', 'agent-1', 'implementation',
                'dispatched', 'active shared', $1, NOW(), NOW()
             )",
        )
        .bind(json!({"worktree_path": fixture.worktree_path.to_string_lossy()}).to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();

        let summary = cleanup_terminal_managed_worktrees_pg(&fixture.pool, "card-terminal-owner")
            .await
            .unwrap();

        assert_eq!(
            summary.removed, 0,
            "shared active worktree reference must prevent removal"
        );
        assert!(
            fixture.worktree_path.exists(),
            "worktree referenced by another active card must remain on disk"
        );

        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_managed_worktree_cleanup_skips_null_card_active_result_reference() {
        let fixture = setup_managed_worktree_cleanup_fixture(
            "card-null-owner",
            "dispatch-null-owner",
            "null-card-worktree",
        )
        .await;

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at
             ) VALUES (
                'dispatch-active-null-card', NULL, 'agent-1', 'implementation',
                'dispatched', 'active null card', $1, NOW(), NOW()
             )",
        )
        .bind(
            json!({"completed_worktree_path": fixture.worktree_path.to_string_lossy()}).to_string(),
        )
        .execute(&fixture.pool)
        .await
        .unwrap();

        let summary = cleanup_terminal_managed_worktrees_pg(&fixture.pool, "card-null-owner")
            .await
            .unwrap();

        assert_eq!(
            summary.removed, 0,
            "active NULL-card result reference must prevent removal"
        );
        assert!(
            fixture.worktree_path.exists(),
            "worktree referenced by active NULL-card dispatch must remain on disk"
        );

        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_managed_worktree_cleanup_skips_same_card_preserved_review_dispatch() {
        let fixture = setup_managed_worktree_cleanup_fixture(
            "card-preserved-review-owner",
            "dispatch-preserved-review-owner",
            "preserved-review-worktree",
        )
        .await;

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
             ) VALUES (
                'dispatch-preserved-review-active', 'card-preserved-review-owner', 'agent-1',
                'review', 'pending', 'preserved review', $1, NOW(), NOW()
             )",
        )
        .bind(json!({"worktree_path": fixture.worktree_path.to_string_lossy()}).to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();

        let summary =
            cleanup_terminal_managed_worktrees_pg(&fixture.pool, "card-preserved-review-owner")
                .await
                .unwrap();

        assert_eq!(
            summary.removed, 0,
            "same-card preserved active review dispatch must prevent removal"
        );
        assert!(
            fixture.worktree_path.exists(),
            "worktree referenced by same-card preserved review must remain on disk"
        );

        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_managed_worktree_cleanup_skips_pr_tracking_worktree_path_reference() {
        let fixture = setup_managed_worktree_cleanup_fixture(
            "card-pr-tracking-owner",
            "dispatch-pr-tracking-owner",
            "pr-tracking-worktree",
        )
        .await;
        seed_card_pg(&fixture.pool, "card-pr-tracking-active", "in_progress").await;

        sqlx::query(
            "INSERT INTO pr_tracking (
                card_id, worktree_path, state, created_at, updated_at
             ) VALUES (
                'card-pr-tracking-active', $1, 'create-pr', NOW(), NOW()
             )",
        )
        .bind(fixture.worktree_path.to_string_lossy().to_string())
        .execute(&fixture.pool)
        .await
        .unwrap();

        let summary =
            cleanup_terminal_managed_worktrees_pg(&fixture.pool, "card-pr-tracking-owner")
                .await
                .unwrap();

        assert_eq!(
            summary.removed, 0,
            "pr_tracking.worktree_path reference must prevent removal"
        );
        assert!(
            fixture.worktree_path.exists(),
            "worktree referenced by pr_tracking must remain on disk"
        );

        fixture.close().await;
    }

    struct ManagedWorktreeCleanupFixture {
        pg_db: KanbanPgDatabase,
        pool: sqlx::PgPool,
        worktree_path: PathBuf,
        _env_lock: std::sync::MutexGuard<'static, ()>,
        _env_guard: EnvVarGuard,
        _runtime_root: tempfile::TempDir,
        _repo: tempfile::TempDir,
    }

    impl ManagedWorktreeCleanupFixture {
        async fn close(self) {
            let Self { pg_db, pool, .. } = self;
            pg_db.close_pool_and_drop(pool).await;
        }
    }

    async fn setup_managed_worktree_cleanup_fixture(
        owner_card_id: &str,
        owner_dispatch_id: &str,
        worktree_name: &str,
    ) -> ManagedWorktreeCleanupFixture {
        let env_lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = tempfile::tempdir().unwrap();
        let env_guard =
            EnvVarGuard::set("AGENTDESK_ROOT_DIR", runtime_root.path().to_str().unwrap());
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());

        let repo_name = repo.path().file_name().unwrap().to_string_lossy();
        let worktree_parent = runtime_root
            .path()
            .join("worktrees")
            .join(repo_name.as_ref());
        std::fs::create_dir_all(&worktree_parent).unwrap();
        let worktree_path = worktree_parent.join(worktree_name);
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--detach",
                worktree_path.to_str().unwrap(),
                "main",
            ],
        );

        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_card_with_repo_pg(&pool, owner_card_id, "done", repo.path().to_str().unwrap()).await;
        seed_completed_managed_worktree_dispatch_pg(
            &pool,
            owner_card_id,
            owner_dispatch_id,
            &worktree_path,
        )
        .await;

        ManagedWorktreeCleanupFixture {
            pg_db,
            pool,
            worktree_path,
            _env_lock: env_lock,
            _env_guard: env_guard,
            _runtime_root: runtime_root,
            _repo: repo,
        }
    }

    async fn seed_completed_managed_worktree_dispatch_pg(
        pool: &sqlx::PgPool,
        card_id: &str,
        dispatch_id: &str,
        worktree_path: &Path,
    ) {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at, completed_at
             ) VALUES (
                $1, $2, 'agent-1', 'implementation', 'completed', 'completed owner',
                $3, NOW(), NOW(), NOW()
             )",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind(
            json!({
                "managed_worktree": true,
                "managed_worktree_cleanup": "terminal",
                "worktree_path": worktree_path.to_string_lossy()
            })
            .to_string(),
        )
        .execute(pool)
        .await
        .unwrap();
    }

    fn init_git_repo(repo_dir: &Path) {
        run_git(repo_dir, &["init", "-b", "main"]);
        run_git(repo_dir, &["config", "user.email", "test@test.com"]);
        run_git(repo_dir, &["config", "user.name", "Test"]);
        std::fs::write(repo_dir.join("README.md"), "test\n").unwrap();
        run_git(repo_dir, &["add", "README.md"]);
        run_git(repo_dir, &["commit", "-m", "initial"]);
    }

    fn run_git(repo_dir: &Path, args: &[&str]) {
        let output = crate::services::git::git_command()
            .args(args)
            .current_dir(repo_dir)
            .output()
            .unwrap_or_else(|error| panic!("spawn git {args:?}: {error}"));
        assert!(
            output.status.success(),
            "git {:?} failed: {}\n{}",
            args,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
