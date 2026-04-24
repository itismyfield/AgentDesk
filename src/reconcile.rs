use anyhow::{Result, anyhow};
use libsql_rusqlite::Connection;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;

use crate::db::agents::load_agent_channel_bindings;
use crate::{db::Db, dispatch, engine::PolicyEngine};

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

pub(crate) fn reconcile_boot_db(conn: &Connection) -> Result<BootReconcileStats> {
    let stale_processing_outbox_reset = conn
        .execute(
            "UPDATE dispatch_outbox SET status = 'pending' WHERE status = 'processing'",
            [],
        )
        .unwrap_or(0) as usize;
    let stale_dispatch_reservations_cleared = conn
        .execute(
            "DELETE FROM kv_meta WHERE key LIKE 'dispatch_reserving:%'",
            [],
        )
        .unwrap_or(0) as usize;

    let missing_notify_outbox_backfilled = backfill_missing_notify_outbox(conn)?;
    let broken_auto_queue_entries_reset = reset_broken_auto_queue_entries(conn)?;
    let stale_channel_thread_map_entries_cleared = cleanup_stale_channel_thread_map_entries(conn)?;

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        stale_channel_thread_map_entries_cleared,
        missing_review_dispatches_refired: 0,
    })
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

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled: 0,
        broken_auto_queue_entries_reset: 0,
        stale_channel_thread_map_entries_cleared: 0,
        missing_review_dispatches_refired: 0,
    })
}

pub(crate) async fn reconcile_boot_runtime(
    db: &Db,
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
) -> Result<BootReconcileStats> {
    let mut stats = if let Some(pool) = pg_pool {
        reconcile_boot_db_pg(pool).await?
    } else {
        let conn = db
            .lock()
            .map_err(|e| anyhow!("boot reconcile DB lock poisoned: {e}"))?;
        reconcile_boot_db(&conn)?
    };

    stats.missing_review_dispatches_refired = refire_missing_review_dispatches(db, engine)?;

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

fn backfill_missing_notify_outbox(conn: &Connection) -> Result<usize> {
    let missing_rows: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT td.id, td.to_agent_id, td.kanban_card_id, td.title
             FROM task_dispatches td
             WHERE td.status IN ('pending', 'dispatched')
               AND NOT EXISTS (
                 SELECT 1 FROM dispatch_outbox o
                 WHERE o.dispatch_id = td.id AND o.action = 'notify'
               )",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut inserted = 0usize;
    for (dispatch_id, agent_id, card_id, title) in missing_rows {
        if dispatch::ensure_dispatch_notify_outbox_on_conn(
            conn,
            &dispatch_id,
            &agent_id,
            &card_id,
            &title,
        )? {
            inserted += 1;
        }
    }
    Ok(inserted)
}

fn reset_broken_auto_queue_entries(conn: &Connection) -> Result<usize> {
    let broken_ids: Vec<String> = conn
        .prepare(
            "SELECT e.id
             FROM auto_queue_entries e
             LEFT JOIN task_dispatches td ON td.id = e.dispatch_id
             WHERE e.status = 'dispatched'
               AND (
                 e.dispatch_id IS NULL
                 OR TRIM(e.dispatch_id) = ''
                 OR td.id IS NULL
                 OR td.status IN ('cancelled', 'failed', 'completed')
               )",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut reset = 0usize;
    for entry_id in broken_ids {
        if crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_PENDING,
            "boot_reconcile",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .map_err(|error| match error {
            crate::db::auto_queue::EntryStatusUpdateError::Sql(sql) => anyhow!(sql),
            other => anyhow!(other.to_string()),
        })?
        .changed
        {
            reset += 1;
        }
    }
    Ok(reset)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadMapValidation {
    Valid,
    Stale,
    Unknown,
}

fn cleanup_stale_channel_thread_map_entries(conn: &Connection) -> Result<usize> {
    let token = crate::credential::read_bot_token("announce");
    cleanup_stale_channel_thread_map_entries_with(conn, |channel_id, thread_id| {
        validate_thread_parent_via_discord(token.as_deref(), channel_id, thread_id)
    })
}

fn cleanup_stale_channel_thread_map_entries_with<F>(
    conn: &Connection,
    mut validate_thread: F,
) -> Result<usize>
where
    F: FnMut(&str, &str) -> ThreadMapValidation,
{
    let rows: Vec<(String, Option<String>, Option<String>, String)> = conn
        .prepare(
            "SELECT id, assigned_agent_id, active_thread_id, channel_thread_map
             FROM kanban_cards
             WHERE channel_thread_map IS NOT NULL
               AND TRIM(channel_thread_map) != ''",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut cleared = 0usize;
    for (card_id, assigned_agent_id, active_thread_id, raw_map) in rows {
        let parsed = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw_map);
        let Ok(map) = parsed else {
            tracing::warn!(
                "[boot-reconcile] clearing malformed channel_thread_map for card {}",
                card_id
            );
            conn.execute(
                "UPDATE kanban_cards
                 SET channel_thread_map = NULL, active_thread_id = NULL
                 WHERE id = ?1",
                [&card_id],
            )?;
            cleared += 1;
            continue;
        };

        let allowed_channels = assigned_agent_id
            .as_deref()
            .and_then(|agent_id| load_agent_channel_bindings(conn, agent_id).ok().flatten())
            .map(|bindings| bindings.all_channels())
            .unwrap_or_default();
        let enforce_channel_bindings = !allowed_channels.is_empty();

        let mut kept = serde_json::Map::new();
        let mut removed_for_card = 0usize;
        for (channel_id, value) in map {
            let Some(thread_id) = value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
            else {
                removed_for_card += 1;
                continue;
            };

            if enforce_channel_bindings
                && !allowed_channels.iter().any(|bound| bound == &channel_id)
            {
                removed_for_card += 1;
                continue;
            }

            match validate_thread(&channel_id, &thread_id) {
                ThreadMapValidation::Valid | ThreadMapValidation::Unknown => {
                    kept.insert(channel_id, serde_json::Value::String(thread_id));
                }
                ThreadMapValidation::Stale => {
                    removed_for_card += 1;
                }
            }
        }

        let new_active_thread_id = active_thread_id
            .as_ref()
            .filter(|active| {
                kept.values()
                    .any(|value| value.as_str() == Some(active.as_str()))
            })
            .cloned()
            .or_else(|| {
                kept.values()
                    .find_map(|value| value.as_str().map(str::to_string))
            });
        let new_map_json = if kept.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&kept)?)
        };
        let map_changed = removed_for_card > 0
            || new_map_json.as_deref() != Some(raw_map.as_str())
            || new_active_thread_id != active_thread_id;
        if map_changed {
            conn.execute(
                "UPDATE kanban_cards
                 SET channel_thread_map = ?1, active_thread_id = ?2
                 WHERE id = ?3",
                libsql_rusqlite::params![new_map_json, new_active_thread_id, card_id],
            )?;
        }
        cleared += removed_for_card;
    }

    Ok(cleared)
}

fn validate_thread_parent_via_discord(
    token: Option<&str>,
    expected_parent: &str,
    thread_id: &str,
) -> ThreadMapValidation {
    let Some(token) = token.map(str::trim).filter(|s| !s.is_empty()) else {
        return ThreadMapValidation::Unknown;
    };

    let url = format!("https://discord.com/api/v10/channels/{thread_id}");
    let resp = match ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .get(&url)
        .set("Authorization", &format!("Bot {token}"))
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(404, _)) => return ThreadMapValidation::Stale,
        Err(ureq::Error::Status(status, _)) => {
            tracing::warn!(
                "[boot-reconcile] thread validation skipped for {thread_id}: HTTP {status}"
            );
            return ThreadMapValidation::Unknown;
        }
        Err(err) => {
            tracing::warn!(
                "[boot-reconcile] thread validation request failed for {thread_id}: {err}"
            );
            return ThreadMapValidation::Unknown;
        }
    };

    let body: serde_json::Value = match resp.into_json() {
        Ok(body) => body,
        Err(err) => {
            tracing::warn!(
                "[boot-reconcile] thread validation decode failed for {thread_id}: {err}"
            );
            return ThreadMapValidation::Unknown;
        }
    };

    match body.get("parent_id").and_then(|value| value.as_str()) {
        Some(parent_id) if parent_id == expected_parent => ThreadMapValidation::Valid,
        _ => ThreadMapValidation::Stale,
    }
}

fn refire_missing_review_dispatches(db: &Db, engine: &PolicyEngine) -> Result<usize> {
    crate::pipeline::ensure_loaded();

    let candidates: Vec<String> = {
        let conn = db
            .lock()
            .map_err(|e| anyhow!("boot reconcile DB lock poisoned: {e}"))?;
        let cards: Vec<(String, String, Option<String>, Option<String>)> = conn
            .prepare(
                "SELECT id, status, repo_id, assigned_agent_id
                 FROM kanban_cards
                 WHERE status NOT IN ('done', 'backlog', 'ready')",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut candidates = Vec::new();
        for (card_id, status, repo_id, agent_id) in cards {
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            let is_review_state = effective.hooks_for_state(&status).map_or(false, |hooks| {
                hooks.on_enter.iter().any(|name| name == "OnReviewEnter")
            });
            if !is_review_state {
                continue;
            }

            let has_review_dispatch: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM task_dispatches
                     WHERE kanban_card_id = ?1
                       AND dispatch_type IN ('review', 'review-decision')
                       AND status IN ('pending', 'dispatched')",
                    [&card_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if !has_review_dispatch {
                candidates.push(card_id);
            }
        }
        candidates
    };

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
        crate::kanban::drain_hook_side_effects(db, engine);

        let has_review_dispatch = if let Some(pool) = engine.pg_pool() {
            let card_id_pg = card_id.clone();
            crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    sqlx::query_scalar::<_, bool>(
                        "SELECT COUNT(*) > 0 FROM task_dispatches
                         WHERE kanban_card_id = $1
                           AND dispatch_type IN ('review', 'review-decision')
                           AND status IN ('pending', 'dispatched')",
                    )
                    .bind(&card_id_pg)
                    .fetch_one(&bridge_pool)
                    .await
                    .map_err(anyhow::Error::from)
                },
                |error| anyhow!(error),
            )
            .unwrap_or(false)
        } else {
            db.lock()
                .map_err(|e| anyhow!("boot reconcile DB lock poisoned: {e}"))?
                .query_row(
                    "SELECT COUNT(*) > 0 FROM task_dispatches
                     WHERE kanban_card_id = ?1
                       AND dispatch_type IN ('review', 'review-decision')
                       AND status IN ('pending', 'dispatched')",
                    [&card_id],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> libsql_rusqlite::Connection {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn boot_reconcile_thread_map_cleanup_removes_stale_entries_and_normalizes_active_thread() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '111', '222')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, channel_thread_map, active_thread_id,
                created_at, updated_at
             ) VALUES (
                'card-thread-clean', 'Thread Clean', 'in_progress', 'agent-1',
                '{\"111\":\"thread-ok\",\"222\":\"thread-mismatch\",\"333\":\"thread-offbind\",\"444\":123}',
                'thread-mismatch', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, channel_thread_map, active_thread_id,
                created_at, updated_at
             ) VALUES (
                'card-thread-gone', 'Thread Gone', 'review', 'agent-1',
                '{\"111\":\"thread-gone\"}', 'thread-gone', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();

        let cleared =
            cleanup_stale_channel_thread_map_entries_with(&conn, |channel_id, thread_id| {
                match (channel_id, thread_id) {
                    ("111", "thread-ok") => ThreadMapValidation::Valid,
                    ("222", "thread-mismatch") => ThreadMapValidation::Stale,
                    ("111", "thread-gone") => ThreadMapValidation::Stale,
                    _ => ThreadMapValidation::Unknown,
                }
            })
            .unwrap();

        assert_eq!(cleared, 4);

        let (map, active_thread_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards WHERE id = 'card-thread-clean'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(map.as_deref(), Some("{\"111\":\"thread-ok\"}"));
        assert_eq!(active_thread_id.as_deref(), Some("thread-ok"));

        let (gone_map, gone_active): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards WHERE id = 'card-thread-gone'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(gone_map.is_none());
        assert!(gone_active.is_none());
    }

    #[test]
    fn boot_reconcile_thread_map_cleanup_preserves_entries_on_unknown_validation() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', '111')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, channel_thread_map, active_thread_id,
                created_at, updated_at
             ) VALUES (
                'card-thread-unknown', 'Thread Unknown', 'review', 'agent-1',
                '{\"111\":\"thread-unknown\"}', 'thread-unknown', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();

        let cleared =
            cleanup_stale_channel_thread_map_entries_with(&conn, |_channel_id, _thread_id| {
                ThreadMapValidation::Unknown
            })
            .unwrap();
        assert_eq!(cleared, 0);

        let (map, active_thread_id): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT channel_thread_map, active_thread_id
                 FROM kanban_cards WHERE id = 'card-thread-unknown'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(map.as_deref(), Some("{\"111\":\"thread-unknown\"}"));
        assert_eq!(active_thread_id.as_deref(), Some("thread-unknown"));
    }

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
