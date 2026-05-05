//! Dispatch outbox route orchestration and message shaping.
//!
//! Owns the completed-dispatch followup orchestration and dispatch message
//! rendering helpers that used to live in
//! `src/server/routes/dispatches/outbox.rs`. The route module now re-exports
//! this service surface so existing callers keep their paths while route-level
//! code stays declarative.
//!
//! - `crate::services::dispatches::outbox_queue` — `OutboxNotifier`,
//!   `RealOutboxNotifier`, `process_outbox_batch_*`, `dispatch_outbox_loop`.
//! - `crate::db::dispatches::outbox` — `claim_pending_dispatch_outbox_batch_pg`,
//!   `mark_outbox_*`, `requeue_dispatch_notify_pg`,
//!   `load_completed_dispatch_info_pg`, etc.

use sqlx::PgPool;

use crate::db::dispatches::outbox::CompletedDispatchInfo;
use crate::server::dto::dispatches::DispatchFollowupConfig;
use crate::server::routes::dispatches::discord_delivery::{
    DispatchTransport, HttpDispatchTransport, discord_api_url,
    send_review_result_to_primary_with_transport,
};
use crate::services::git::GitCommand;

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

// ── Followup & verdict helpers ──────────────────────────────────

pub(crate) fn extract_review_verdict(result_json: Option<&str>) -> String {
    parse_json_value(result_json, "result_json")
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

pub(crate) fn parse_json_value(
    raw: Option<&str>,
    field_name: &'static str,
) -> Option<serde_json::Value> {
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
    GitCommand::new()
        .repo(repo_dir)
        .args(["rev-parse", "--verify", git_ref])
        .run_output()
        .map(|_| true)
        .unwrap_or(false)
}

fn resolve_upstream_base_ref(repo_dir: &str) -> Option<String> {
    ["origin/main", "main", "origin/master", "master"]
        .into_iter()
        .find(|candidate| git_ref_exists(repo_dir, candidate))
        .map(str::to_string)
}

fn git_diff_stats(repo_dir: &str, diff_spec: &str) -> Result<DispatchChangeStats, String> {
    let output = GitCommand::new()
        .repo(repo_dir)
        .args(["diff", "--numstat", "--find-renames", diff_spec])
        .run_output()
        .map_err(|err| format!("git diff {diff_spec} failed: {err}"))?;

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
        return match GitCommand::new()
            .repo(repo_dir)
            .args(["merge-base", "--is-ancestor", completed_commit, &base_ref])
            .run_output()
        {
            Ok(_) => DispatchMergeStatus::Merged,
            // Exit 1 is git merge-base's ordinary "not an ancestor" result,
            // which means the dispatch has not reached the upstream base yet.
            Err(error) if error.status_code() == Some(1) => DispatchMergeStatus::Pending,
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
    use crate::services::discord::outbound::HttpOutboundClient;
    use crate::services::discord::outbound::delivery::deliver_outbound;
    use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
    use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
    use crate::services::discord::outbound::result::DeliveryResult;
    use poise::serenity_prelude::ChannelId;

    let Some(token) = config.notify_bot_token.as_deref() else {
        return Err("no notify bot token".to_string());
    };

    let client = reqwest::Client::new();
    ensure_thread_is_postable(&client, token, &config.discord_api_base, thread_id).await?;

    let target_channel_id = thread_id
        .parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| format!("invalid dispatch summary thread id {thread_id}: {error}"))?;
    let outbound_client =
        HttpOutboundClient::new(client, token.to_string(), config.discord_api_base.clone());
    let outbound_msg = DiscordOutboundMessage::new(
        format!("dispatch:{dispatch_id}"),
        format!("dispatch:{dispatch_id}:completion-summary"),
        message,
        OutboundTarget::Channel(target_channel_id),
        DiscordOutboundPolicy::review_notification(),
    );

    match deliver_outbound(
        &outbound_client,
        dispatch_completion_summary_deduper(),
        outbound_msg,
    )
    .await
    {
        DeliveryResult::Sent { .. }
        | DeliveryResult::Fallback { .. }
        | DeliveryResult::Duplicate { .. }
        | DeliveryResult::Skip { .. } => Ok(()),
        DeliveryResult::PermanentFailure { reason } => Err(format!(
            "failed to post dispatch summary for {dispatch_id}: {reason}"
        )),
    }
}

fn dispatch_completion_summary_deduper()
-> &'static crate::services::discord::outbound::OutboundDeduper {
    static DEDUPER: std::sync::OnceLock<crate::services::discord::outbound::OutboundDeduper> =
        std::sync::OnceLock::new();
    DEDUPER.get_or_init(crate::services::discord::outbound::OutboundDeduper::new)
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
            send_review_result_to_primary_with_transport(
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
            if should_defer_done_card_thread_archive(pg_pool, tid, dispatch_id).await? {
                return Err(format!(
                    "defer completed dispatch followups for {dispatch_id}: thread {tid} still has an active turn"
                ));
            }
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
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<Option<CompletedDispatchInfo>, String> {
    if let Some(pool) = pg_pool {
        return crate::db::dispatches::outbox::load_completed_dispatch_info_pg(pool, dispatch_id)
            .await;
    }

    Err("dispatch lookup requires postgres pool".to_string())
}

async fn load_card_status(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
) -> Result<Option<String>, String> {
    if let Some(pool) = pg_pool {
        return crate::db::dispatches::outbox::load_card_status_pg(pool, card_id).await;
    }

    Err("card status lookup requires postgres pool".to_string())
}

async fn should_defer_done_card_thread_archive(
    pg_pool: Option<&PgPool>,
    thread_id: &str,
    _dispatch_id: &str,
) -> Result<bool, String> {
    crate::server::routes::dispatches::should_defer_thread_archive_pg(pg_pool, thread_id).await
}

async fn clear_all_dispatch_threads(
    _db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    card_id: &str,
) -> Result<(), String> {
    if let Some(pool) = pg_pool {
        return crate::db::dispatches::outbox::clear_all_dispatch_threads_pg(pool, card_id).await;
    }

    Err("thread cleanup requires postgres pool".to_string())
}

// ── Channel helpers ─────────────────────────────────────────────

/// Resolve a channel name alias (e.g. "adk-cc") to a numeric channel ID.
/// Public wrapper around the shared resolve_channel_alias.
pub fn resolve_channel_alias_pub(alias: &str) -> Option<u64> {
    crate::server::routes::dispatches::resolve_channel_alias(alias)
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
pub(crate) const DISPATCH_MESSAGE_HARD_LIMIT: usize = 1800;
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

pub(crate) fn review_target_hint(
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

pub(crate) fn review_submission_hint(
    dispatch_type: Option<&str>,
    dispatch_id: &str,
    context_json: &serde_json::Value,
) -> Option<String> {
    match dispatch_type {
        Some("review") => Some(format!(
            "제출: `{}` (`dispatch_id={dispatch_id}`)",
            trim_context_string(context_json, "verdict_endpoint")
                .unwrap_or("POST /api/reviews/verdict")
        )),
        Some("review-decision") => Some(format!(
            "제출: `{}`",
            trim_context_string(context_json, "decision_endpoint")
                .unwrap_or("POST /api/reviews/decision")
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

pub(crate) fn build_minimal_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> String {
    let context_json = parse_json_value(dispatch_context, "dispatch_context")
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

pub(crate) fn format_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> String {
    let context_json = parse_json_value(dispatch_context, "dispatch_context")
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

pub(crate) fn prefix_dispatch_message(dispatch_type: &str, message: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_review_verdict_requires_explicit_verdict_or_decision() {
        assert_eq!(extract_review_verdict(None), "unknown");
        assert_eq!(
            extract_review_verdict(Some(r#"{"auto_completed":true}"#)),
            "unknown"
        );
        assert_eq!(
            extract_review_verdict(Some(r#"{"decision":"dismiss"}"#)),
            "dismiss"
        );
        assert_eq!(
            extract_review_verdict(Some(r#"{"verdict":"improve"}"#)),
            "improve"
        );
    }

    #[test]
    fn format_dispatch_message_preserves_issue_target_and_instruction() {
        let message = format_dispatch_message(
            "dispatch-1",
            "P2-A: Thin routes/dispatches/outbox.rs",
            Some("https://github.com/itismyfield/AgentDesk/issues/1722"),
            Some(1722),
            Some("implementation"),
            Some(r#"{"auto_queue":true}"#),
        );

        assert!(message.starts_with("── implementation dispatch ──\n"));
        assert!(message.contains("DISPATCH:dispatch-1 [📋 구현] - #1722"));
        assert!(message.contains("<https://github.com/itismyfield/AgentDesk/issues/1722>"));
        assert!(message.contains("한 줄 지시: 이 이슈를 구현"));
        assert!(message.contains("(auto-queue)"));
        assert!(message.chars().count() <= DISPATCH_MESSAGE_HARD_LIMIT);
    }
}
