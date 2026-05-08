//! GitHub issue state sync: keep kanban cards consistent with GitHub issue state.

use chrono::{Duration, NaiveDate, Utc};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};

const ISSUE_JSON_FIELDS: &str =
    "number,state,title,labels,body,url,closedAt,closedByPullRequestsReferences";
const PRIMARY_FETCH_LIMIT: u32 = 100;
const RECENTLY_CLOSED_FETCH_LIMIT: u32 = 50;
const RECENTLY_CLOSED_LOOKBACK_DAYS: i64 = 30;

/// Represents a GitHub issue as returned by `gh issue list --json`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GhIssue {
    pub number: i64,
    pub state: String,
    pub title: String,
    #[serde(default)]
    pub labels: Vec<GhLabel>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "closedAt")]
    pub closed_at: Option<String>,
    #[serde(default, rename = "closedByPullRequestsReferences")]
    pub closed_by_pull_requests_references: Vec<GhPullRequestReference>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GhLabel {
    pub name: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GhPullRequestReference {
    pub number: Option<i64>,
    pub url: Option<String>,
}

/// Fetch open issues for a repo via `gh` CLI.
/// Returns parsed issues or an error if `gh` is unavailable / fails.
pub fn fetch_issues(repo: &str) -> Result<Vec<GhIssue>, String> {
    fetch_issues_with(super::adapter(), repo)
}

pub(crate) fn fetch_issues_with(
    adapter: &dyn super::GitHubAdapter,
    repo: &str,
) -> Result<Vec<GhIssue>, String> {
    fetch_issues_with_cutoff(adapter, repo, Utc::now().date_naive())
}

fn fetch_issues_with_cutoff(
    adapter: &dyn super::GitHubAdapter,
    repo: &str,
    today: NaiveDate,
) -> Result<Vec<GhIssue>, String> {
    let mut issues = fetch_issue_batch(adapter, repo, "all", PRIMARY_FETCH_LIMIT, None)?;
    let recent_closed_search = recent_closed_search_query(today);
    let recent_closed = fetch_issue_batch(
        adapter,
        repo,
        "closed",
        RECENTLY_CLOSED_FETCH_LIMIT,
        Some(recent_closed_search.as_str()),
    )?;
    merge_unique_issues(&mut issues, recent_closed);
    Ok(issues)
}

fn fetch_issue_batch(
    adapter: &dyn super::GitHubAdapter,
    repo: &str,
    state: &str,
    limit: u32,
    search: Option<&str>,
) -> Result<Vec<GhIssue>, String> {
    let limit_text = limit.to_string();
    let mut args = vec![
        "issue",
        "list",
        "--repo",
        repo,
        "--json",
        ISSUE_JSON_FIELDS,
        "--limit",
        limit_text.as_str(),
        "--state",
        state,
    ];
    if let Some(search) = search {
        args.extend(["--search", search]);
    }

    let output = adapter.run(&args)?;
    serde_json::from_str::<Vec<GhIssue>>(&output)
        .map_err(|e| format!("failed to parse gh output: {e}"))
}

fn recent_closed_search_query(today: NaiveDate) -> String {
    let cutoff = today - Duration::days(RECENTLY_CLOSED_LOOKBACK_DAYS);
    format!("closed:>{} sort:updated-desc", cutoff.format("%Y-%m-%d"))
}

fn merge_unique_issues(target: &mut Vec<GhIssue>, extras: Vec<GhIssue>) {
    let mut seen: HashSet<i64> = target.iter().map(|issue| issue.number).collect();
    target.extend(extras.into_iter().filter(|issue| seen.insert(issue.number)));
}

fn mainline_issue_numbers_for_repo(repo: &str) -> Vec<i64> {
    let repo_dir = match crate::services::platform::shell::resolve_repo_dir_for_target(Some(repo)) {
        Ok(Some(repo_dir)) => repo_dir,
        Ok(None) => {
            tracing::debug!("[github-sync] {repo}: repo dir unavailable for mainline sync");
            return Vec::new();
        }
        Err(error) => {
            tracing::warn!(
                "[github-sync] {repo}: repo dir resolution failed for mainline sync: {error}"
            );
            return Vec::new();
        }
    };

    match crate::services::platform::shell::git_mainline_issue_numbers(&repo_dir) {
        Ok(numbers) => numbers,
        Err(error) => {
            tracing::warn!("[github-sync] {repo}: mainline commit scan failed: {error}");
            Vec::new()
        }
    }
}

/// Sync GitHub issue state with kanban cards for a single repo.
///
/// - If a linked issue is CLOSED on GitHub -> update card to "done"
/// - If a linked issue is OPEN but card is "done" -> log inconsistency
///
/// Returns (closed_count, inconsistency_count).
pub fn sync_github_issues_for_repo(
    _db: &crate::db::Db,
    engine: &crate::engine::PolicyEngine,
    repo: &str,
    issues: &[GhIssue],
) -> Result<SyncResult, String> {
    let pool = engine
        .pg_pool()
        .ok_or_else(|| "postgres backend required for GitHub issue sync".to_string())?;
    let repo = repo.to_string();
    let issues = issues.to_vec();
    crate::utils::async_bridge::block_on_pg_result(
        pool,
        move |bridge_pool| async move {
            sync_github_issues_for_repo_pg(&bridge_pool, &repo, &issues).await
        },
        |error| error,
    )
}

#[derive(Debug, Clone)]
struct PgCardRecord {
    id: String,
    status: String,
    review_status: Option<String>,
    latest_dispatch_id: Option<String>,
    assigned_agent_id: Option<String>,
}

/// PostgreSQL variant of GitHub issue state sync for a single repo.
pub async fn sync_github_issues_for_repo_pg(
    pool: &PgPool,
    repo: &str,
    issues: &[GhIssue],
) -> Result<SyncResult, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = load_pg_repo_override(pool, repo).await?;
    let mut agent_overrides: HashMap<String, Option<crate::pipeline::PipelineOverride>> =
        HashMap::new();
    let mut result = SyncResult::default();

    for issue in issues {
        let cards = load_pg_cards_for_issue(pool, repo, issue.number).await?;

        for card in cards {
            if let Some(ref body) = issue.body {
                let trimmed = body.trim_end();
                sqlx::query(
                    "UPDATE kanban_cards
                     SET description = $1, updated_at = NOW()
                     WHERE id = $2 AND (description IS NULL OR description != $1)",
                )
                .bind(trimmed)
                .bind(&card.id)
                .execute(pool)
                .await
                .map_err(|error| format!("update description for {}: {error}", card.id))?;
            }

            let pipeline = resolve_pg_pipeline(
                pool,
                repo,
                repo_override.as_ref(),
                card.assigned_agent_id.as_deref(),
                &mut agent_overrides,
            )
            .await?;
            let is_terminal = pipeline.is_terminal(&card.status);

            if issue.state == "CLOSED" && !is_terminal {
                close_pg_card_for_issue(pool, &card, &pipeline).await?;
                result.closed_count += 1;
                tracing::info!(
                    "[github-sync] {repo}#{}: card {} → terminal (issue closed)",
                    issue.number,
                    card.id
                );
            } else if issue.state == "OPEN" && is_terminal {
                result.inconsistency_count += 1;
                tracing::warn!(
                    "[github-sync] {repo}#{}: card {} is terminal but issue is OPEN",
                    issue.number,
                    card.id
                );
            }
        }

        if issue.state == "CLOSED" {
            let event = issue_completion_event(repo, issue);
            if let Err(error) =
                crate::services::issue_announcements::complete_issue_announcement_pg(pool, event)
                    .await
            {
                tracing::warn!(
                    "[github-sync] {repo}#{}: issue announcement completion edit failed: {error}",
                    issue.number
                );
            }
        }
    }

    let mainline_issue_numbers = mainline_issue_numbers_for_repo(repo);
    if !mainline_issue_numbers.is_empty() {
        let mut mainline_done_count = 0usize;

        for issue_number in mainline_issue_numbers {
            let cards = load_pg_cards_for_issue(pool, repo, issue_number).await?;

            for card in cards {
                if !matches!(card.status.as_str(), "in_progress" | "review") {
                    continue;
                }

                let pipeline = resolve_pg_pipeline(
                    pool,
                    repo,
                    repo_override.as_ref(),
                    card.assigned_agent_id.as_deref(),
                    &mut agent_overrides,
                )
                .await?;
                close_pg_card_for_issue(pool, &card, &pipeline).await?;
                mainline_done_count += 1;
                tracing::info!(
                    "[github-sync] {repo}#{}: card {} → terminal (mainline commit matched issue)",
                    issue_number,
                    card.id
                );
            }

            if let Err(error) =
                crate::services::issue_announcements::complete_issue_announcement_pg(
                    pool,
                    crate::services::issue_announcements::IssueCompletionEvent {
                        repo: repo.to_string(),
                        issue_number,
                        title: None,
                        kind: crate::services::issue_announcements::IssueCompletionKind::Merged,
                        pr_number: None,
                        pr_url: None,
                    },
                )
                .await
            {
                tracing::warn!(
                    "[github-sync] {repo}#{issue_number}: mainline issue announcement completion edit failed: {error}"
                );
            }
        }

        if mainline_done_count > 0 {
            tracing::info!(
                "[github-sync] {repo}: mainline commit sync completed for {} card(s)",
                mainline_done_count
            );
        }
    }

    sqlx::query("UPDATE github_repos SET last_synced_at = NOW() WHERE id = $1")
        .bind(repo)
        .execute(pool)
        .await
        .map_err(|error| format!("update last_synced_at: {error}"))?;

    Ok(result)
}

fn issue_completion_event(
    repo: &str,
    issue: &GhIssue,
) -> crate::services::issue_announcements::IssueCompletionEvent {
    let first_pr = issue.closed_by_pull_requests_references.first();
    let kind = if first_pr.is_some() {
        crate::services::issue_announcements::IssueCompletionKind::Merged
    } else {
        crate::services::issue_announcements::IssueCompletionKind::Closed
    };
    crate::services::issue_announcements::IssueCompletionEvent {
        repo: repo.to_string(),
        issue_number: issue.number,
        title: Some(issue.title.clone()),
        kind,
        pr_number: first_pr.and_then(|pr| pr.number),
        pr_url: first_pr.and_then(|pr| pr.url.clone()),
    }
}

async fn load_pg_cards_for_issue(
    pool: &PgPool,
    repo: &str,
    issue_number: i64,
) -> Result<Vec<PgCardRecord>, String> {
    let rows = sqlx::query(
        "SELECT id, status, review_status, latest_dispatch_id, assigned_agent_id
         FROM kanban_cards
         WHERE github_issue_number = $1 AND repo_id = $2",
    )
    .bind(issue_number)
    .bind(repo)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load cards for {repo}#{issue_number}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(PgCardRecord {
                id: row
                    .try_get("id")
                    .map_err(|error| format!("read card id: {error}"))?,
                status: row
                    .try_get("status")
                    .map_err(|error| format!("read card status: {error}"))?,
                review_status: row
                    .try_get("review_status")
                    .map_err(|error| format!("read review_status: {error}"))?,
                latest_dispatch_id: row
                    .try_get("latest_dispatch_id")
                    .map_err(|error| format!("read latest_dispatch_id: {error}"))?,
                assigned_agent_id: row
                    .try_get("assigned_agent_id")
                    .map_err(|error| format!("read assigned_agent_id: {error}"))?,
            })
        })
        .collect()
}

async fn load_pg_repo_override(
    pool: &PgPool,
    repo: &str,
) -> Result<Option<crate::pipeline::PipelineOverride>, String> {
    load_pg_pipeline_override(
        pool,
        "SELECT pipeline_config::text AS pipeline_config FROM github_repos WHERE id = $1",
        repo,
        "repo",
    )
    .await
}

async fn load_pg_agent_override(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<crate::pipeline::PipelineOverride>, String> {
    load_pg_pipeline_override(
        pool,
        "SELECT pipeline_config::text AS pipeline_config FROM agents WHERE id = $1",
        agent_id,
        "agent",
    )
    .await
}

async fn load_pg_pipeline_override(
    pool: &PgPool,
    sql: &str,
    id: &str,
    label: &str,
) -> Result<Option<crate::pipeline::PipelineOverride>, String> {
    let row = sqlx::query(sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load {label} pipeline override for {id}: {error}"))?;

    let raw = row
        .map(|row| {
            row.try_get::<Option<String>, _>("pipeline_config")
                .map_err(|error| format!("read {label} pipeline override for {id}: {error}"))
        })
        .transpose()?
        .flatten();

    match raw {
        Some(raw) => crate::pipeline::parse_override(&raw)
            .map_err(|error| format!("parse {label} pipeline override for {id}: {error}")),
        None => Ok(None),
    }
}

async fn resolve_pg_pipeline(
    pool: &PgPool,
    repo: &str,
    repo_override: Option<&crate::pipeline::PipelineOverride>,
    assigned_agent_id: Option<&str>,
    agent_overrides: &mut HashMap<String, Option<crate::pipeline::PipelineOverride>>,
) -> Result<crate::pipeline::PipelineConfig, String> {
    let agent_override = match assigned_agent_id {
        Some(agent_id) => {
            if !agent_overrides.contains_key(agent_id) {
                let override_value = load_pg_agent_override(pool, agent_id).await?;
                agent_overrides.insert(agent_id.to_string(), override_value);
            }
            agent_overrides
                .get(agent_id)
                .and_then(|value| value.as_ref())
        }
        None => None,
    };

    let _ = repo;
    Ok(crate::pipeline::resolve(repo_override, agent_override))
}

async fn close_pg_card_for_issue(
    pool: &PgPool,
    card: &PgCardRecord,
    pipeline: &crate::pipeline::PipelineConfig,
) -> Result<(), String> {
    let target_status = pipeline
        .states
        .iter()
        .find(|state| state.terminal)
        .map(|state| state.id.clone())
        .unwrap_or_else(|| "done".to_string());

    let ctx = crate::engine::transition::TransitionContext {
        card: crate::engine::transition::CardState {
            id: card.id.clone(),
            status: card.status.clone(),
            review_status: card.review_status.clone(),
            latest_dispatch_id: card.latest_dispatch_id.clone(),
        },
        pipeline: pipeline.clone(),
        gates: crate::engine::transition::GateSnapshot::default(),
    };
    let decision = crate::engine::transition::decide_status_transition(
        &ctx,
        &target_status,
        "github-sync",
        crate::engine::transition::ForceIntent::SystemRecovery,
    );

    match &decision.outcome {
        crate::engine::transition::TransitionOutcome::Allowed => {}
        crate::engine::transition::TransitionOutcome::NoOp => return Ok(()),
        crate::engine::transition::TransitionOutcome::Blocked(reason) => {
            return Err(format!(
                "transition {} {} -> {} blocked: {reason}",
                card.id, card.status, target_status
            ));
        }
    }

    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin pg sync tx for {}: {error}", card.id))?;
    for intent in &decision.intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await?;
    }
    let _ = crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
        &mut tx, &card.id,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit pg sync tx for {}: {error}", card.id))?;
    Ok(())
}

pub(crate) async fn sync_auto_queue_terminal_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<(), String> {
    // Keep this sync limited to entry cleanup. Run completion and phase-gate
    // decisions for `done` entries are handled by the auto-queue policy hook
    // (#815 handoff). `skipped` pending entries are finalized inline by
    // `update_entry_status_on_pg_tx` — see explicit sweep at the end for why
    // we still fall back to `maybe_finalize_run_if_ready_pg` for those runs.
    let terminal_rows = sqlx::query(
        "SELECT id, run_id, status
         FROM auto_queue_entries
         WHERE kanban_card_id = $1 AND status IN ('dispatched', 'failed')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load dispatched auto-queue entries for {card_id}: {error}"))?;

    let mut done_run_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for row in terminal_rows {
        let entry_id: String = row
            .try_get("id")
            .map_err(|error| format!("read terminal auto-queue entry id: {error}"))?;
        let entry_status: String = row
            .try_get("status")
            .map_err(|error| format!("read terminal auto-queue entry status: {error}"))?;
        if let Ok(Some(run_id)) = row.try_get::<Option<String>, _>("run_id") {
            done_run_ids.insert(run_id);
        }
        if entry_status == crate::db::auto_queue::ENTRY_STATUS_FAILED {
            crate::db::auto_queue::update_entry_status_on_pg_tx(
                tx,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_DONE,
                "card_terminal",
                &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
            )
            .await?;
        } else {
            sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'done',
                     completed_at = NOW()
                 WHERE id = $1 AND status = 'dispatched'",
            )
            .bind(&entry_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("mark auto-queue entry {entry_id} done: {error}"))?;
            record_auto_queue_transition_on_pg(
                tx,
                &entry_id,
                crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                crate::db::auto_queue::ENTRY_STATUS_DONE,
                "card_terminal",
            )
            .await?;
        }
    }

    let pending_rows = sqlx::query(
        "SELECT id, run_id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status = 'pending'
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load pending auto-queue entries for {card_id}: {error}"))?;

    let mut pending_run_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for row in pending_rows {
        let entry_id: String = row
            .try_get("id")
            .map_err(|error| format!("read pending auto-queue entry id: {error}"))?;
        if let Ok(Some(run_id)) = row.try_get::<Option<String>, _>("run_id") {
            pending_run_ids.insert(run_id);
        }
        crate::db::auto_queue::update_entry_status_on_pg_tx(
            tx,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "card_terminal_pending_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await?;
    }

    // #1019: PG github-sync's close path never fires the auto-queue policy
    // hook, so we cannot rely on it to finalize a run after its last
    // `skipped` entry is recorded. `update_entry_status_on_pg_tx` already
    // attempts finalization inline, but an explicit sweep per-affected-run
    // here keeps the invariant defensively ordered even if a concurrent
    // update earlier in the tx races the inline check — making the CI
    // `_pg` lane deterministic.
    for run_id in pending_run_ids {
        let _ = crate::db::auto_queue::maybe_finalize_run_if_ready_pg(tx, &run_id).await?;
    }

    // PG GitHub sync does not receive a PolicyEngine, so it cannot fire the
    // `OnCardTerminal` phase-gate policy path. Review-disabled runs explicitly
    // opt out of that gate, so complete them here once the last dispatched
    // entry has been marked done.
    for run_id in done_run_ids {
        if auto_queue_run_review_disabled_on_pg(tx, &run_id).await? {
            let _ = crate::db::auto_queue::maybe_finalize_run_if_ready_pg(tx, &run_id).await?;
        }
    }

    Ok(())
}

async fn auto_queue_run_review_disabled_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let review_mode = sqlx::query_scalar::<_, Option<String>>(
        "SELECT review_mode FROM auto_queue_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue review mode for run {run_id}: {error}"))?
    .flatten();

    Ok(review_mode.as_deref().unwrap_or("enabled") == "disabled")
}

async fn record_auto_queue_transition_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    entry_id: &str,
    from_status: &str,
    to_status: &str,
    trigger_source: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO auto_queue_entry_transitions (
            entry_id, from_status, to_status, trigger_source
         )
         VALUES ($1, $2, $3, $4)",
    )
    .bind(entry_id)
    .bind(from_status)
    .bind(to_status)
    .bind(trigger_source)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("record auto-queue transition for {entry_id}: {error}"))?;
    Ok(())
}

pub(crate) async fn sync_review_state_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    state: &str,
) -> Result<(), String> {
    if state == "clear_verdict" {
        sqlx::query(
            "UPDATE card_review_state
             SET last_verdict = NULL,
                 updated_at = NOW()
             WHERE card_id = $1",
        )
        .bind(card_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("clear review verdict for {card_id}: {error}"))?;
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, state, review_round, last_verdict, last_decision,
            pending_dispatch_id, approach_change_round, session_reset_round,
            review_entered_at, updated_at
         )
         VALUES (
            $1,
            $2,
            COALESCE((SELECT COALESCE(review_round, 0) FROM kanban_cards WHERE id = $1), 0),
            NULL,
            NULL,
            NULL,
            NULL,
            NULL,
            CASE WHEN $2 = 'reviewing' THEN NOW() ELSE NULL END,
            NOW()
         )
         ON CONFLICT (card_id) DO UPDATE
         SET state = EXCLUDED.state,
             pending_dispatch_id = CASE
                 WHEN EXCLUDED.state = 'suggestion_pending' THEN card_review_state.pending_dispatch_id
                 ELSE NULL
             END,
             review_entered_at = CASE
                 WHEN EXCLUDED.state = 'reviewing' THEN COALESCE(card_review_state.review_entered_at, NOW())
                 ELSE card_review_state.review_entered_at
             END,
             updated_at = NOW()",
    )
    .bind(card_id)
    .bind(state)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("sync review state for {card_id}: {error}"))?;

    Ok(())
}

/// Sync all registered repos (orchestration function).
#[allow(dead_code)]
pub fn sync_all_repos(
    db: &crate::db::Db,
    engine: &crate::engine::PolicyEngine,
) -> Result<SyncResult, String> {
    let _ = db;
    let pool = engine
        .pg_pool()
        .ok_or_else(|| "postgres backend required for GitHub repo sync".to_string())?;
    let repos = crate::utils::async_bridge::block_on_pg_result(
        pool,
        |bridge_pool| async move { super::list_repos_pg(&bridge_pool).await },
        |error| error,
    )?;
    let mut total = SyncResult::default();

    for repo in &repos {
        if !repo.sync_enabled {
            continue;
        }

        match fetch_issues(&repo.id) {
            Ok(issues) => match sync_github_issues_for_repo(db, engine, &repo.id, &issues) {
                Ok(r) => {
                    total.closed_count += r.closed_count;
                    total.inconsistency_count += r.inconsistency_count;
                }
                Err(e) => {
                    tracing::error!("[github-sync] sync failed for {}: {e}", repo.id);
                }
            },
            Err(e) => {
                tracing::warn!("[github-sync] fetch failed for {}: {e}", repo.id);
            }
        }
    }

    Ok(total)
}

#[derive(Debug, Default)]
pub struct SyncResult {
    pub closed_count: usize,
    pub inconsistency_count: usize,
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::github::test_utils::RecordingAdapter;

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
            let database_name = format!("agentdesk_github_sync_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "github sync tests",
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
                "github sync tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "github sync tests",
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
    fn parse_gh_issue_json() {
        let json = r#"[
            {"number": 1, "state": "OPEN", "title": "Bug fix", "labels": [{"name": "bug"}], "body": "Fix it"},
            {"number": 2, "state": "CLOSED", "title": "Feature", "labels": [], "body": null}
        ]"#;

        let issues: Vec<GhIssue> = serde_json::from_str(json).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[0].state, "OPEN");
        assert_eq!(issues[0].labels[0].name, "bug");
        assert_eq!(issues[1].state, "CLOSED");
    }

    #[test]
    fn fetch_issues_merges_recently_closed_batch_without_duplicates() {
        let adapter = RecordingAdapter::with_sync_responses(vec![
            Ok(
                r#"[
                    {"number":105,"state":"OPEN","title":"Recent open","labels":[{"name":"bug"}],"body":"Body"},
                    {"number":5,"state":"CLOSED","title":"Already included","labels":[],"body":null}
                ]"#
                .to_string(),
            ),
            Ok(
                r#"[
                    {"number":5,"state":"CLOSED","title":"Already included","labels":[],"body":null},
                    {"number":210,"state":"CLOSED","title":"Old issue closed recently","labels":[],"body":null}
                ]"#
                .to_string(),
            ),
        ]);

        let issues = fetch_issues_with_cutoff(
            &adapter,
            "owner/repo",
            NaiveDate::from_ymd_opt(2026, 4, 12).unwrap(),
        )
        .unwrap();

        assert_eq!(issues.len(), 3);
        assert_eq!(
            issues.iter().map(|issue| issue.number).collect::<Vec<_>>(),
            vec![105, 5, 210]
        );
        assert_eq!(
            adapter.calls(),
            vec![
                vec![
                    "issue".to_string(),
                    "list".to_string(),
                    "--repo".to_string(),
                    "owner/repo".to_string(),
                    "--json".to_string(),
                    "number,state,title,labels,body".to_string(),
                    "--limit".to_string(),
                    "100".to_string(),
                    "--state".to_string(),
                    "all".to_string(),
                ],
                vec![
                    "issue".to_string(),
                    "list".to_string(),
                    "--repo".to_string(),
                    "owner/repo".to_string(),
                    "--json".to_string(),
                    "number,state,title,labels,body".to_string(),
                    "--limit".to_string(),
                    "50".to_string(),
                    "--state".to_string(),
                    "closed".to_string(),
                    "--search".to_string(),
                    "closed:>2026-03-13 sort:updated-desc".to_string(),
                ],
            ]
        );
    }

    #[tokio::test]
    async fn pg_terminal_sync_marks_entries_without_finalizing_before_policy_hooks() {
        let test_pg = TestPostgresDb::create().await;
        let pool = test_pg.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ('agent-1', 'Agent 1', '123')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, created_at, updated_at
             ) VALUES (
                'card-pg-terminal-sync',
                'PG terminal sync',
                'done',
                'agent-1',
                NOW(),
                NOW()
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at)
             VALUES ('run-pg-sync', 'repo-1', 'agent-1', 'active', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id,
                run_id,
                kanban_card_id,
                agent_id,
                priority_rank,
                status,
                dispatch_id,
                slot_index,
                dispatched_at,
                created_at
             ) VALUES (
                'entry-pg-sync',
                'run-pg-sync',
                'card-pg-terminal-sync',
                'agent-1',
                0,
                'dispatched',
                'dispatch-pg-sync',
                0,
                NOW(),
                NOW()
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_slots (
                agent_id,
                slot_index,
                assigned_run_id,
                assigned_thread_group,
                thread_id_map,
                created_at,
                updated_at
             ) VALUES (
                'agent-1',
                0,
                'run-pg-sync',
                0,
                '{}'::jsonb,
                NOW(),
                NOW()
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut tx = pool.begin().await.unwrap();
        sync_auto_queue_terminal_on_pg(&mut tx, "card-pg-terminal-sync")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-pg-sync'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(run_status, "active");

        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-pg-sync'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(entry_status, "done");

        let assigned_run_id: Option<String> = sqlx::query_scalar(
            "SELECT assigned_run_id
             FROM auto_queue_slots
             WHERE agent_id = 'agent-1' AND slot_index = 0",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(assigned_run_id.as_deref(), Some("run-pg-sync"));

        let notify_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM message_outbox
             WHERE target = 'channel:123' AND bot = 'notify'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notify_count, 0);

        crate::db::postgres::close_test_pool(pool, "github sync tests")
            .await
            .unwrap();
        test_pg.drop().await;
    }
}
