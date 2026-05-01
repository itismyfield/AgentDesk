use anyhow::{Result, anyhow};
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;

use crate::{db::Db, engine::PolicyEngine};

/// Hard cutoff for "stale inflight" detection in the periodic reconcile.
/// Anything older than this with no live tmux pane is considered abandoned.
/// #1076 (905-7): zombie resource sweep cadence.
const STALE_INFLIGHT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Hard cutoff for orphan `discord_uploads/<channel>/*` files. 7 days matches
/// the default retention hint used by `settings/content.rs::cleanup_old_uploads`
/// for manually-aged attachments, so the periodic sweep is a strict superset.
const STALE_UPLOAD_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootReconcileStats {
    pub stale_processing_outbox_reset: usize,
    pub stale_dispatch_reservations_cleared: usize,
    pub missing_notify_outbox_backfilled: usize,
    pub broken_auto_queue_entries_reset: usize,
    pub stale_channel_thread_map_entries_cleared: usize,
    pub missing_review_dispatches_refired: usize,
}

impl BootReconcileStats {
    pub(crate) fn touched(&self) -> bool {
        self.stale_processing_outbox_reset > 0
            || self.stale_dispatch_reservations_cleared > 0
            || self.missing_notify_outbox_backfilled > 0
            || self.broken_auto_queue_entries_reset > 0
            || self.stale_channel_thread_map_entries_cleared > 0
            || self.missing_review_dispatches_refired > 0
    }
}

pub(crate) async fn reconcile_boot_db_pg(pool: &PgPool) -> Result<BootReconcileStats> {
    // Touch next_attempt_at so oldest_pending_age reflects "re-queued at boot",
    // not the original created_at. Without this, rows that were stuck in
    // 'processing' across a restart show up as multi-minute-aged pending rows
    // and the promote health gate fails even though the outbox worker picks
    // them up on the next tick.
    let stale_processing_outbox_reset = sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'pending',
                next_attempt_at = NOW()
          WHERE status = 'processing'",
    )
    .execute(pool)
    .await
    .map(|r| r.rows_affected() as usize)
    .unwrap_or(0);

    let stale_dispatch_reservations_cleared =
        sqlx::query("DELETE FROM kv_meta WHERE key LIKE 'dispatch_reserving:%'")
            .execute(pool)
            .await
            .map(|r| r.rows_affected() as usize)
            .unwrap_or(0);

    let missing_notify_outbox_backfilled = backfill_missing_notify_outbox_pg(pool).await?;
    let broken_auto_queue_entries_reset = reset_broken_auto_queue_entries_pg(pool).await?;

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        stale_channel_thread_map_entries_cleared: 0,
        missing_review_dispatches_refired: 0,
    })
}

pub(crate) async fn reconcile_boot_runtime(
    db: Option<&Db>,
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
) -> Result<BootReconcileStats> {
    let mut stats = if let Some(pool) = pg_pool {
        reconcile_boot_db_pg(pool).await?
    } else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            reconcile_boot_db_sqlite(
                db.ok_or_else(|| anyhow!("SQLite db required for test boot reconcile"))?,
            )?
        }
        #[cfg(not(feature = "legacy-sqlite-tests"))]
        {
            return Err(anyhow!("Postgres pool required for boot reconcile"));
        }
    };

    stats.missing_review_dispatches_refired = if let Some(pool) = pg_pool {
        refire_missing_review_dispatches_pg(pool, db, engine).await?
    } else {
        0
    };

    if stats.touched() {
        tracing::info!(
            "[boot-reconcile] reset_processing={} cleared_reservations={} missing_notify={} broken_auto_queue={} cleared_thread_map={} refired_review={}",
            stats.stale_processing_outbox_reset,
            stats.stale_dispatch_reservations_cleared,
            stats.missing_notify_outbox_backfilled,
            stats.broken_auto_queue_entries_reset,
            stats.stale_channel_thread_map_entries_cleared,
            stats.missing_review_dispatches_refired
        );
    }

    Ok(stats)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn reconcile_boot_db_sqlite(db: &Db) -> Result<BootReconcileStats> {
    let conn = db
        .separate_conn()
        .map_err(|error| anyhow!("open sqlite boot reconcile connection: {error}"))?;
    let stale_processing_outbox_reset = conn
        .execute(
            "UPDATE dispatch_outbox SET status = 'pending' WHERE status = 'processing'",
            [],
        )
        .unwrap_or(0);
    let stale_dispatch_reservations_cleared = conn
        .execute(
            "DELETE FROM kv_meta WHERE key LIKE 'dispatch_reserving:%'",
            [],
        )
        .unwrap_or(0);
    let missing_notify_outbox_backfilled = conn
        .execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status)
             SELECT td.id, 'notify', td.to_agent_id, td.kanban_card_id, td.title, 'pending'
             FROM task_dispatches td
             WHERE td.status IN ('pending', 'dispatched')
               AND NOT EXISTS (
                 SELECT 1 FROM dispatch_outbox o
                 WHERE o.dispatch_id = td.id AND o.action = 'notify'
               )",
            [],
        )
        .map_err(|error| anyhow!("backfill sqlite missing notify outbox: {error}"))?;
    let broken_auto_queue_entries_reset = conn
        .execute(
            "UPDATE auto_queue_entries
             SET status = 'pending',
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = NULL
             WHERE status = 'dispatched'
               AND (
                 dispatch_id IS NULL
                 OR TRIM(dispatch_id) = ''
                 OR NOT EXISTS (
                   SELECT 1
                   FROM task_dispatches td
                   WHERE td.id = auto_queue_entries.dispatch_id
                     AND td.status NOT IN ('cancelled', 'failed', 'completed')
                 )
               )",
            [],
        )
        .map_err(|error| anyhow!("reset sqlite broken auto-queue entries: {error}"))?;

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        stale_channel_thread_map_entries_cleared: 0,
        missing_review_dispatches_refired: 0,
    })
}

async fn backfill_missing_notify_outbox_pg(pool: &PgPool) -> Result<usize> {
    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, status, required_capabilities
         )
         SELECT
            td.id,
            'notify',
            td.to_agent_id,
            td.kanban_card_id,
            td.title,
            'pending',
            td.required_capabilities
         FROM task_dispatches td
         WHERE td.status IN ('pending', 'dispatched')
           AND NOT EXISTS (
             SELECT 1 FROM dispatch_outbox o
             WHERE o.dispatch_id = td.id AND o.action = 'notify'
           )
         ON CONFLICT (dispatch_id, action) WHERE action IN ('notify', 'followup')
         DO NOTHING",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(anyhow::Error::from)
}

async fn reset_broken_auto_queue_entries_pg(pool: &PgPool) -> Result<usize> {
    sqlx::query(
        "UPDATE auto_queue_entries e
         SET status = 'pending',
             dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = NULL
         WHERE e.status = 'dispatched'
           AND (
             e.dispatch_id IS NULL
             OR TRIM(e.dispatch_id) = ''
             OR NOT EXISTS (
               SELECT 1
               FROM task_dispatches td
               WHERE td.id = e.dispatch_id
                 AND td.status NOT IN ('cancelled', 'failed', 'completed')
             )
           )",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(anyhow::Error::from)
}

async fn refire_missing_review_dispatches_pg(
    pool: &PgPool,
    db: Option<&Db>,
    engine: &PolicyEngine,
) -> Result<usize> {
    crate::pipeline::ensure_loaded();

    let cards: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, status, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE status NOT IN ('done', 'backlog', 'ready')",
    )
    .fetch_all(pool)
    .await?;

    let mut candidates = Vec::new();
    for (card_id, status, repo_id, agent_id) in cards {
        let effective =
            crate::pipeline::resolve_for_card_pg(pool, repo_id.as_deref(), agent_id.as_deref())
                .await;
        let is_review_state = effective.hooks_for_state(&status).map_or(false, |hooks| {
            hooks.on_enter.iter().any(|name| name == "OnReviewEnter")
        });
        if !is_review_state {
            continue;
        }

        let has_review_dispatch = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM task_dispatches
                WHERE kanban_card_id = $1
                  AND dispatch_type IN ('review', 'review-decision')
                  AND status IN ('pending', 'dispatched')
            )",
        )
        .bind(&card_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if !has_review_dispatch {
            candidates.push(card_id);
        }
    }

    let mut refired = 0usize;
    for card_id in candidates {
        if let Err(e) =
            engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": card_id }))
        {
            tracing::warn!(
                "[boot-reconcile] failed to re-fire OnReviewEnter for card {}: {e}",
                card_id
            );
            continue;
        }
        crate::kanban::drain_hook_side_effects_with_backends(db, engine);

        let has_review_dispatch = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM task_dispatches
                WHERE kanban_card_id = $1
                  AND dispatch_type IN ('review', 'review-decision')
                  AND status IN ('pending', 'dispatched')
            )",
        )
        .bind(&card_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if has_review_dispatch {
            refired += 1;
        } else {
            tracing::warn!(
                "[boot-reconcile] OnReviewEnter re-fired for card {} but no active review dispatch was created",
                card_id
            );
        }
    }

    Ok(refired)
}

// ============================================================================
// #1076 (905-7): zombie resource sweep — periodic reconcile
// ============================================================================
//
// Four zombie classes are covered by `reconcile_zombie_resources()`:
//
//   1. Orphan tmux sessions (AgentDesk-* with no owning channel entry AND no
//      live pane) — already handled on boot by `cleanup_orphan_tmux_sessions`;
//      the periodic path re-checks hourly so long-running processes do not
//      accumulate leaks between restarts. The periodic path is a no-op unless
//      a live `SharedData` was registered via `register_shared_runtime_handle`.
//   2. Stale inflight state files (> `STALE_INFLIGHT_MAX_AGE` and restart_mode
//      is None, i.e. never planned for resume) — deletes the JSON file so the
//      next boot does not try to resume an abandoned turn.
//   3. Zombie DashMap entries (intake dedupe / api_timestamps / tmux_relay_coords
//      growing unboundedly when channels disappear). The sweep trims any entry
//      whose key channel no longer has a matching live tmux session + no
//      active inflight.
//   4. Unrelocated `discord_uploads/<channel>/*` files older than
//      `STALE_UPLOAD_MAX_AGE` — the Discord upload content-addressed mirror
//      migrated off disk-per-channel but legacy files can linger when a
//      migration step aborts.
//
// Each helper returns a count, aggregated into `ZombieReconcileStats` for
// log emission. The callers must tolerate Postgres / SQLite / tmux being
// unavailable — all helpers degrade gracefully (zero count + warn log).

/// Aggregate stats from one run of [`reconcile_zombie_resources`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZombieReconcileStats {
    pub orphan_tmux_killed: usize,
    pub stale_inflight_removed: usize,
    pub zombie_dashmap_trimmed: usize,
    pub stale_uploads_removed: usize,
}

impl ZombieReconcileStats {
    pub(crate) fn total(&self) -> usize {
        self.orphan_tmux_killed
            + self.stale_inflight_removed
            + self.zombie_dashmap_trimmed
            + self.stale_uploads_removed
    }
}

/// Remove inflight state JSON files older than [`STALE_INFLIGHT_MAX_AGE`] that
/// have no `restart_mode` assignment (i.e. were never scheduled for resume).
/// Returns the number of files deleted. Safe to call without a Postgres pool.
pub(crate) fn sweep_stale_inflight_files() -> usize {
    let Some(root) =
        crate::config::runtime_root().map(|p| p.join("runtime").join("discord_inflight"))
    else {
        return 0;
    };
    sweep_stale_inflight_files_at(&root, STALE_INFLIGHT_MAX_AGE)
}

pub(crate) fn sweep_stale_inflight_files_at(root: &std::path::Path, max_age: Duration) -> usize {
    use std::fs;
    use std::time::SystemTime;

    if !root.exists() {
        return 0;
    }

    let Ok(provider_dirs) = fs::read_dir(root) else {
        return 0;
    };

    let now = SystemTime::now();
    let mut removed = 0usize;

    for provider in provider_dirs.filter_map(|e| e.ok()) {
        let pdir = provider.path();
        if !pdir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&pdir) else {
            continue;
        };
        for entry in files.filter_map(|e| e.ok()) {
            let fpath = entry.path();
            if !fpath.is_file() {
                continue;
            }
            // Only consider .json state files — skip anything unexpected.
            if fpath.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let age_ok = fs::metadata(&fpath)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);
            if !age_ok {
                continue;
            }
            // Only remove when restart_mode is absent. A file with a restart_mode
            // set is owned by a planned lifecycle (drain/hot-swap); the existing
            // inflight retention helpers cover those.
            let restart_mode_present = fs::read_to_string(&fpath)
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|v| {
                    v.get("restart_mode")
                        .filter(|rm| !rm.is_null())
                        .map(|_| true)
                })
                .unwrap_or(false);
            if restart_mode_present {
                continue;
            }
            if fs::remove_file(&fpath).is_ok() {
                removed += 1;
                tracing::info!(
                    target: "reconcile",
                    path = %fpath.display(),
                    "[zombie-reconcile] removed stale inflight state file"
                );
            }
        }
    }
    removed
}

/// Remove `discord_uploads/<channel>/*` files older than
/// [`STALE_UPLOAD_MAX_AGE`]. Returns the number of files removed.
pub(crate) fn sweep_stale_discord_uploads() -> usize {
    let Some(root) =
        crate::config::runtime_root().map(|p| p.join("runtime").join("discord_uploads"))
    else {
        return 0;
    };
    sweep_stale_discord_uploads_at(&root, STALE_UPLOAD_MAX_AGE)
}

pub(crate) fn sweep_stale_discord_uploads_at(root: &std::path::Path, max_age: Duration) -> usize {
    use std::fs;
    use std::time::SystemTime;

    if !root.exists() {
        return 0;
    }
    let Ok(channels) = fs::read_dir(root) else {
        return 0;
    };

    let now = SystemTime::now();
    let mut removed = 0usize;

    for ch in channels.filter_map(|e| e.ok()) {
        let ch_path = ch.path();
        if !ch_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&ch_path) else {
            continue;
        };
        for entry in files.filter_map(|e| e.ok()) {
            let fpath = entry.path();
            if !fpath.is_file() {
                continue;
            }
            let stale = fs::metadata(&fpath)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);
            if stale && fs::remove_file(&fpath).is_ok() {
                removed += 1;
            }
        }
        // Drop the channel dir if now empty.
        if fs::read_dir(&ch_path)
            .ok()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&ch_path);
        }
    }
    removed
}

/// Run the full zombie sweep (stale inflight + stale uploads).
///
/// The tmux orphan cleanup + DashMap trim require a live `Arc<SharedData>`
/// handle which is owned by the Discord bot runtime; those two counters are
/// filled in by the Discord-side runtime loop in
/// `services/discord/mod.rs::run_discord_zombie_sweep_tick`. The periodic
/// maintenance job records whatever the file-system and PG layers can do
/// without the Discord runtime handle, which means it is safe to run before
/// (and independently of) the bot coming up.
pub(crate) async fn reconcile_zombie_resources() -> ZombieReconcileStats {
    let stale_inflight_removed = tokio::task::spawn_blocking(sweep_stale_inflight_files)
        .await
        .unwrap_or(0);
    let stale_uploads_removed = tokio::task::spawn_blocking(sweep_stale_discord_uploads)
        .await
        .unwrap_or(0);

    let stats = ZombieReconcileStats {
        orphan_tmux_killed: 0,
        stale_inflight_removed,
        zombie_dashmap_trimmed: 0,
        stale_uploads_removed,
    };

    if stats.total() > 0 {
        tracing::info!(
            target: "reconcile",
            orphan_tmux = stats.orphan_tmux_killed,
            stale_inflight = stats.stale_inflight_removed,
            zombie_dashmap = stats.zombie_dashmap_trimmed,
            stale_uploads = stats.stale_uploads_removed,
            "[zombie-reconcile] sweep completed"
        );
    } else {
        tracing::debug!(
            target: "reconcile",
            "[zombie-reconcile] sweep completed (no zombies found)"
        );
    }

    stats
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // #1076 (905-7): zombie reconcile sweep tests
    // ------------------------------------------------------------------

    #[test]
    fn zombie_sweep_removes_old_inflight_files_without_restart_mode() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("claude");
        fs::create_dir_all(&provider_dir).unwrap();

        // File with restart_mode = null -> must be removed once max_age=0.
        let stale = provider_dir.join("stale.json");
        fs::write(
            &stale,
            "{\"channel_id\":1,\"restart_mode\":null,\"updated_at\":\"x\"}",
        )
        .unwrap();

        // File WITH restart_mode -> must be preserved even when max_age=0.
        let planned = provider_dir.join("planned.json");
        fs::write(
            &planned,
            "{\"channel_id\":2,\"restart_mode\":\"DrainRestart\",\"updated_at\":\"x\"}",
        )
        .unwrap();

        // Non-json file -> ignored.
        let stray = provider_dir.join("junk.tmp");
        fs::write(&stray, "nope").unwrap();

        // max_age = 0 -> every file is "stale" by age, so the restart_mode
        // branch is the only thing protecting `planned.json`.
        let removed = sweep_stale_inflight_files_at(tmp.path(), Duration::from_secs(0));
        assert_eq!(
            removed, 1,
            "only the stale unplanned file should be removed"
        );
        assert!(!stale.exists(), "stale file must be gone");
        assert!(planned.exists(), "planned-restart file must survive");
        assert!(stray.exists(), "non-json files must be ignored");
    }

    #[test]
    fn zombie_sweep_preserves_everything_when_max_age_is_far_in_future() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("codex");
        fs::create_dir_all(&provider_dir).unwrap();
        let a = provider_dir.join("a.json");
        fs::write(&a, "{\"restart_mode\":null}").unwrap();
        let removed =
            sweep_stale_inflight_files_at(tmp.path(), Duration::from_secs(365 * 24 * 60 * 60));
        assert_eq!(removed, 0);
        assert!(a.exists());
    }

    #[test]
    fn zombie_sweep_removes_stale_discord_uploads() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let channel_dir = tmp.path().join("999");
        fs::create_dir_all(&channel_dir).unwrap();
        let f = channel_dir.join("old.png");
        fs::write(&f, b"old").unwrap();

        // max_age = 0 -> file qualifies as stale.
        let removed = sweep_stale_discord_uploads_at(tmp.path(), Duration::from_secs(0));
        assert_eq!(removed, 1);
        assert!(!f.exists());
        // The empty channel dir is pruned.
        assert!(!channel_dir.exists());
    }

    #[test]
    fn zombie_stats_total_sums_all_buckets() {
        let stats = ZombieReconcileStats {
            orphan_tmux_killed: 1,
            stale_inflight_removed: 2,
            zombie_dashmap_trimmed: 3,
            stale_uploads_removed: 4,
        };
        assert_eq!(stats.total(), 10);
    }
}
