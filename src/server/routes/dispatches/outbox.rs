use super::{
    discord_delivery::{discord_api_base_url, discord_api_url},
    thread_reuse::clear_all_threads,
};
use rusqlite::OptionalExtension;
use std::process::Command;

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
        db: crate::db::Db,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    fn handle_followup(
        &self,
        db: crate::db::Db,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    fn sync_status_reaction(
        &self,
        db: crate::db::Db,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Production notifier that calls the real Discord functions.
pub(crate) struct RealOutboxNotifier;

impl OutboxNotifier for RealOutboxNotifier {
    async fn notify_dispatch(
        &self,
        db: crate::db::Db,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> Result<(), String> {
        super::discord_delivery::send_dispatch_to_discord(
            &db,
            &agent_id,
            &title,
            &card_id,
            &dispatch_id,
        )
        .await
    }

    async fn handle_followup(&self, db: crate::db::Db, dispatch_id: String) -> Result<(), String> {
        handle_completed_dispatch_followups(&db, &dispatch_id).await
    }

    async fn sync_status_reaction(
        &self,
        db: crate::db::Db,
        dispatch_id: String,
    ) -> Result<(), String> {
        super::discord_delivery::sync_dispatch_status_reaction(&db, &dispatch_id).await
    }
}

/// Backoff delays for outbox retries: 1m → 5m → 15m → 1h
const RETRY_BACKOFF_SECS: [i64; 4] = [60, 300, 900, 3600];
/// Maximum number of retries before marking as permanent failure.
const MAX_RETRY_COUNT: i32 = 4;

fn dispatch_notify_delivery_suppressed(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> rusqlite::Result<bool> {
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
    let pending: Vec<(
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i32,
    )> = {
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
            let suppress_delivery = if let Ok(conn) = db.lock() {
                dispatch_notify_delivery_suppressed(&conn, &dispatch_id).unwrap_or(false)
            } else {
                false
            };
            if suppress_delivery {
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "UPDATE dispatch_outbox SET status = 'done', processed_at = datetime('now'), error = NULL WHERE id = ?1",
                        [id],
                    )
                    .ok();
                }
                continue;
            }
        }

        // Mark as processing
        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE dispatch_outbox SET status = 'processing' WHERE id = ?1",
                [id],
            )
            .ok();
        }

        let result = match action.as_str() {
            "notify" => {
                if let (Some(aid), Some(cid), Some(t)) =
                    (agent_id.clone(), card_id.clone(), title.clone())
                {
                    // Two-phase delivery guard (reservation + notified marker) is handled
                    // inside send_dispatch_to_discord, protecting all callers uniformly.
                    notifier
                        .notify_dispatch(db.clone(), aid, t, cid, dispatch_id.clone())
                        .await
                } else {
                    Err("missing agent_id, card_id, or title for notify action".into())
                }
            }
            "followup" => {
                notifier
                    .handle_followup(db.clone(), dispatch_id.clone())
                    .await
            }
            "status_reaction" => {
                notifier
                    .sync_status_reaction(db.clone(), dispatch_id.clone())
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
            Err(err) => {
                let new_count = retry_count + 1;
                if new_count > MAX_RETRY_COUNT {
                    // Permanent failure — exhausted all 4 retries (1m → 5m → 15m → 1h)
                    tracing::error!(
                        "[dispatch-outbox] Permanent failure for entry {id} (dispatch={dispatch_id}, action={action}): {err}"
                    );
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox SET status = 'failed', error = ?1, \
                             retry_count = ?2, processed_at = datetime('now') WHERE id = ?3",
                            rusqlite::params![err, new_count, id],
                        )
                        .ok();
                    }
                } else {
                    // Schedule retry with backoff (index = new_count - 1, since retry 1 uses BACKOFF[0])
                    let backoff_idx = (new_count - 1) as usize;
                    let backoff_secs = RETRY_BACKOFF_SECS.get(backoff_idx).copied().unwrap_or(3600);
                    tracing::warn!(
                        "[dispatch-outbox] Retry {new_count}/{MAX_RETRY_COUNT} for entry {id} (dispatch={dispatch_id}, action={action}) \
                         in {backoff_secs}s: {err}",
                    );
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox SET status = 'pending', error = ?1, \
                             retry_count = ?2, \
                             next_attempt_at = datetime('now', '+' || ?3 || ' seconds') \
                             WHERE id = ?4",
                            rusqlite::params![err, new_count, backoff_secs, id],
                        )
                        .ok();
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

fn parse_json_value(raw: Option<&str>) -> Option<serde_json::Value> {
    raw.and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
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

    let result_json = parse_json_value(info.result_json.as_deref());
    let context_json = parse_json_value(info.context_json.as_deref());
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
    handle_completed_dispatch_followups_with_config(
        db,
        dispatch_id,
        &DispatchFollowupConfig::from_runtime(),
    )
    .await
}

pub(crate) async fn handle_completed_dispatch_followups_with_config(
    db: &crate::db::Db,
    dispatch_id: &str,
    config: &DispatchFollowupConfig,
) -> Result<(), String> {
    let info: Option<CompletedDispatchInfo> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for dispatch lookup".into()),
        };
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
        .ok()
    };

    let Some(mut info) = info else {
        return Err(format!("dispatch {dispatch_id} not found"));
    };
    if info.status != "completed" {
        return Ok(()); // Not an error — dispatch not yet completed
    }
    let context_json_value = parse_json_value(info.context_json.as_deref());
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
            super::discord_delivery::send_review_result_to_primary(
                db,
                &info.card_id,
                dispatch_id,
                &verdict,
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
    let card_status: Option<String> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&info.card_id],
            |row| row.get(0),
        )
        .ok()
    });
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
        // Clear all thread mappings when card is done
        if let Ok(conn) = db.lock() {
            clear_all_threads(&conn, &info.card_id);
        }
    }

    // Generic resend removed — dispatch Discord notification is handled by:
    // 1. kanban.rs fire_transition_hooks → onCardTransition → send_dispatch_to_discord
    // 2. timeouts.js [I-0] recovery for unnotified dispatches
    // 3. dispatch_notified guard in process_outbox_batch prevents duplicates
    // Previously this generic resend caused 2-3x duplicate messages for every dispatch.
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

fn review_quality_checklist(context_json: &serde_json::Value) -> Vec<String> {
    let checklist = context_json
        .get("review_quality_checklist")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if checklist.is_empty() {
        crate::dispatch::REVIEW_QUALITY_CHECKLIST
            .iter()
            .map(|item| (*item).to_string())
            .collect()
    } else {
        checklist
    }
}

// ── Message formatting ──────────────────────────────────────────

pub(super) fn format_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    use_alt: bool,
    reviewed_commit: Option<&str>,
    target_provider: Option<&str>,
    review_branch: Option<&str>,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> String {
    let context_json = dispatch_context
        .and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    // Format issue link as markdown hyperlink with angle brackets to suppress embed
    let issue_link = match (issue_url, issue_number) {
        (Some(url), Some(num)) => format!("[{title} #{num}](<{url}>)"),
        (Some(url), None) => format!("[{title}](<{url}>)"),
        _ => String::new(),
    };

    // Build dispatch type label and reason line
    let type_label = match dispatch_type {
        Some("implementation") => "📋 구현",
        Some("review") => "🔍 리뷰",
        Some("rework") => "🔧 리워크",
        Some("review-decision") => "⚖️ 리뷰 검토",
        Some("pm-decision") => "🎯 PM 판단",
        Some("e2e-test") => "🧪 E2E 테스트",
        Some(other) => other,
        None => "dispatch",
    };

    // Extract reason from context JSON
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

    let reason_suffix = reason.map(|r| format!(" ({r})")).unwrap_or_default();
    let review_verdict = context_json
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let review_mode = context_json
        .get("review_mode")
        .and_then(|value| value.as_str());
    let noop_verification = review_mode == Some("noop_verification");

    if dispatch_type == Some("review") {
        let mut message = format!(
            "DISPATCH:{dispatch_id} [{type_label}] - {title}\n\
             ⚠️ 검토 전용 — 작업 착수 금지\n\
             코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        );
        if !issue_link.is_empty() {
            message.push('\n');
            message.push_str(&issue_link);
        }
        // #193: Include branch info so reviewer inspects the correct code
        if let Some(branch) = review_branch {
            let short_commit = reviewed_commit.map(|c| &c[..8.min(c.len())]).unwrap_or("?");
            message.push_str(&format!(
                "\n\n리뷰 대상 브랜치: `{branch}` (commit: `{short_commit}`)\n\
                 반드시 해당 브랜치를 checkout하여 리뷰하세요. main 브랜치가 아닙니다."
            ));
            if !noop_verification {
                if let (Some(merge_base), Some(reviewed_commit)) = (
                    context_json
                        .get("merge_base")
                        .and_then(|value| value.as_str()),
                    reviewed_commit,
                ) {
                    message.push_str(&format!(
                        "\n\
                         merge-base(main, `{branch}`): `{merge_base}`\n\
                         정확한 변경 범위는 아래 명령으로 확인하세요:\n\
                         ```bash\n\
                         git diff {merge_base}..{reviewed_commit}\n\
                         ```"
                    ));
                }
            } else {
                message.push_str(&format!(
                    "\n\
                     이번 리뷰는 diff 검토가 아니라 현재 브랜치 상태 검증입니다. `git diff`보다 이슈 본문과 실제 코드 상태 대조를 우선하세요."
                ));
            }
        }
        if let Some(warning) = context_json
            .get("review_target_warning")
            .and_then(|value| value.as_str())
        {
            message.push_str(&format!("\n\n리뷰 타겟 안내: {warning}"));
        }
        if noop_verification {
            let noop_reason = context_json
                .get("noop_reason")
                .and_then(|value| value.as_str())
                .or_else(|| {
                    context_json
                        .get("noop_result")
                        .and_then(|value| value.get("notes"))
                        .and_then(|value| value.as_str())
                })
                .unwrap_or("noop 사유가 제공되지 않았습니다.");
            message.push_str(&format!(
                "\n\n리뷰 모드: `noop_verification`\n\
                 이번 리뷰는 코드 diff 대신 GitHub 이슈 본문과 현재 코드 상태를 대조하는 검증입니다.\n\
                 noop 사유: {noop_reason}\n\
                 반드시 아래 항목을 판정하세요:\n\
                 - 이슈가 요구한 변경이 현재 코드에 이미 존재하는지\n\
                 - noop 사유가 구체적이고 직접 검증 가능한지\n\
                 둘 중 하나라도 아니면 `VERDICT: reject` 또는 `VERDICT: rework`로 판정하세요."
            ));
        }
        let review_scope_reminder = context_json
            .get("review_quality_scope_reminder")
            .and_then(|value| value.as_str())
            .unwrap_or(crate::dispatch::REVIEW_QUALITY_SCOPE_REMINDER);
        let review_verdict_guidance = context_json
            .get("review_verdict_guidance")
            .and_then(|value| value.as_str())
            .unwrap_or(crate::dispatch::REVIEW_VERDICT_IMPROVE_GUIDANCE);
        let quality_checklist = review_quality_checklist(&context_json);
        message.push_str(&format!("\n\n{review_scope_reminder}"));
        for item in quality_checklist {
            message.push_str(&format!("\n- {item}"));
        }
        message.push_str(&format!("\n{review_verdict_guidance}"));
        // Append verdict API call instructions for the counter-model reviewer
        let commit_arg = reviewed_commit
            .map(|c| format!(r#","commit":"{}""#, c))
            .unwrap_or_default();
        let provider_arg = target_provider
            .map(|p| format!(r#","provider":"{}""#, p))
            .unwrap_or_default();
        let base_url = crate::config::local_api_url(crate::config::load_graceful().server.port, "");
        message.push_str(&format!(
            "\n---\n\
             응답 첫 줄에 반드시 `VERDICT: pass|improve|reject|rework` 중 하나를 적으세요.\n\
             verdict API가 200 OK로 호출되기 전까지 리뷰는 완료로 간주되지 않습니다.\n\
             `improve`/`reject`/`rework` 시 반드시 `notes`에 구체적 피드백을, `items`에 개별 지적 사항을 포함하세요.\n\
             리뷰 완료 후 verdict API를 호출하세요:\n\
             `curl -sf -X POST {base_url}/api/review-verdict \
             -H \"Content-Type: application/json\" \
             -d '{{\"dispatch_id\":\"{dispatch_id}\",\"overall\":\"pass|improve|reject|rework\",\
             \"notes\":\"리뷰 피드백 요약\",\
             \"items\":[{{\"category\":\"bug|style|perf|security|logic\",\"summary\":\"개별 지적 사항\"}}]\
             {commit_arg}{provider_arg}}}'`"
        ));
        message
    } else if dispatch_type == Some("review-decision") {
        let mut message = format!(
            "DISPATCH:{dispatch_id} [{type_label}] - {title}\n\
             ⛔ 코드 리뷰 금지 — 이미 완료된 리뷰 결과를 검토하는 단계입니다\n\
             📝 카운터모델 리뷰 결과: **{review_verdict}**\n\
             GitHub 이슈 코멘트에서 피드백을 확인하고 다음 중 하나를 선택하세요:\n\
             • **수용** → 피드백 반영 수정 후 review-decision API에 `accept` 호출\n\
             • **반론** → GitHub 코멘트로 이의 제기 후 review-decision API에 `dispute` 호출\n\
             • **무시** → review-decision API에 `dismiss` 호출"
        );
        if !issue_link.is_empty() {
            message.push('\n');
            message.push_str(&issue_link);
        }
        message
    } else if matches!(dispatch_type, Some("implementation") | Some("rework")) {
        let mut message = if !issue_link.is_empty() {
            format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}\n{issue_link}")
        } else {
            format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}")
        };
        message.push_str(
            "\n\n구현이 불필요하고 현재 worktree에 tracked 변경이 전혀 없을 때만 응답 첫 줄에 반드시 `OUTCOME: noop`를 적고 근거를 설명하세요.\n\
             tracked 변경이 남아 있으면 noop 완료가 거부되므로 먼저 commit 또는 정리를 해야 합니다.\n\
             이 marker가 있으면 일반 완료 대신 non-implementation terminal path로 처리됩니다.\n\
             \n\
             커밋 메시지에 반드시 GitHub 이슈 번호를 포함하세요 (예: `#123 구현 내용`). 이슈-커밋 추적성을 위해 필수입니다.",
        );
        message
    } else if use_alt {
        let mut message = if !issue_link.is_empty() {
            format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}\n{issue_link}")
        } else {
            format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}")
        };
        let base_url = crate::config::local_api_url(crate::config::load_graceful().server.port, "");
        message.push_str(&format!(
            "\n\n작업을 마치면 일반 dispatch 완료 API로 종료하세요.\n\
             리뷰 전용 verdict 절차를 쓰지 말고 아래 완료 경로를 그대로 사용하세요.\n\
             완료 예시:\n\
             `curl -sf -X PATCH {base_url}/api/dispatches/{dispatch_id} \
             -H \"Content-Type: application/json\" \
             -d '{{\"status\":\"completed\",\"result\":{{\"summary\":\"결과 요약\"}}}}'`"
        ));
        message
    } else if !issue_link.is_empty() {
        format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}\n{issue_link}")
    } else {
        format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}")
    }
}

pub(super) fn prefix_dispatch_message(dispatch_type: &str, message: &str) -> String {
    format!("── {} dispatch ──\n{}", dispatch_type, message)
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

/// Worker loop that drains dispatch_outbox and executes Discord side-effects.
///
/// This is the SINGLE place where dispatch-related Discord HTTP calls originate.
/// All other code paths insert into the outbox table and return immediately.
pub(crate) async fn dispatch_outbox_loop(db: crate::db::Db) {
    use std::time::Duration;

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[dispatch-outbox] Worker started (adaptive backoff 500ms-5s)");

    let notifier = RealOutboxNotifier;
    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;

        let processed = process_outbox_batch(&db, &notifier).await;
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
    use std::sync::{Arc, Mutex};

    fn test_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[derive(Clone, Default)]
    struct MockOutboxNotifier {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl OutboxNotifier for MockOutboxNotifier {
        async fn notify_dispatch(
            &self,
            _db: crate::db::Db,
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
            _db: crate::db::Db,
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
            _db: crate::db::Db,
            dispatch_id: String,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("status_reaction:{dispatch_id}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn process_outbox_batch_handles_status_reaction_action() {
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
            notifier.calls.lock().unwrap().as_slice(),
            ["status_reaction:dispatch-status"]
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
}
