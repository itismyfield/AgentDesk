use super::{
    discord_delivery::{
        DispatchTransport, HttpDispatchTransport, discord_api_base_url, discord_api_url,
    },
    thread_reuse::clear_all_threads,
};
use libsql_rusqlite::OptionalExtension;
use sqlx::{PgPool, Row as SqlxRow};
use std::process::Command;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(crate) struct DispatchFollowupConfig {
    pub discord_api_base: String,
    pub notify_bot_token: Option<String>,
    pub announce_bot_token: Option<String>,
}

impl DispatchFollowupConfig {
    fn from_runtime() -> Self {
        Self {
            discord_api_base: discord_api_base_url(),
            notify_bot_token: crate::credential::read_bot_token("notify"),
            announce_bot_token: crate::credential::read_bot_token("announce"),
        }
    }
}

#[derive(Clone, Debug)]
struct CompletedDispatchInfo {
    dispatch_type: String,
    status: String,
    card_id: String,
    result_json: Option<String>,
    context_json: Option<String>,
    thread_id: Option<String>,
    duration_seconds: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchMergeStatus {
    Noop,
    Pending,
    Merged,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DispatchChangeStats {
    files_changed: u64,
    additions: u64,
    deletions: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DispatchCompletionSummary {
    stats: DispatchChangeStats,
    merge_status: DispatchMergeStatus,
    duration_seconds: Option<i64>,
}

// ── Outbox worker trait ───────────────────────────────────────

/// Trait for outbox side-effects (Discord notifications, followups).
/// Extracted from `dispatch_outbox_loop` to allow mock injection in tests.
pub(crate) trait OutboxNotifier: Send + Sync {
    fn notify_dispatch(
        &self,
        db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    fn handle_followup(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    fn sync_status_reaction(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Production notifier that calls the real Discord functions.
pub(crate) struct RealOutboxNotifier {
    pg_pool: Arc<PgPool>,
}

impl RealOutboxNotifier {
    fn new(pg_pool: Arc<PgPool>) -> Self {
        Self { pg_pool }
    }
}

impl OutboxNotifier for RealOutboxNotifier {
    async fn notify_dispatch(
        &self,
        db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> Result<(), String> {
        super::discord_delivery::send_dispatch_to_discord_with_pg(
            db.as_ref(),
            Some(self.pg_pool.as_ref()),
            &agent_id,
            &title,
            &card_id,
            &dispatch_id,
        )
        .await
    }

    async fn handle_followup(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> Result<(), String> {
        handle_completed_dispatch_followups_with_pg(
            db.as_ref(),
            Some(self.pg_pool.as_ref()),
            &dispatch_id,
        )
        .await
    }

    async fn sync_status_reaction(
        &self,
        db: Option<crate::db::Db>,
        dispatch_id: String,
    ) -> Result<(), String> {
        super::discord_delivery::sync_dispatch_status_reaction_with_pg(
            db.as_ref(),
            Some(self.pg_pool.as_ref()),
            &dispatch_id,
        )
        .await
    }
}

/// Backoff delays for outbox retries: 1m → 5m → 15m → 1h
const RETRY_BACKOFF_SECS: [i64; 4] = [60, 300, 900, 3600];
/// Maximum number of retries before marking as permanent failure.
const MAX_RETRY_COUNT: i64 = 4;

fn dispatch_notify_delivery_suppressed(
    conn: &libsql_rusqlite::Connection,
    dispatch_id: &str,
) -> libsql_rusqlite::Result<bool> {
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(matches!(
        status.as_deref(),
        Some("completed") | Some("failed") | Some("cancelled")
    ))
}

async fn dispatch_notify_delivery_suppressed_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, sqlx::Error> {
    let status =
        sqlx::query_scalar::<_, Option<String>>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await?;
    Ok(matches!(
        status.flatten().as_deref(),
        Some("completed") | Some("failed") | Some("cancelled")
    ))
}

type DispatchOutboxRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

async fn claim_pending_dispatch_outbox_batch_pg(pool: &PgPool) -> Vec<DispatchOutboxRow> {
    let rows = match sqlx::query(
        "WITH claimed AS (
            SELECT id
              FROM dispatch_outbox
             WHERE status = 'pending'
               AND (next_attempt_at IS NULL OR next_attempt_at <= NOW())
             ORDER BY id ASC
             FOR UPDATE SKIP LOCKED
             LIMIT 5
        )
        UPDATE dispatch_outbox o
           SET status = 'processing'
          FROM claimed
         WHERE o.id = claimed.id
        RETURNING o.id, o.dispatch_id, o.action, o.agent_id, o.card_id, o.title, o.retry_count",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!("[dispatch-outbox] failed to claim postgres outbox rows: {error}");
            return Vec::new();
        }
    };

    let mut pending = rows
        .into_iter()
        .filter_map(|row| {
            Some((
                row.try_get::<i64, _>("id").ok()?,
                row.try_get::<String, _>("dispatch_id").ok()?,
                row.try_get::<String, _>("action").ok()?,
                row.try_get::<Option<String>, _>("agent_id").ok()?,
                row.try_get::<Option<String>, _>("card_id").ok()?,
                row.try_get::<Option<String>, _>("title").ok()?,
                row.try_get::<i64, _>("retry_count").ok()?,
            ))
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|row| row.0);
    pending
}

async fn mark_dispatch_dispatched_pg(pool: &PgPool, dispatch_id: &str) -> Result<(), String> {
    let current = sqlx::query(
        "SELECT status, kanban_card_id, dispatch_type
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for status update: {error}"))?;

    let Some(current) = current else {
        return Ok(());
    };

    let current_status = current
        .try_get::<String, _>("status")
        .map_err(|error| format!("read postgres dispatch status for {dispatch_id}: {error}"))?;
    if current_status != "pending" {
        return Ok(());
    }

    let kanban_card_id = current
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("read postgres dispatch card for {dispatch_id}: {error}"))?;
    let dispatch_type = current
        .try_get::<Option<String>, _>("dispatch_type")
        .map_err(|error| format!("read postgres dispatch type for {dispatch_id}: {error}"))?;

    let changed = sqlx::query(
        "UPDATE task_dispatches
            SET status = 'dispatched',
                updated_at = NOW()
          WHERE id = $1
            AND status = 'pending'",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("update postgres dispatch {dispatch_id} to dispatched: {error}"))?
    .rows_affected();
    if changed == 0 {
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO dispatch_events (
            dispatch_id,
            kanban_card_id,
            dispatch_type,
            from_status,
            to_status,
            transition_source,
            payload_json
        ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(dispatch_id)
    .bind(kanban_card_id)
    .bind(dispatch_type)
    .bind(Some(current_status.as_str()))
    .bind("dispatched")
    .bind("dispatch_outbox_notify")
    .bind(Option::<serde_json::Value>::None)
    .execute(pool)
    .await
    .map_err(|error| format!("insert postgres dispatch event for {dispatch_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action)
         SELECT $1, 'status_reaction'
          WHERE NOT EXISTS (
              SELECT 1
                FROM dispatch_outbox
               WHERE dispatch_id = $1
                 AND action = 'status_reaction'
                 AND status IN ('pending', 'processing')
          )",
    )
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("enqueue postgres status_reaction for {dispatch_id}: {error}"))?;

    Ok(())
}

/// Process one batch of pending outbox entries.
/// Returns the number of entries processed (0 if queue was empty).
///
/// Retry/backoff policy (#209):
/// - On notifier success: mark entry as 'done'
/// - On notifier failure (retry_count < MAX_RETRY_COUNT): increment retry_count,
///   set next_attempt_at with exponential backoff, revert to 'pending'
/// - On max retry exceeded: mark as 'failed' (permanent failure)
/// - For 'notify' actions: manages dispatch_notified reservation atomically
pub(crate) async fn process_outbox_batch<N: OutboxNotifier>(
    db: &crate::db::Db,
    notifier: &N,
) -> usize {
    process_outbox_batch_with_pg(Some(db), None, notifier).await
}

pub(crate) async fn process_outbox_batch_with_pg<N: OutboxNotifier>(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    notifier: &N,
) -> usize {
    let pending: Vec<DispatchOutboxRow> = if let Some(pool) = pg_pool {
        claim_pending_dispatch_outbox_batch_pg(pool).await
    } else {
        let Some(db) = db else {
            return 0;
        };
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let mut stmt = match conn.prepare(
            "SELECT id, dispatch_id, action, agent_id, card_id, title, retry_count \
             FROM dispatch_outbox \
             WHERE status = 'pending' \
               AND (next_attempt_at IS NULL OR next_attempt_at <= datetime('now')) \
             ORDER BY id ASC LIMIT 5",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    let count = pending.len();
    for (id, dispatch_id, action, agent_id, card_id, title, retry_count) in pending {
        if action == "notify" {
            let suppress_delivery = if let Some(pool) = pg_pool {
                dispatch_notify_delivery_suppressed_pg(pool, &dispatch_id)
                    .await
                    .unwrap_or(false)
            } else if let Some(db) = db {
                if let Ok(conn) = db.lock() {
                    dispatch_notify_delivery_suppressed(&conn, &dispatch_id).unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            };
            if suppress_delivery {
                if let Some(pool) = pg_pool {
                    sqlx::query(
                        "UPDATE dispatch_outbox
                            SET status = 'done',
                                processed_at = NOW(),
                                error = NULL
                          WHERE id = $1",
                    )
                    .bind(id)
                    .execute(pool)
                    .await
                    .ok();
                } else if let Some(db) = db {
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox SET status = 'done', processed_at = datetime('now'), error = NULL WHERE id = ?1",
                            [id],
                        )
                        .ok();
                    }
                }
                continue;
            }
        }

        // Mark as processing
        if pg_pool.is_none() {
            if let Some(db) = db {
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "UPDATE dispatch_outbox SET status = 'processing' WHERE id = ?1",
                        [id],
                    )
                    .ok();
                }
            }
        }

        let result = match action.as_str() {
            "notify" => {
                if let (Some(aid), Some(cid), Some(t)) =
                    (agent_id.clone(), card_id.clone(), title.clone())
                {
                    // Two-phase delivery guard (reservation + notified marker) is handled
                    // inside send_dispatch_to_discord, protecting all callers uniformly.
                    notifier
                        .notify_dispatch(db.cloned(), aid, t, cid, dispatch_id.clone())
                        .await
                } else {
                    Err("missing agent_id, card_id, or title for notify action".into())
                }
            }
            "followup" => {
                notifier
                    .handle_followup(db.cloned(), dispatch_id.clone())
                    .await
            }
            "status_reaction" => {
                // #750: narrow-path sync — sync_dispatch_status_reaction only
                // writes ❌ on failed/cancelled dispatches (command bot owns
                // ⏳/✅). Drains legacy rows correctly and covers repair paths
                // that bypass turn_bridge (queue/API cancel, orphan recovery).
                notifier
                    .sync_status_reaction(db.cloned(), dispatch_id.clone())
                    .await
            }
            other => {
                tracing::warn!("[dispatch-outbox] Unknown action: {other}");
                Err(format!("unknown action: {other}"))
            }
        };

        match result {
            Ok(()) => {
                // Mark done + transition dispatch pending → dispatched
                if let Some(pool) = pg_pool {
                    sqlx::query(
                        "UPDATE dispatch_outbox
                            SET status = 'done',
                                processed_at = NOW()
                          WHERE id = $1",
                    )
                    .bind(id)
                    .execute(pool)
                    .await
                    .ok();
                    if action == "notify" {
                        mark_dispatch_dispatched_pg(pool, &dispatch_id).await.ok();
                    }
                } else if let Some(db) = db {
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox SET status = 'done', processed_at = datetime('now') WHERE id = ?1",
                            [id],
                        )
                        .ok();
                        if action == "notify" {
                            crate::dispatch::set_dispatch_status_on_conn(
                                &conn,
                                &dispatch_id,
                                "dispatched",
                                None,
                                "dispatch_outbox_notify",
                                Some(&["pending"]),
                                false,
                            )
                            .ok();
                        }
                    }
                }
            }
            Err(err) => {
                let new_count = retry_count + 1;
                if new_count > MAX_RETRY_COUNT {
                    // Permanent failure — exhausted all 4 retries (1m → 5m → 15m → 1h)
                    tracing::error!(
                        "[dispatch-outbox] Permanent failure for entry {id} (dispatch={dispatch_id}, action={action}): {err}"
                    );
                    if let Some(pool) = pg_pool {
                        sqlx::query(
                            "UPDATE dispatch_outbox
                                SET status = 'failed',
                                    error = $1,
                                    retry_count = $2,
                                    processed_at = NOW()
                              WHERE id = $3",
                        )
                        .bind(&err)
                        .bind(new_count)
                        .bind(id)
                        .execute(pool)
                        .await
                        .ok();
                    } else if let Some(db) = db {
                        if let Ok(conn) = db.lock() {
                            conn.execute(
                                "UPDATE dispatch_outbox SET status = 'failed', error = ?1, \
                                 retry_count = ?2, processed_at = datetime('now') WHERE id = ?3",
                                libsql_rusqlite::params![err, new_count, id],
                            )
                            .ok();
                        }
                    }
                } else {
                    // Schedule retry with backoff (index = new_count - 1, since retry 1 uses BACKOFF[0])
                    let backoff_idx = (new_count - 1) as usize;
                    let backoff_secs = RETRY_BACKOFF_SECS.get(backoff_idx).copied().unwrap_or(3600);
                    tracing::warn!(
                        "[dispatch-outbox] Retry {new_count}/{MAX_RETRY_COUNT} for entry {id} (dispatch={dispatch_id}, action={action}) \
                         in {backoff_secs}s: {err}",
                    );
                    if let Some(pool) = pg_pool {
                        sqlx::query(
                            "UPDATE dispatch_outbox
                                SET status = 'pending',
                                    error = $1,
                                    retry_count = $2,
                                    next_attempt_at = NOW() + ($3::bigint * INTERVAL '1 second')
                              WHERE id = $4",
                        )
                        .bind(&err)
                        .bind(new_count)
                        .bind(backoff_secs)
                        .bind(id)
                        .execute(pool)
                        .await
                        .ok();
                    } else if let Some(db) = db {
                        if let Ok(conn) = db.lock() {
                            conn.execute(
                                "UPDATE dispatch_outbox SET status = 'pending', error = ?1, \
                                 retry_count = ?2, \
                                 next_attempt_at = datetime('now', '+' || ?3 || ' seconds') \
                                 WHERE id = ?4",
                                libsql_rusqlite::params![err, new_count, backoff_secs, id],
                            )
                            .ok();
                        }
                    }
                }
            }
        }
    }
    count
}

// ── Followup & verdict helpers ──────────────────────────────────

pub(super) fn extract_review_verdict(result_json: Option<&str>) -> String {
    result_json
        .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .and_then(|v| {
            v.get("verdict")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("decision")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
        })
        // NEVER default to "pass" — missing verdict means the review agent
        // did not submit a verdict (e.g. session idle auto-complete).
        // Returning "unknown" forces the followup path to request human/agent review.
        .unwrap_or_else(|| "unknown".to_string())
}

fn parse_json_value(raw: Option<&str>, field_name: &'static str) -> Option<serde_json::Value> {
    let value = raw?;
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                "[dispatch-outbox] malformed JSON in {field_name}; ignoring payload: {error}"
            );
            None
        }
    }
}

fn json_string_field<'a>(value: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    value
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_work_dispatch_type(dispatch_type: &str) -> bool {
    matches!(dispatch_type, "implementation" | "rework")
}

fn resolve_thread_id(
    thread_id: Option<&str>,
    context_json: Option<&serde_json::Value>,
) -> Option<String> {
    thread_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| json_string_field(context_json, "thread_id").map(str::to_string))
}

fn resolve_worktree_path(
    result_json: Option<&serde_json::Value>,
    context_json: Option<&serde_json::Value>,
) -> Option<String> {
    json_string_field(result_json, "completed_worktree_path")
        .or_else(|| json_string_field(result_json, "worktree_path"))
        .or_else(|| json_string_field(context_json, "worktree_path"))
        .map(str::to_string)
}

fn resolve_completed_branch(
    result_json: Option<&serde_json::Value>,
    context_json: Option<&serde_json::Value>,
    worktree_path: Option<&str>,
) -> Option<String> {
    json_string_field(result_json, "completed_branch")
        .or_else(|| json_string_field(result_json, "worktree_branch"))
        .or_else(|| json_string_field(result_json, "branch"))
        .or_else(|| json_string_field(context_json, "worktree_branch"))
        .or_else(|| json_string_field(context_json, "branch"))
        .map(str::to_string)
        .or_else(|| worktree_path.and_then(crate::services::platform::shell::git_branch_name))
}

fn resolve_completed_commit(result_json: Option<&serde_json::Value>) -> Option<String> {
    json_string_field(result_json, "completed_commit")
        .or_else(|| json_string_field(result_json, "reviewed_commit"))
        .map(str::to_string)
}

fn resolve_start_commit(
    result_json: Option<&serde_json::Value>,
    context_json: Option<&serde_json::Value>,
) -> Option<String> {
    json_string_field(context_json, "reviewed_commit")
        .or_else(|| json_string_field(result_json, "reviewed_commit"))
        .map(str::to_string)
}

fn dispatch_completed_without_changes(result_json: Option<&serde_json::Value>) -> bool {
    json_string_field(result_json, "work_outcome") == Some("noop")
        || result_json
            .and_then(|value| value.get("completed_without_changes"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
}

fn git_ref_exists(repo_dir: &str, git_ref: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", git_ref])
        .current_dir(repo_dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn resolve_upstream_base_ref(repo_dir: &str) -> Option<String> {
    ["origin/main", "main", "origin/master", "master"]
        .into_iter()
        .find(|candidate| git_ref_exists(repo_dir, candidate))
        .map(str::to_string)
}

fn git_diff_stats(repo_dir: &str, diff_spec: &str) -> Result<DispatchChangeStats, String> {
    let output = Command::new("git")
        .args(["diff", "--numstat", "--find-renames", diff_spec])
        .current_dir(repo_dir)
        .output()
        .map_err(|err| format!("git diff failed: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff {} failed with status {}",
            diff_spec, output.status
        ));
    }

    let mut stats = DispatchChangeStats::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let additions = parts.next().unwrap_or_default();
        let deletions = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        if path.trim().is_empty() {
            continue;
        }
        stats.files_changed += 1;
        stats.additions += additions.parse::<u64>().unwrap_or(0);
        stats.deletions += deletions.parse::<u64>().unwrap_or(0);
    }

    Ok(stats)
}

fn compute_dispatch_change_stats(
    worktree_path: Option<&str>,
    start_commit: Option<&str>,
    completed_commit: Option<&str>,
    completed_without_changes: bool,
) -> Option<DispatchChangeStats> {
    if completed_without_changes {
        return Some(DispatchChangeStats::default());
    }

    let repo_dir = worktree_path.filter(|path| std::path::Path::new(path).is_dir())?;
    let diff_spec =
        if let (Some(start_commit), Some(completed_commit)) = (start_commit, completed_commit) {
            format!("{start_commit}..{completed_commit}")
        } else {
            let completed_commit = completed_commit?;
            let base_ref = resolve_upstream_base_ref(repo_dir)?;
            format!("{base_ref}...{completed_commit}")
        };

    git_diff_stats(repo_dir, &diff_spec).ok()
}

fn compute_dispatch_merge_status(
    worktree_path: Option<&str>,
    completed_commit: Option<&str>,
    completed_branch: Option<&str>,
    completed_without_changes: bool,
) -> DispatchMergeStatus {
    if completed_without_changes {
        return DispatchMergeStatus::Noop;
    }

    let Some(repo_dir) = worktree_path.filter(|path| std::path::Path::new(path).is_dir()) else {
        return DispatchMergeStatus::Unknown;
    };

    if let Some(completed_commit) = completed_commit {
        let Some(base_ref) = resolve_upstream_base_ref(repo_dir) else {
            return DispatchMergeStatus::Unknown;
        };
        return match Command::new("git")
            .args(["merge-base", "--is-ancestor", completed_commit, &base_ref])
            .current_dir(repo_dir)
            .status()
        {
            Ok(status) if status.success() => DispatchMergeStatus::Merged,
            Ok(status) if status.code() == Some(1) => DispatchMergeStatus::Pending,
            _ => DispatchMergeStatus::Unknown,
        };
    }

    match completed_branch {
        Some("main") | Some("master") => DispatchMergeStatus::Merged,
        Some(_) => DispatchMergeStatus::Pending,
        None => DispatchMergeStatus::Unknown,
    }
}

fn format_dispatch_duration(duration_seconds: Option<i64>) -> String {
    let Some(total_seconds) = duration_seconds.filter(|value| *value > 0) else {
        return "확인 불가".to_string();
    };
    let total_minutes = (total_seconds + 59) / 60;
    if total_minutes < 60 {
        return format!("{total_minutes}분");
    }
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if minutes == 0 {
        format!("{hours}시간")
    } else {
        format!("{hours}시간 {minutes}분")
    }
}

fn format_merge_status(merge_status: DispatchMergeStatus) -> &'static str {
    match merge_status {
        DispatchMergeStatus::Noop => "noop",
        DispatchMergeStatus::Pending => "머지 대기",
        DispatchMergeStatus::Merged => "main 반영됨",
        DispatchMergeStatus::Unknown => "머지 상태 확인 불가",
    }
}

fn build_dispatch_completion_summary(info: &CompletedDispatchInfo) -> Option<String> {
    if !is_work_dispatch_type(&info.dispatch_type) {
        return None;
    }

    let result_json = parse_json_value(info.result_json.as_deref(), "result_json");
    let context_json = parse_json_value(info.context_json.as_deref(), "context_json");
    let completed_without_changes = dispatch_completed_without_changes(result_json.as_ref());
    let worktree_path = resolve_worktree_path(result_json.as_ref(), context_json.as_ref());
    let completed_commit = resolve_completed_commit(result_json.as_ref());
    let start_commit = resolve_start_commit(result_json.as_ref(), context_json.as_ref());
    let completed_branch = resolve_completed_branch(
        result_json.as_ref(),
        context_json.as_ref(),
        worktree_path.as_deref(),
    );
    let stats = compute_dispatch_change_stats(
        worktree_path.as_deref(),
        start_commit.as_deref(),
        completed_commit.as_deref(),
        completed_without_changes,
    )?;
    let merge_status = compute_dispatch_merge_status(
        worktree_path.as_deref(),
        completed_commit.as_deref(),
        completed_branch.as_deref(),
        completed_without_changes,
    );
    let summary = DispatchCompletionSummary {
        stats,
        merge_status,
        duration_seconds: info.duration_seconds,
    };

    Some(format!(
        "🔔 완료 요약: {}개 파일, +{}/-{}, {}\n소요 시간 {}",
        summary.stats.files_changed,
        summary.stats.additions,
        summary.stats.deletions,
        format_merge_status(summary.merge_status),
        format_dispatch_duration(summary.duration_seconds),
    ))
}

async fn ensure_thread_is_postable(
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    thread_id: &str,
) -> Result<(), String> {
    let info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
    let response = client
        .get(&info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect dispatch thread {thread_id}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "dispatch thread {thread_id} unavailable: HTTP {}",
            response.status()
        ));
    }

    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse dispatch thread {thread_id}: {err}"))?;
    let metadata = body.get("thread_metadata");
    let is_locked = metadata
        .and_then(|value| value.get("locked"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if is_locked {
        return Err(format!("dispatch thread {thread_id} is locked"));
    }

    let is_archived = metadata
        .and_then(|value| value.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !is_archived {
        return Ok(());
    }

    let response = client
        .patch(&info_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"archived": false}))
        .send()
        .await
        .map_err(|err| format!("failed to unarchive dispatch thread {thread_id}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "failed to unarchive dispatch thread {thread_id}: HTTP {}",
            response.status()
        ));
    }

    Ok(())
}

async fn post_dispatch_completion_summary(
    dispatch_id: &str,
    thread_id: &str,
    message: &str,
    config: &DispatchFollowupConfig,
) -> Result<(), String> {
    let Some(token) = config.notify_bot_token.as_deref() else {
        return Err("no notify bot token".to_string());
    };

    let client = reqwest::Client::new();
    ensure_thread_is_postable(&client, token, &config.discord_api_base, thread_id).await?;

    let message_url = discord_api_url(
        &config.discord_api_base,
        &format!("/channels/{thread_id}/messages"),
    );
    let response = client
        .post(&message_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
        .await
        .map_err(|err| format!("failed to post dispatch summary for {dispatch_id}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "failed to post dispatch summary for {dispatch_id}: HTTP {}",
            response.status()
        ));
    }

    Ok(())
}

async fn archive_dispatch_thread(
    thread_id: &str,
    dispatch_id: &str,
    config: &DispatchFollowupConfig,
) -> Result<(), String> {
    let Some(token) = config.announce_bot_token.as_deref() else {
        return Err("no announce bot token".to_string());
    };

    let archive_url = discord_api_url(&config.discord_api_base, &format!("/channels/{thread_id}"));
    let client = reqwest::Client::new();
    let response = client
        .patch(&archive_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"archived": true}))
        .send()
        .await
        .map_err(|err| format!("failed to archive thread {thread_id}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "failed to archive thread {thread_id} for completed dispatch {dispatch_id}: HTTP {}",
            response.status()
        ));
    }

    Ok(())
}

/// Send Discord notifications for a completed dispatch (review verdicts, etc.).
/// Callers of `finalize_dispatch` should spawn this after the sync call returns.
pub(crate) async fn handle_completed_dispatch_followups(
    db: &crate::db::Db,
    dispatch_id: &str,
) -> Result<(), String> {
    handle_completed_dispatch_followups_with_pg(Some(db), None, dispatch_id).await
}

pub(crate) async fn handle_completed_dispatch_followups_with_pg(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<(), String> {
    let transport = HttpDispatchTransport::from_runtime_with_pg(db, pg_pool.cloned());
    handle_completed_dispatch_followups_internal(
        db,
        pg_pool,
        dispatch_id,
        &DispatchFollowupConfig::from_runtime(),
        &transport,
    )
    .await
}

pub(crate) async fn handle_completed_dispatch_followups_with_config(
    db: &crate::db::Db,
    dispatch_id: &str,
    config: &DispatchFollowupConfig,
) -> Result<(), String> {
    let transport = HttpDispatchTransport::from_runtime(db);
    handle_completed_dispatch_followups_internal(Some(db), None, dispatch_id, config, &transport)
        .await
}

pub(crate) async fn handle_completed_dispatch_followups_with_config_and_transport<
    T: DispatchTransport,
>(
    db: &crate::db::Db,
    dispatch_id: &str,
    config: &DispatchFollowupConfig,
    transport: &T,
) -> Result<(), String> {
    handle_completed_dispatch_followups_internal(Some(db), None, dispatch_id, config, transport)
        .await
}

async fn handle_completed_dispatch_followups_internal<T: DispatchTransport>(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    config: &DispatchFollowupConfig,
    transport: &T,
) -> Result<(), String> {
    let pg_pool = pg_pool.or_else(|| transport.pg_pool());
    let info = load_completed_dispatch_info(db, pg_pool, dispatch_id).await?;

    let Some(mut info) = info else {
        return Err(format!("dispatch {dispatch_id} not found"));
    };
    if info.status != "completed" {
        return Ok(()); // Not an error — dispatch not yet completed
    }
    let context_json_value = parse_json_value(info.context_json.as_deref(), "context_json");
    info.thread_id = resolve_thread_id(info.thread_id.as_deref(), context_json_value.as_ref());

    if info.dispatch_type == "review" {
        let verdict = extract_review_verdict(info.result_json.as_deref());
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔍 REVIEW-FOLLOWUP: dispatch={dispatch_id} verdict={verdict} result={:?}",
            info.result_json.as_deref().unwrap_or("NULL")
        );
        // Skip Discord notification for auto-completed reviews without an explicit verdict.
        // The policy engine's onDispatchCompleted hook handles those (review-automation.js).
        // Only send_review_result_to_primary for explicit verdicts (pass/improve/reject)
        // submitted via the verdict API — these have a real "verdict" field in the result.
        if verdict != "unknown" {
            super::discord_delivery::send_review_result_to_primary_with_transport(
                db,
                &info.card_id,
                dispatch_id,
                &verdict,
                transport,
            )
            .await?;
        } else {
            tracing::info!(
                "  [{ts}] ⏭ REVIEW-FOLLOWUP: skipping send_review_result_to_primary (verdict=unknown)"
            );
        }
    }

    if let (Some(thread_id), Some(summary_message)) = (
        info.thread_id.as_deref(),
        build_dispatch_completion_summary(&info),
    ) {
        if let Err(err) =
            post_dispatch_completion_summary(dispatch_id, thread_id, &summary_message, config).await
        {
            tracing::warn!(
                "[dispatch] Failed to post completion summary for dispatch {dispatch_id} to thread {thread_id}: {err}"
            );
        }
    }

    // Archive thread on dispatch completion — but only if the card is done.
    // When the card has an active lifecycle (not done), keep the thread open for reuse
    // by subsequent dispatches (rework, review-decision, etc.).
    let card_status = load_card_status(db, pg_pool, &info.card_id).await?;
    let should_archive = card_status.as_deref() == Some("done");

    if should_archive {
        if let Some(ref tid) = info.thread_id {
            if let Err(err) = archive_dispatch_thread(tid, dispatch_id, config).await {
                tracing::warn!(
                    "[dispatch] Failed to archive thread {tid} for completed dispatch {dispatch_id}: {err}"
                );
            } else {
                tracing::info!(
                    "[dispatch] Archived thread {tid} for completed dispatch {dispatch_id} (card done)"
                );
            }
        }
        clear_all_dispatch_threads(db, pg_pool, &info.card_id).await?;
    }

    // Generic resend removed — dispatch Discord notification is handled by:
    // 1. kanban.rs fire_transition_hooks → onCardTransition → send_dispatch_to_discord
    // 2. timeouts.js [I-0] recovery for unnotified dispatches
    // 3. dispatch_notified guard in process_outbox_batch prevents duplicates
    // Previously this generic resend caused 2-3x duplicate messages for every dispatch.
    Ok(())
}

async fn load_completed_dispatch_info(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<Option<CompletedDispatchInfo>, String> {
    if let Some(pool) = pg_pool {
        let row = sqlx::query(
            "SELECT td.dispatch_type,
                    td.status,
                    kc.id AS card_id,
                    td.result,
                    td.context,
                    td.thread_id,
                    CAST(
                        EXTRACT(
                            EPOCH FROM (
                                COALESCE(td.completed_at, td.updated_at, td.created_at) - td.created_at
                            )
                        ) AS BIGINT
                    ) AS duration_seconds
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id
             WHERE td.id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load dispatch {dispatch_id} followup info from postgres: {error}"))?;

        return row
            .map(|row| {
                Ok(CompletedDispatchInfo {
                    dispatch_type: row.try_get("dispatch_type").map_err(|error| {
                        format!("read postgres dispatch_type for {dispatch_id}: {error}")
                    })?,
                    status: row.try_get("status").map_err(|error| {
                        format!("read postgres status for {dispatch_id}: {error}")
                    })?,
                    card_id: row.try_get("card_id").map_err(|error| {
                        format!("read postgres card_id for {dispatch_id}: {error}")
                    })?,
                    result_json: row.try_get("result").map_err(|error| {
                        format!("read postgres result for {dispatch_id}: {error}")
                    })?,
                    context_json: row.try_get("context").map_err(|error| {
                        format!("read postgres context for {dispatch_id}: {error}")
                    })?,
                    thread_id: row.try_get("thread_id").map_err(|error| {
                        format!("read postgres thread_id for {dispatch_id}: {error}")
                    })?,
                    duration_seconds: row.try_get("duration_seconds").map_err(|error| {
                        format!("read postgres duration_seconds for {dispatch_id}: {error}")
                    })?,
                })
            })
            .transpose();
    }

    let Some(db) = db else {
        return Err("dispatch lookup requires sqlite db or postgres pool".to_string());
    };
    let conn = db
        .lock()
        .map_err(|_| "db lock failed for dispatch lookup".to_string())?;
    conn.query_row(
        "SELECT td.dispatch_type, td.status, kc.id, td.result, td.context, td.thread_id, \
                CAST(ROUND((julianday(COALESCE(td.completed_at, td.updated_at, td.created_at)) - julianday(td.created_at)) * 86400) AS INTEGER) \
         FROM task_dispatches td \
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
         WHERE td.id = ?1",
        [dispatch_id],
        |row| {
            Ok(CompletedDispatchInfo {
                dispatch_type: row.get(0)?,
                status: row.get(1)?,
                card_id: row.get(2)?,
                result_json: row.get(3)?,
                context_json: row.get(4)?,
                thread_id: row.get(5)?,
                duration_seconds: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|error| format!("load dispatch {dispatch_id} followup info: {error}"))
}

async fn load_card_status(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
) -> Result<Option<String>, String> {
    if let Some(pool) = pg_pool {
        let row = sqlx::query("SELECT status FROM kanban_cards WHERE id = $1")
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres card status for {card_id}: {error}"))?;
        return row
            .map(|row| {
                row.try_get("status")
                    .map_err(|error| format!("read postgres card status for {card_id}: {error}"))
            })
            .transpose();
    }

    let Some(db) = db else {
        return Err("card status lookup requires sqlite db or postgres pool".to_string());
    };

    Ok(db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    }))
}

async fn clear_all_dispatch_threads(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
) -> Result<(), String> {
    if let Some(pool) = pg_pool {
        sqlx::query(
            "UPDATE kanban_cards
             SET channel_thread_map = NULL,
                 active_thread_id = NULL
             WHERE id = $1",
        )
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| format!("clear postgres thread mappings for {card_id}: {error}"))?;
        return Ok(());
    }

    let Some(db) = db else {
        return Err("thread cleanup requires sqlite db or postgres pool".to_string());
    };
    if let Ok(conn) = db.lock() {
        clear_all_threads(&conn, card_id);
    }
    Ok(())
}

// ── Channel helpers ─────────────────────────────────────────────

/// Resolve a channel name alias (e.g. "adk-cc") to a numeric channel ID.
/// Public wrapper around the shared resolve_channel_alias.
pub fn resolve_channel_alias_pub(alias: &str) -> Option<u64> {
    super::resolve_channel_alias(alias)
}

pub(crate) fn use_counter_model_channel(dispatch_type: Option<&str>) -> bool {
    // "review", "e2e-test" (#197), and "consultation" (#256) go to the counter-model channel.
    // "review-decision" is routed back to the original implementation provider
    // so it reuses the implementation-side thread rather than the reviewer channel.
    matches!(
        dispatch_type,
        Some("review") | Some("e2e-test") | Some("consultation")
    )
}

// ── Message formatting ──────────────────────────────────────────

const DISPATCH_MESSAGE_TARGET_LEN: usize = 500;
pub(super) const DISPATCH_MESSAGE_HARD_LIMIT: usize = 1800;
const DISPATCH_TITLE_PRIMARY_LIMIT: usize = 160;
const DISPATCH_TITLE_COMPACT_LIMIT: usize = 96;
const DISPATCH_TITLE_MINIMAL_LIMIT: usize = 72;

fn truncate_chars(value: &str, limit: usize) -> String {
    let total = value.chars().count();
    if total <= limit {
        return value.to_string();
    }
    if limit <= 1 {
        return "…".chars().take(limit).collect();
    }

    let mut truncated: String = value.chars().take(limit - 1).collect();
    truncated.push('…');
    truncated
}

fn compact_dispatch_title(title: &str, limit: usize) -> String {
    let first_line = title
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(title);
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        "Untitled dispatch".to_string()
    } else {
        truncate_chars(trimmed, limit)
    }
}

fn dispatch_type_label(dispatch_type: Option<&str>) -> &'static str {
    match dispatch_type {
        Some("implementation") => "📋 구현",
        Some("review") => "🔍 리뷰",
        Some("rework") => "🔧 리워크",
        Some("review-decision") => "⚖️ 리뷰 검토",
        Some("pm-decision") => "🎯 PM 판단",
        Some("e2e-test") => "🧪 E2E 테스트",
        Some("consultation") => "💬 상담",
        Some("phase-gate") => "🚦 Phase Gate",
        _ => "dispatch",
    }
}

fn dispatch_reason_suffix(context_json: &serde_json::Value) -> String {
    let reason = context_json
        .get("resumed_from")
        .and_then(|r| r.as_str())
        .map(|s| format!("resume from {s}"))
        .or_else(|| {
            if context_json
                .get("retry")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("retry".to_string())
            } else if context_json
                .get("redispatch")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("redispatch".to_string())
            } else if context_json
                .get("auto_queue")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("auto-queue".to_string())
            } else if context_json
                .get("auto_accept")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("auto-accept rework".to_string())
            } else {
                None
            }
        });

    reason
        .map(|value| format!(" ({value})"))
        .unwrap_or_default()
}

fn trim_context_string<'a>(context_json: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    context_json
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(super) fn review_target_hint(
    issue_number: Option<i64>,
    context_json: &serde_json::Value,
) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(repo) = trim_context_string(context_json, "repo")
        .or_else(|| trim_context_string(context_json, "target_repo"))
    {
        parts.push(format!("repo={repo}"));
    }
    if let Some(issue_number) = context_json
        .get("issue_number")
        .and_then(|value| value.as_i64())
        .or(issue_number)
    {
        parts.push(format!("issue=#{issue_number}"));
    }
    if let Some(pr_number) = context_json
        .get("pr_number")
        .and_then(|value| value.as_i64())
    {
        parts.push(format!("pr=#{pr_number}"));
    }
    if let Some(commit) = trim_context_string(context_json, "reviewed_commit") {
        parts.push(format!("commit={}", truncate_chars(commit, 12)));
    }

    (!parts.is_empty()).then(|| parts.join(", "))
}

pub(super) fn review_submission_hint(
    dispatch_type: Option<&str>,
    dispatch_id: &str,
    context_json: &serde_json::Value,
) -> Option<String> {
    match dispatch_type {
        Some("review") => Some(format!(
            "제출: `{}` (`dispatch_id={dispatch_id}`)",
            trim_context_string(context_json, "verdict_endpoint")
                .unwrap_or("POST /api/review-verdict")
        )),
        Some("review-decision") => Some(format!(
            "제출: `{}`",
            trim_context_string(context_json, "decision_endpoint")
                .unwrap_or("POST /api/review-decision")
        )),
        _ => None,
    }
}

fn dispatch_instruction_line(
    dispatch_type: Option<&str>,
    dispatch_id: &str,
    issue_number: Option<i64>,
    context_json: &serde_json::Value,
) -> String {
    match dispatch_type {
        Some("review") => {
            let mut line =
                "한 줄 지시: 코드 리뷰만 수행하고 상세 범위와 verdict 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
                    .to_string();
            if let Some(target) = review_target_hint(issue_number, context_json) {
                line.push_str(&format!(" 대상: {target}."));
            }
            if let Some(submission) = review_submission_hint(dispatch_type, dispatch_id, context_json)
            {
                line.push_str(&format!(" {submission}."));
            }
            line
        }
        Some("review-decision") => {
            let mut line =
                "한 줄 지시: GitHub 리뷰 피드백을 확인하고 accept/dispute/dismiss 중 하나를 제출하세요."
                    .to_string();
            if let Some(target) = review_target_hint(issue_number, context_json) {
                line.push_str(&format!(" 대상: {target}."));
            }
            if let Some(submission) = review_submission_hint(dispatch_type, dispatch_id, context_json)
            {
                line.push_str(&format!(" {submission}."));
            }
            line
        }
        Some("implementation") => {
            "한 줄 지시: 이 이슈를 구현하고 상세 요구사항과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
                .to_string()
        }
        Some("rework") => {
            "한 줄 지시: 기존 결과를 수정하고 상세 요구사항과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
                .to_string()
        }
        Some("e2e-test") => {
            "한 줄 지시: 검증만 수행하고 상세 기준과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
                .to_string()
        }
        Some("consultation") => {
            "한 줄 지시: 필요한 조사/판단만 수행하고 상세 기준과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
                .to_string()
        }
        Some("phase-gate") => {
            "한 줄 지시: phase gate 판정만 수행하고 체크 항목과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요."
                .to_string()
        }
        _ => "한 줄 지시: 상세 요구사항은 시스템 프롬프트의 [Current Task]를 따르세요."
            .to_string(),
    }
}

fn minimal_dispatch_instruction_line(
    dispatch_type: Option<&str>,
    dispatch_id: &str,
    issue_number: Option<i64>,
    context_json: &serde_json::Value,
) -> String {
    match dispatch_type {
        Some("review") | Some("review-decision") => {
            let mut line =
                "상세 요구사항은 시스템 프롬프트의 [Current Task]를 따르세요.".to_string();
            if let Some(target) = review_target_hint(issue_number, context_json) {
                line.push_str(&format!(" 대상: {target}."));
            }
            if let Some(submission) =
                review_submission_hint(dispatch_type, dispatch_id, context_json)
            {
                line.push_str(&format!(" {submission}."));
            }
            line
        }
        _ => "상세 요구사항과 완료 규칙은 시스템 프롬프트의 [Current Task]를 따르세요.".to_string(),
    }
}

fn render_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    dispatch_type: Option<&str>,
    context_json: &serde_json::Value,
    title_limit: usize,
    include_url: bool,
    instruction_line: &str,
) -> String {
    let compact_title = compact_dispatch_title(title, title_limit);
    let title_with_issue = match issue_number {
        Some(number) if !compact_title.contains(&format!("#{number}")) => {
            format!("#{number} {compact_title}")
        }
        _ => compact_title,
    };
    let mut lines = vec![format!(
        "DISPATCH:{dispatch_id} [{}] - {}{}",
        dispatch_type_label(dispatch_type),
        title_with_issue,
        dispatch_reason_suffix(context_json),
    )];
    if include_url && let Some(url) = issue_url.map(str::trim).filter(|value| !value.is_empty()) {
        lines.push(format!("<{url}>"));
    }
    lines.push(instruction_line.to_string());

    prefix_dispatch_message(dispatch_type.unwrap_or("dispatch"), &lines.join("\n"))
}

pub(super) fn build_minimal_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> String {
    let context_json = dispatch_context
        .and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let message = render_dispatch_message(
        dispatch_id,
        title,
        issue_url,
        issue_number,
        dispatch_type,
        &context_json,
        DISPATCH_TITLE_MINIMAL_LIMIT,
        false,
        &minimal_dispatch_instruction_line(dispatch_type, dispatch_id, issue_number, &context_json),
    );
    truncate_chars(&message, DISPATCH_MESSAGE_HARD_LIMIT)
}

pub(super) fn format_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> String {
    let context_json = dispatch_context
        .and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let primary = render_dispatch_message(
        dispatch_id,
        title,
        issue_url,
        issue_number,
        dispatch_type,
        &context_json,
        DISPATCH_TITLE_PRIMARY_LIMIT,
        true,
        &dispatch_instruction_line(dispatch_type, dispatch_id, issue_number, &context_json),
    );
    if primary.chars().count() <= DISPATCH_MESSAGE_TARGET_LEN {
        return primary;
    }

    let compact = render_dispatch_message(
        dispatch_id,
        title,
        issue_url,
        issue_number,
        dispatch_type,
        &context_json,
        DISPATCH_TITLE_COMPACT_LIMIT,
        true,
        &minimal_dispatch_instruction_line(dispatch_type, dispatch_id, issue_number, &context_json),
    );
    if compact.chars().count() <= DISPATCH_MESSAGE_HARD_LIMIT {
        return compact;
    }

    build_minimal_dispatch_message(
        dispatch_id,
        title,
        issue_url,
        issue_number,
        dispatch_type,
        dispatch_context,
    )
}

pub(super) fn prefix_dispatch_message(dispatch_type: &str, message: &str) -> String {
    let full = format!("── {} dispatch ──\n{}", dispatch_type, message);
    truncate_dispatch_message(&full)
}

/// Hard-truncate dispatch message to stay within Discord's 2000-char limit.
/// Preserves the first line (DISPATCH:id header) and appends a truncation marker.
fn truncate_dispatch_message(message: &str) -> String {
    const DISCORD_LIMIT: usize = 1900;
    if message.chars().count() <= DISCORD_LIMIT {
        return message.to_string();
    }
    let byte_boundary = message
        .char_indices()
        .nth(DISCORD_LIMIT)
        .map(|(i, _)| i)
        .unwrap_or(message.len());
    let cut = message[..byte_boundary]
        .rfind('\n')
        .unwrap_or(byte_boundary);
    format!(
        "{}\n\n[… truncated — full context in system prompt]",
        &message[..cut]
    )
}

// ── #144: Dispatch Notification Outbox ───────────────────────

/// Queue a dispatch completion followup for async processing.
///
/// Replaces `tokio::spawn(handle_completed_dispatch_followups(...))`.
pub(crate) fn queue_dispatch_followup(db: &crate::db::Db, dispatch_id: &str) {
    if let Ok(conn) = db.separate_conn() {
        conn.execute(
            "INSERT OR IGNORE INTO dispatch_outbox (dispatch_id, action) VALUES (?1, 'followup')",
            [dispatch_id],
        )
        .ok();
    }
}

pub(crate) fn queue_dispatch_followup_sync(
    db: &crate::db::Db,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) {
    if let Some(pool) = pg_pool {
        let dispatch_id_owned = dispatch_id.to_string();
        if let Err(error) = crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                queue_dispatch_followup_pg(&bridge_pool, &dispatch_id_owned).await
            },
            |error| error,
        ) {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                "failed to enqueue postgres followup: {error}"
            );
        }
        return;
    }

    queue_dispatch_followup(db, dispatch_id);
}

pub(crate) async fn queue_dispatch_followup_pg(
    pg_pool: &PgPool,
    dispatch_id: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action)
         VALUES ($1, 'followup')
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("enqueue postgres followup for {dispatch_id}: {error}"))?;
    Ok(())
}

pub(crate) async fn requeue_dispatch_notify_pg(
    pg_pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool, String> {
    let dispatch = sqlx::query(
        "SELECT status, to_agent_id, kanban_card_id, title
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| format!("load postgres dispatch {dispatch_id} for notify requeue: {error}"))?;

    let Some(dispatch) = dispatch else {
        return Ok(false);
    };

    let status = dispatch
        .try_get::<String, _>("status")
        .map_err(|error| format!("read postgres dispatch status for {dispatch_id}: {error}"))?;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Ok(false);
    }

    let agent_id = dispatch
        .try_get::<Option<String>, _>("to_agent_id")
        .map_err(|error| format!("read postgres dispatch agent for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing to_agent_id"))?;
    let card_id = dispatch
        .try_get::<Option<String>, _>("kanban_card_id")
        .map_err(|error| format!("read postgres dispatch card for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing kanban_card_id"))?;
    let title = dispatch
        .try_get::<Option<String>, _>("title")
        .map_err(|error| format!("read postgres dispatch title for {dispatch_id}: {error}"))?
        .ok_or_else(|| format!("postgres dispatch {dispatch_id} missing title"))?;

    let updated = sqlx::query(
        "UPDATE dispatch_outbox
            SET agent_id = $2,
                card_id = $3,
                title = $4,
                status = 'pending',
                retry_count = 0,
                next_attempt_at = NULL,
                processed_at = NULL,
                error = NULL
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("reset postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    if updated > 0 {
        return Ok(true);
    }

    let inserted = sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, status, retry_count
         ) VALUES ($1, 'notify', $2, $3, $4, 'pending', 0)
         ON CONFLICT DO NOTHING",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("insert postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    if inserted > 0 {
        return Ok(true);
    }

    let rearmed = sqlx::query(
        "UPDATE dispatch_outbox
            SET agent_id = $2,
                card_id = $3,
                title = $4,
                status = 'pending',
                retry_count = 0,
                next_attempt_at = NULL,
                processed_at = NULL,
                error = NULL
          WHERE dispatch_id = $1
            AND action = 'notify'",
    )
    .bind(dispatch_id)
    .bind(&agent_id)
    .bind(&card_id)
    .bind(&title)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("rearm postgres notify outbox for {dispatch_id}: {error}"))?
    .rows_affected();
    Ok(rearmed > 0)
}

/// Worker loop that drains dispatch_outbox and executes Discord side-effects.
///
/// This is the SINGLE place where dispatch-related Discord HTTP calls originate.
/// All other code paths insert into the outbox table and return immediately.
pub(crate) async fn dispatch_outbox_loop(pg_pool: Arc<PgPool>) {
    use std::time::Duration;

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[dispatch-outbox] Worker started (adaptive backoff 500ms-5s)");

    let notifier = RealOutboxNotifier::new(pg_pool);
    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;

        let processed =
            process_outbox_batch_with_pg(None, Some(notifier.pg_pool.as_ref()), &notifier).await;
        if processed == 0 {
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
        } else {
            poll_interval = Duration::from_millis(500);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::routes::dispatches::discord_delivery;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    fn test_db() -> crate::db::Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_dispatch_outbox_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch outbox tests",
            )
            .await
            .unwrap();

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatch outbox tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatch outbox tests",
            )
            .await
            .unwrap();
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[test]
    fn parse_json_value_logs_warn_and_returns_none_for_malformed_json() {
        let (value, logs) = capture_logs(|| parse_json_value(Some("{"), "result_json"));
        assert!(value.is_none());
        assert!(logs.contains("[dispatch-outbox] malformed JSON in result_json"));
    }

    #[derive(Clone, Default)]
    struct MockOutboxNotifier {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Clone)]
    struct PgAwareTransport {
        pg_pool: PgPool,
        dispatch_calls: Arc<Mutex<Vec<String>>>,
    }

    impl PgAwareTransport {
        fn new(pg_pool: PgPool) -> Self {
            Self {
                pg_pool,
                dispatch_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl DispatchTransport for PgAwareTransport {
        fn pg_pool(&self) -> Option<&PgPool> {
            Some(&self.pg_pool)
        }

        async fn send_dispatch(
            &self,
            _db: Option<crate::db::Db>,
            agent_id: String,
            _title: String,
            _card_id: String,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.dispatch_calls
                .lock()
                .unwrap()
                .push(format!("{agent_id}:{dispatch_id}"));
            Ok(())
        }

        async fn send_review_followup(
            &self,
            _db: Option<crate::db::Db>,
            _card_id: String,
            _channel_id_num: u64,
            _message: String,
            _kind: discord_delivery::ReviewFollowupKind,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    impl OutboxNotifier for MockOutboxNotifier {
        async fn notify_dispatch(
            &self,
            _db: Option<crate::db::Db>,
            _agent_id: String,
            _title: String,
            _card_id: String,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("notify:{dispatch_id}"));
            Ok(())
        }

        async fn handle_followup(
            &self,
            _db: Option<crate::db::Db>,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("followup:{dispatch_id}"));
            Ok(())
        }

        async fn sync_status_reaction(
            &self,
            _db: Option<crate::db::Db>,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("status_reaction:{dispatch_id}"));
            Ok(())
        }
    }

    /// #750: status_reaction outbox rows route through notifier.sync_status_reaction.
    /// The real notifier's sync is narrowed to write ❌ only for failed/cancelled
    /// dispatches (command bot's ⏳/✅ covers normal lifecycle); mock captures
    /// every invocation so we can assert the action is wired through.
    #[tokio::test]
    async fn process_outbox_batch_routes_status_reaction_through_notifier() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO dispatch_outbox (dispatch_id, action) VALUES ('dispatch-status', 'status_reaction')",
                [],
            )
            .unwrap();
        }

        let notifier = MockOutboxNotifier::default();
        let processed = process_outbox_batch(&db, &notifier).await;
        assert_eq!(processed, 1);
        assert_eq!(
            *notifier.calls.lock().unwrap(),
            vec!["status_reaction:dispatch-status".to_string()],
            "#750: status_reaction action must flow through notifier.sync_status_reaction"
        );

        let conn = db.lock().unwrap();
        let row: (String, Option<String>) = conn
            .query_row(
                "SELECT status, processed_at FROM dispatch_outbox WHERE dispatch_id = 'dispatch-status'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "done");
        assert!(row.1.is_some());
    }

    #[tokio::test]
    async fn handle_completed_dispatch_followups_with_pg_clears_done_card_threads() {
        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, active_thread_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-done")
        .bind("Done Card")
        .bind("done")
        .bind("thread-final")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-final")
        .bind("card-done")
        .bind("implementation")
        .bind("completed")
        .bind("Done Card")
        .bind("thread-final")
        .execute(&pool)
        .await
        .unwrap();

        handle_completed_dispatch_followups_with_pg(Some(&sqlite), Some(&pool), "dispatch-final")
            .await
            .unwrap();

        let active_thread: Option<String> =
            sqlx::query_scalar("SELECT active_thread_id FROM kanban_cards WHERE id = $1")
                .bind("card-done")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            active_thread.is_none(),
            "done-card followup should clear active_thread_id in postgres"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn handle_completed_dispatch_followups_with_transport_uses_transport_pg_pool() {
        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, active_thread_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-transport-pg")
        .bind("Transport PG Card")
        .bind("done")
        .bind("thread-transport")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-transport-pg")
        .bind("card-transport-pg")
        .bind("implementation")
        .bind("completed")
        .bind("Transport PG Card")
        .bind("thread-transport")
        .execute(&pool)
        .await
        .unwrap();

        let transport = PgAwareTransport::new(pool.clone());
        let config = DispatchFollowupConfig {
            discord_api_base: "http://127.0.0.1:9".to_string(),
            notify_bot_token: None,
            announce_bot_token: None,
        };

        handle_completed_dispatch_followups_with_config_and_transport(
            &sqlite,
            "dispatch-transport-pg",
            &config,
            &transport,
        )
        .await
        .unwrap();

        let active_thread: Option<String> =
            sqlx::query_scalar("SELECT active_thread_id FROM kanban_cards WHERE id = $1")
                .bind("card-transport-pg")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            active_thread.is_none(),
            "transport-backed PG followup should clear active_thread_id without SQLite mirroring"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn send_dispatch_with_transport_uses_transport_pg_pool_for_delivery_guard() {
        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let transport = PgAwareTransport::new(pool.clone());

        discord_delivery::send_dispatch_to_discord_with_transport(
            &sqlite,
            "agent-pg-transport",
            "Transport dispatch",
            "card-pg-transport",
            "dispatch-pg-transport",
            &transport,
        )
        .await
        .unwrap();

        assert_eq!(
            *transport.dispatch_calls.lock().unwrap(),
            vec!["agent-pg-transport:dispatch-pg-transport".to_string()]
        );

        let notified: Option<String> =
            sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
                .bind("dispatch_notified:dispatch-pg-transport")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            notified.as_deref(),
            Some("dispatch-pg-transport"),
            "delivery guard should persist notification state in postgres when transport carries a pool"
        );

        let sqlite_notified: Option<String> = sqlite
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                ["dispatch_notified:dispatch-pg-transport"],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(
            sqlite_notified.is_none(),
            "transport-backed PG delivery should not backfill SQLite guard keys"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn process_outbox_batch_with_pg_notify_transitions_dispatch_and_enqueues_reaction() {
        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (
                id, name, created_at, updated_at
             ) VALUES ($1, $2, NOW(), NOW())",
        )
        .bind("agent-pg")
        .bind("Agent PG")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-pg-notify")
        .bind("PG Notify Card")
        .bind("todo")
        .bind("agent-pg")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-pg-notify")
        .bind("card-pg-notify")
        .bind("agent-pg")
        .bind("implementation")
        .bind("pending")
        .bind("PG Notify Card")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO dispatch_outbox (
                dispatch_id, action, agent_id, card_id, title
             ) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind("dispatch-pg-notify")
        .bind("notify")
        .bind("agent-pg")
        .bind("card-pg-notify")
        .bind("PG Notify Card")
        .execute(&pool)
        .await
        .unwrap();

        let notifier = MockOutboxNotifier::default();
        let processed = process_outbox_batch_with_pg(Some(&sqlite), Some(&pool), &notifier).await;
        assert_eq!(processed, 1);
        assert_eq!(
            notifier.calls.lock().unwrap().as_slice(),
            ["notify:dispatch-pg-notify"]
        );

        let outbox_row: (String, Option<String>) = sqlx::query_as(
            "SELECT status, processed_at::text
               FROM dispatch_outbox
              WHERE dispatch_id = $1
                AND action = 'notify'",
        )
        .bind("dispatch-pg-notify")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(outbox_row.0, "done");
        assert!(outbox_row.1.is_some());

        let dispatch_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-pg-notify")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(dispatch_status, "dispatched");

        let reaction_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM dispatch_outbox
              WHERE dispatch_id = $1
                AND action = 'status_reaction'
                AND status = 'pending'",
        )
        .bind("dispatch-pg-notify")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(reaction_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn queue_dispatch_followup_pg_inserts_one_shot_row() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        queue_dispatch_followup_pg(&pool, "dispatch-pg-followup")
            .await
            .unwrap();
        queue_dispatch_followup_pg(&pool, "dispatch-pg-followup")
            .await
            .unwrap();

        let row: (String, String, String) = sqlx::query_as(
            "SELECT dispatch_id, action, status
               FROM dispatch_outbox
              WHERE dispatch_id = $1",
        )
        .bind("dispatch-pg-followup")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "dispatch-pg-followup");
        assert_eq!(row.1, "followup");
        assert_eq!(row.2, "pending");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM dispatch_outbox
              WHERE dispatch_id = $1
                AND action = 'followup'",
        )
        .bind("dispatch-pg-followup")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn requeue_dispatch_notify_pg_inserts_and_rearms_notify_row() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (
                id, name, created_at, updated_at
             ) VALUES ($1, $2, NOW(), NOW())",
        )
        .bind("agent-requeue")
        .bind("Agent Requeue")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-requeue")
        .bind("PG Requeue Card")
        .bind("todo")
        .bind("agent-requeue")
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-requeue")
        .bind("card-requeue")
        .bind("agent-requeue")
        .bind("implementation")
        .bind("pending")
        .bind("PG Requeue Card")
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            requeue_dispatch_notify_pg(&pool, "dispatch-requeue")
                .await
                .unwrap()
        );

        sqlx::query(
            "UPDATE dispatch_outbox
                SET status = 'failed',
                    retry_count = 3,
                    next_attempt_at = NOW() + INTERVAL '10 minutes',
                    processed_at = NOW(),
                    error = 'boom'
              WHERE dispatch_id = $1
                AND action = 'notify'",
        )
        .bind("dispatch-requeue")
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            requeue_dispatch_notify_pg(&pool, "dispatch-requeue")
                .await
                .unwrap()
        );

        let row: (
            String,
            String,
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT agent_id, card_id, title, status, retry_count,
                        next_attempt_at::text, processed_at::text, error
                   FROM dispatch_outbox
                  WHERE dispatch_id = $1
                    AND action = 'notify'",
        )
        .bind("dispatch-requeue")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "agent-requeue");
        assert_eq!(row.1, "card-requeue");
        assert_eq!(row.2, "PG Requeue Card");
        assert_eq!(row.3, "pending");
        assert_eq!(row.4, 0);
        assert!(row.5.is_none());
        assert!(row.6.is_none());
        assert!(row.7.is_none());

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM dispatch_outbox
              WHERE dispatch_id = $1
                AND action = 'notify'",
        )
        .bind("dispatch-requeue")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn queue_dispatch_followup_sync_prefers_postgres_when_available() {
        let sqlite = test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        queue_dispatch_followup_sync(&sqlite, Some(&pool), "dispatch-sync-followup");
        queue_dispatch_followup_sync(&sqlite, Some(&pool), "dispatch-sync-followup");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM dispatch_outbox
              WHERE dispatch_id = $1
                AND action = 'followup'",
        )
        .bind("dispatch-sync-followup")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);

        pool.close().await;
        pg_db.drop().await;
    }
}
