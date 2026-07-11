//! GitHub issue state sync: keep kanban cards consistent with GitHub issue state.

use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

const ISSUE_JSON_FIELDS: &str =
    "number,state,title,labels,body,url,closedAt,closedByPullRequestsReferences";
const PRIMARY_FETCH_LIMIT: u32 = 100;
const RECENTLY_CLOSED_FETCH_LIMIT: u32 = 50;
const RECENTLY_CLOSED_LOOKBACK_DAYS: i64 = 30;
const STALE_CARD_RECONCILE_ISSUE_SCAN_LIMIT: usize = 500;
const STALE_CARD_RECONCILE_ISSUE_LIMIT: usize = 100;
const STALE_CARD_RECONCILE_BATCH_SIZE: usize = 25;
const STALE_CARD_RECONCILE_TIMEOUT_SECS: u64 = 30;
const STALE_CARD_RECONCILE_CURSOR_KEY_PREFIX: &str = "github_sync.stale_card_reconcile_cursor:";

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
    let cutoff = today - ChronoDuration::days(RECENTLY_CLOSED_LOOKBACK_DAYS);
    format!("closed:>{} sort:updated-desc", cutoff.format("%Y-%m-%d"))
}

fn merge_unique_issues(target: &mut Vec<GhIssue>, extras: Vec<GhIssue>) {
    let mut seen: HashSet<i64> = target.iter().map(|issue| issue.number).collect();
    target.extend(extras.into_iter().filter(|issue| seen.insert(issue.number)));
}

/// Repos for which we've already emitted the "no repo_dirs mapping" WARN.
/// When a repo lacks an `agentdesk.yaml` `repo_dirs` entry, mainline sync runs
/// every ~10 minutes and would otherwise spam an identical WARN forever (#3566).
/// We log once per repo_id at WARN, then drop to DEBUG. A new/misconfigured
/// repo_id still WARNs on its first cycle. Reset on process restart (intended).
static REPO_MAPPING_WARN_SENT: OnceLock<dashmap::DashSet<String>> = OnceLock::new();

fn mainline_issue_numbers_for_repo(repo: &str) -> Vec<i64> {
    let repo_dir = match crate::services::platform::shell::resolve_repo_dir_for_target(Some(repo)) {
        Ok(Some(repo_dir)) => repo_dir,
        Ok(None) => {
            tracing::debug!("[github-sync] {repo}: repo dir unavailable for mainline sync");
            return Vec::new();
        }
        Err(error) => {
            // Only the genuine "no repo_dirs mapping" failure is benign/noisy and
            // safe to rate-suppress. Persistent misconfiguration errors — invalid
            // mapped dir, non-git worktree, wrong remote — must keep WARNing with an
            // accurate label so they aren't hidden behind a "no mapping" message (#3566).
            if crate::services::platform::shell::is_no_repo_mapping_error(&error) {
                let warned = REPO_MAPPING_WARN_SENT.get_or_init(dashmap::DashSet::new);
                if warned.insert(repo.to_string()) {
                    tracing::warn!(
                        "[github-sync] {repo}: repo dir resolution failed for mainline sync (no repo_dirs mapping); suppressing further repeats — {error}"
                    );
                } else {
                    tracing::debug!(
                        "[github-sync] {repo}: repo dir resolution failed (no mapping, suppressed): {error}"
                    );
                }
            } else {
                tracing::warn!(
                    "[github-sync] {repo}: repo dir resolution failed for mainline sync (misconfiguration): {error}"
                );
            }
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
    sync_github_issues_for_repo_pg_with_adapter(pool, repo, issues, super::adapter()).await
}

pub(crate) async fn sync_github_issues_for_repo_pg_with_adapter(
    pool: &PgPool,
    repo: &str,
    issues: &[GhIssue],
    adapter: &dyn super::GitHubAdapter,
) -> Result<SyncResult, String> {
    crate::pipeline::ensure_loaded();

    let repo_override = load_pg_repo_override(pool, repo).await?;
    let mut agent_overrides: HashMap<String, Option<crate::pipeline::PipelineOverride>> =
        HashMap::new();
    let mut result = SyncResult::default();
    let fetched_issue_numbers: HashSet<i64> = issues.iter().map(|issue| issue.number).collect();
    let mut issues = issues.to_vec();

    match stale_card_issue_numbers_for_reconcile(
        pool,
        repo,
        &fetched_issue_numbers,
        repo_override.as_ref(),
        &mut agent_overrides,
    )
    .await
    {
        Ok(selection) => {
            if let Some(next_cursor) = selection.next_cursor {
                if let Err(error) = save_stale_reconcile_cursor(pool, repo, next_cursor).await {
                    tracing::warn!(
                        "[github-sync] {repo}: failed to persist stale reconcile cursor {next_cursor}: {error}"
                    );
                }
            }
            if selection.issue_numbers.is_empty() {
                return sync_loaded_github_issues_for_repo_pg(
                    pool,
                    repo,
                    &issues,
                    repo_override.as_ref(),
                    &mut agent_overrides,
                    result,
                )
                .await;
            }
            let report =
                fetch_issues_by_number_batches(adapter, repo, &selection.issue_numbers).await;
            apply_stale_reconcile_fetch_report(
                repo,
                &mut result,
                &mut issues,
                selection.issue_numbers.len(),
                report,
            );
        }
        Err(error) => {
            result.stale_card_issue_error_count += 1;
            tracing::warn!(
                "[github-sync] {repo}: stale card reconcile candidate selection failed: {error}"
            );
        }
    }

    sync_loaded_github_issues_for_repo_pg(
        pool,
        repo,
        &issues,
        repo_override.as_ref(),
        &mut agent_overrides,
        result,
    )
    .await
}

async fn sync_loaded_github_issues_for_repo_pg(
    pool: &PgPool,
    repo: &str,
    issues: &[GhIssue],
    repo_override: Option<&crate::pipeline::PipelineOverride>,
    agent_overrides: &mut HashMap<String, Option<crate::pipeline::PipelineOverride>>,
    mut result: SyncResult,
) -> Result<SyncResult, String> {
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
                repo_override,
                card.assigned_agent_id.as_deref(),
                agent_overrides,
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
                // #1946 (codex C — observability promotion): the OPEN/terminal
                // mismatch was previously only counted in the result and
                // emitted as a tracing warning, so production retros for the
                // direct-first publication gap had to be reconstructed from
                // commit logs after the fact. Promote it to a deduped alert
                // on the operator channel so each new mismatch surfaces
                // within one GitHub-sync cycle (~20 min) of going terminal.
                if let Err(error) =
                    enqueue_terminal_open_alert_pg(pool, repo, issue.number, &card, &card.status)
                        .await
                {
                    tracing::warn!(
                        "[github-sync] {repo}#{}: terminal-open alert enqueue failed: {error}",
                        issue.number
                    );
                }
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
                    repo_override,
                    card.assigned_agent_id.as_deref(),
                    agent_overrides,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleIssueSelection {
    issue_numbers: Vec<i64>,
    next_cursor: Option<i64>,
}

#[derive(Debug, Default)]
struct StaleIssueFetchReport {
    issues: Vec<GhIssue>,
    batch_count: usize,
    error_count: usize,
    errors: Vec<String>,
}

#[derive(Debug, Clone)]
struct StaleCardCandidate {
    issue_number: i64,
    card: PgCardRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StaleIssueCandidate {
    issue_number: i64,
    is_terminal: bool,
}

fn select_stale_issue_numbers_from_candidates(
    candidates: &[StaleIssueCandidate],
    fetched_issue_numbers: &HashSet<i64>,
    cursor: Option<i64>,
    limit: usize,
) -> StaleIssueSelection {
    if limit == 0 {
        return StaleIssueSelection {
            issue_numbers: Vec::new(),
            next_cursor: None,
        };
    }

    let mut candidates_by_issue = std::collections::BTreeMap::<i64, bool>::new();
    for candidate in candidates {
        if candidate.issue_number <= 0 || fetched_issue_numbers.contains(&candidate.issue_number) {
            continue;
        }
        candidates_by_issue
            .entry(candidate.issue_number)
            .and_modify(|all_terminal| *all_terminal &= candidate.is_terminal)
            .or_insert(candidate.is_terminal);
    }

    let mut ordered = candidates_by_issue
        .into_iter()
        .map(|(issue_number, is_terminal)| StaleIssueCandidate {
            issue_number,
            is_terminal,
        })
        .collect::<Vec<_>>();
    if let Some(cursor) = cursor {
        ordered.sort_by_key(|candidate| (candidate.issue_number <= cursor, candidate.issue_number));
    }

    let mut issue_numbers = Vec::new();
    let mut next_cursor = None;
    for candidate in ordered {
        next_cursor = Some(candidate.issue_number);
        if candidate.is_terminal {
            continue;
        }
        issue_numbers.push(candidate.issue_number);
        if issue_numbers.len() >= limit {
            break;
        }
    }

    StaleIssueSelection {
        issue_numbers,
        next_cursor,
    }
}

async fn stale_card_issue_numbers_for_reconcile(
    pool: &PgPool,
    repo: &str,
    fetched_issue_numbers: &HashSet<i64>,
    repo_override: Option<&crate::pipeline::PipelineOverride>,
    agent_overrides: &mut HashMap<String, Option<crate::pipeline::PipelineOverride>>,
) -> Result<StaleIssueSelection, String> {
    crate::pipeline::ensure_loaded();

    let fetched_issue_numbers: Vec<i64> = fetched_issue_numbers.iter().copied().collect();
    let cursor = load_stale_reconcile_cursor(pool, repo).await?;
    let mut stale_issue_numbers = load_stale_reconcile_issue_page(
        pool,
        repo,
        &fetched_issue_numbers,
        cursor,
        false,
        STALE_CARD_RECONCILE_ISSUE_SCAN_LIMIT,
    )
    .await?;

    if cursor.is_some() && stale_issue_numbers.len() < STALE_CARD_RECONCILE_ISSUE_SCAN_LIMIT {
        let mut wrapped = load_stale_reconcile_issue_page(
            pool,
            repo,
            &fetched_issue_numbers,
            cursor,
            true,
            STALE_CARD_RECONCILE_ISSUE_SCAN_LIMIT - stale_issue_numbers.len(),
        )
        .await?;
        stale_issue_numbers.append(&mut wrapped);
    }

    let card_candidates =
        load_stale_reconcile_cards_for_issues(pool, repo, &stale_issue_numbers).await?;
    let mut candidates = Vec::with_capacity(card_candidates.len());
    for candidate in card_candidates {
        let pipeline = resolve_pg_pipeline(
            pool,
            repo,
            repo_override,
            candidate.card.assigned_agent_id.as_deref(),
            agent_overrides,
        )
        .await?;
        candidates.push(StaleIssueCandidate {
            issue_number: candidate.issue_number,
            is_terminal: pipeline.is_terminal(&candidate.card.status),
        });
    }

    let selection = select_stale_issue_numbers_from_candidates(
        &candidates,
        &HashSet::new(),
        cursor,
        STALE_CARD_RECONCILE_ISSUE_LIMIT,
    );

    if !selection.issue_numbers.is_empty() {
        tracing::info!(
            "[github-sync] {repo}: card-driven stale reconcile selected {} missing non-terminal issue(s) after cursor {:?} (issue_limit={}, next_cursor={:?})",
            selection.issue_numbers.len(),
            cursor,
            STALE_CARD_RECONCILE_ISSUE_LIMIT,
            selection.next_cursor
        );
    }

    Ok(selection)
}

async fn load_stale_reconcile_issue_page(
    pool: &PgPool,
    repo: &str,
    fetched_issue_numbers: &[i64],
    cursor: Option<i64>,
    wrap: bool,
    limit: usize,
) -> Result<Vec<i64>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let rows = if wrap {
        sqlx::query(
            "SELECT DISTINCT github_issue_number::BIGINT AS issue_number
             FROM kanban_cards
             WHERE repo_id = $1
               AND github_issue_number IS NOT NULL
               AND github_issue_number > 0
               AND NOT (github_issue_number = ANY($2::BIGINT[]))
               AND github_issue_number <= $3
             ORDER BY issue_number ASC
             LIMIT $4",
        )
        .bind(repo)
        .bind(fetched_issue_numbers)
        .bind(cursor.unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT DISTINCT github_issue_number::BIGINT AS issue_number
             FROM kanban_cards
             WHERE repo_id = $1
               AND github_issue_number IS NOT NULL
               AND github_issue_number > 0
               AND NOT (github_issue_number = ANY($2::BIGINT[]))
               AND ($3::BIGINT IS NULL OR github_issue_number > $3)
             ORDER BY issue_number ASC
             LIMIT $4",
        )
        .bind(repo)
        .bind(fetched_issue_numbers)
        .bind(cursor)
        .bind(limit)
        .fetch_all(pool)
        .await
    }
    .map_err(|error| format!("load stale card reconcile candidates for {repo}: {error}"))?;

    rows.into_iter()
        .map(|row| {
            row.try_get("issue_number")
                .map_err(|error| format!("read stale reconcile issue number: {error}"))
        })
        .collect()
}

async fn load_stale_reconcile_cards_for_issues(
    pool: &PgPool,
    repo: &str,
    issue_numbers: &[i64],
) -> Result<Vec<StaleCardCandidate>, String> {
    if issue_numbers.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT id,
                status,
                review_status,
                latest_dispatch_id,
                assigned_agent_id,
                github_issue_number::BIGINT AS issue_number
         FROM kanban_cards
         WHERE repo_id = $1
           AND github_issue_number = ANY($2::BIGINT[])
         ORDER BY issue_number ASC, id ASC",
    )
    .bind(repo)
    .bind(issue_numbers)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load stale card reconcile cards for {repo}: {error}"))?;

    rows.into_iter()
        .map(stale_card_candidate_from_row)
        .collect()
}

fn stale_card_candidate_from_row(row: sqlx::postgres::PgRow) -> Result<StaleCardCandidate, String> {
    Ok(StaleCardCandidate {
        issue_number: row
            .try_get("issue_number")
            .map_err(|error| format!("read stale reconcile issue number: {error}"))?,
        card: PgCardRecord {
            id: row
                .try_get("id")
                .map_err(|error| format!("read stale reconcile card id: {error}"))?,
            status: row
                .try_get("status")
                .map_err(|error| format!("read stale reconcile card status: {error}"))?,
            review_status: row
                .try_get("review_status")
                .map_err(|error| format!("read stale reconcile review_status: {error}"))?,
            latest_dispatch_id: row
                .try_get("latest_dispatch_id")
                .map_err(|error| format!("read stale reconcile latest_dispatch_id: {error}"))?,
            assigned_agent_id: row
                .try_get("assigned_agent_id")
                .map_err(|error| format!("read stale reconcile assigned_agent_id: {error}"))?,
        },
    })
}

fn stale_reconcile_cursor_key(repo: &str) -> String {
    format!("{STALE_CARD_RECONCILE_CURSOR_KEY_PREFIX}{repo}")
}

async fn load_stale_reconcile_cursor(pool: &PgPool, repo: &str) -> Result<Option<i64>, String> {
    let key = stale_reconcile_cursor_key(repo);
    let raw: Option<String> =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
            .bind(&key)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load stale reconcile cursor for {repo}: {error}"))?;

    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.trim().parse::<i64>() {
        Ok(value) if value > 0 => Ok(Some(value)),
        _ => {
            tracing::warn!(
                "[github-sync] {repo}: ignoring invalid stale reconcile cursor value {:?}",
                raw
            );
            Ok(None)
        }
    }
}

async fn save_stale_reconcile_cursor(
    pool: &PgPool,
    repo: &str,
    issue_number: i64,
) -> Result<(), String> {
    let key = stale_reconcile_cursor_key(repo);
    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(&key)
    .bind(issue_number.to_string())
    .execute(pool)
    .await
    .map_err(|error| format!("save stale reconcile cursor for {repo}: {error}"))?;
    Ok(())
}

async fn fetch_issues_by_number_batches(
    adapter: &dyn super::GitHubAdapter,
    repo: &str,
    issue_numbers: &[i64],
) -> StaleIssueFetchReport {
    let (owner, name) = match parse_repo_owner_name(repo) {
        Ok(parts) => parts,
        Err(error) => {
            return StaleIssueFetchReport {
                error_count: 1,
                errors: vec![error],
                ..StaleIssueFetchReport::default()
            };
        }
    };
    let mut report = StaleIssueFetchReport::default();

    for batch in issue_numbers.chunks(STALE_CARD_RECONCILE_BATCH_SIZE) {
        report.batch_count += 1;
        let query = issue_number_batch_graphql_query(batch);
        let args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("owner={owner}"),
            "-f".to_string(),
            format!("name={name}"),
            "-f".to_string(),
            format!("query={query}"),
        ];
        let output = match adapter
            .run_async(
                args,
                Duration::from_secs(STALE_CARD_RECONCILE_TIMEOUT_SECS),
                format!(
                    "github stale card reconcile timed out for {repo} ({} issue(s))",
                    batch.len()
                ),
            )
            .await
        {
            Ok(output) => output,
            Err(error) => {
                report.error_count += 1;
                report.errors.push(error);
                continue;
            }
        };
        match parse_issue_number_batch_graphql_response(&output, batch) {
            Ok(mut batch_issues) => report.issues.append(&mut batch_issues),
            Err(error) => {
                report.error_count += 1;
                report.errors.push(error);
            }
        }
    }

    report
}

fn apply_stale_reconcile_fetch_report(
    repo: &str,
    result: &mut SyncResult,
    issues: &mut Vec<GhIssue>,
    attempted_issue_count: usize,
    report: StaleIssueFetchReport,
) {
    result.stale_card_issue_check_count += attempted_issue_count;
    result.stale_card_issue_batch_count += report.batch_count;
    result.stale_card_issue_error_count += report.error_count;

    if report.error_count > 0 {
        tracing::warn!(
            "[github-sync] {repo}: stale card reconcile had {} non-fatal GraphQL error(s): {}",
            report.error_count,
            report.errors.join("; ")
        );
    }

    let stale_closed_issue_count = report
        .issues
        .iter()
        .filter(|issue| issue.state == "CLOSED")
        .count();
    tracing::info!(
        "[github-sync] {repo}: card-driven stale reconcile checked {} issue(s) in {} GraphQL batch(es); fetched={}, closed={}, errors={}",
        attempted_issue_count,
        report.batch_count,
        report.issues.len(),
        stale_closed_issue_count,
        report.error_count
    );
    merge_unique_issues(issues, report.issues);
}

fn parse_repo_owner_name(repo: &str) -> Result<(&str, &str), String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo id '{repo}' is not in owner/name form"))?;
    if owner.trim().is_empty() || name.trim().is_empty() || name.contains('/') {
        return Err(format!("repo id '{repo}' is not in owner/name form"));
    }
    Ok((owner, name))
}

fn issue_number_batch_graphql_query(issue_numbers: &[i64]) -> String {
    let fields = issue_numbers
        .iter()
        .enumerate()
        .map(|(index, issue_number)| {
            format!(
                "i{index}: issue(number: {issue_number}) {{ number state title body url closedAt closedByPullRequestsReferences(first: 5) {{ nodes {{ number url }} }} }}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields} }} }}"
    )
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlIssueBatchResponse {
    data: Option<GraphQlIssueBatchData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlIssueBatchData {
    repository: Option<HashMap<String, Option<GraphQlIssue>>>,
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlIssue {
    number: i64,
    state: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(default, rename = "closedByPullRequestsReferences")]
    closed_by_pull_requests_references: Option<GraphQlPullRequestConnection>,
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlPullRequestConnection {
    #[serde(default)]
    nodes: Vec<GraphQlPullRequestReference>,
}

#[derive(Debug, serde::Deserialize)]
struct GraphQlPullRequestReference {
    number: Option<i64>,
    url: Option<String>,
}

fn parse_issue_number_batch_graphql_response(
    output: &str,
    issue_numbers: &[i64],
) -> Result<Vec<GhIssue>, String> {
    let response: GraphQlIssueBatchResponse = serde_json::from_str(output)
        .map_err(|error| format!("parse stale issue GraphQL response: {error}"))?;
    if !response.errors.is_empty() {
        let messages = response
            .errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("stale issue GraphQL query failed: {messages}"));
    }

    let repository = response
        .data
        .and_then(|data| data.repository)
        .ok_or_else(|| "stale issue GraphQL response missing repository".to_string())?;

    let mut issues = Vec::new();
    for (index, issue_number) in issue_numbers.iter().enumerate() {
        match repository.get(&format!("i{index}")) {
            Some(Some(issue)) => issues.push(GhIssue {
                number: issue.number,
                state: issue.state.clone(),
                title: issue.title.clone(),
                labels: Vec::new(),
                body: issue.body.clone(),
                url: issue.url.clone(),
                closed_at: issue.closed_at.clone(),
                closed_by_pull_requests_references: issue
                    .closed_by_pull_requests_references
                    .as_ref()
                    .map(|connection| {
                        connection
                            .nodes
                            .iter()
                            .map(|node| GhPullRequestReference {
                                number: node.number,
                                url: node.url.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            Some(None) | None => {
                tracing::debug!(
                    "[github-sync] stale reconcile: issue #{} missing from GraphQL response",
                    issue_number
                );
            }
        }
    }

    Ok(issues)
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
pub fn sync_all_repos(engine: &crate::engine::PolicyEngine) -> Result<SyncResult, String> {
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
            Ok(issues) => match sync_github_issues_for_repo(engine, &repo.id, &issues) {
                Ok(r) => {
                    total.closed_count += r.closed_count;
                    total.inconsistency_count += r.inconsistency_count;
                    total.stale_card_issue_check_count += r.stale_card_issue_check_count;
                    total.stale_card_issue_batch_count += r.stale_card_issue_batch_count;
                    total.stale_card_issue_error_count += r.stale_card_issue_error_count;
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
    pub stale_card_issue_check_count: usize,
    pub stale_card_issue_batch_count: usize,
    pub stale_card_issue_error_count: usize,
}

/// Reason code attached to terminal-card / OPEN-issue mismatch alerts so the
/// outbox can dedupe per `(repo, issue, card)` window.
const TERMINAL_OPEN_REASON_CODE: &str = "github_sync.terminal_open_issue";

/// Dedupe TTL for terminal-open mismatch alerts. The GitHub sync interval is
/// runtime-configurable (default ~5 min, often raised to ~20 min in
/// production); a 24h dedupe window keeps the alert from spamming the
/// channel every cycle while still giving a daily reminder until the
/// mismatch is resolved.
const TERMINAL_OPEN_ALERT_DEDUPE_TTL_SECS: i64 = 24 * 60 * 60;

/// Render the alert content for a terminal-card / OPEN-issue mismatch.
///
/// Public for unit-testing the formatter without spinning up a Postgres pool.
pub(crate) fn format_terminal_open_alert(
    repo: &str,
    issue_number: i64,
    card_id: &str,
    card_status: &str,
) -> String {
    format!(
        "[github-sync] terminal/OPEN mismatch: {repo}#{issue_number} card={card_id} status={card_status} \
         — kanban marks this card terminal but the GitHub issue is still OPEN. \
         Likely caused by direct-first / cherry-merge publication without a \
         closing PR (see retro #1946). Verify the commit landed on main and \
         close the issue manually if appropriate."
    )
}

async fn resolve_terminal_open_alert_channel_pg(pool: &PgPool) -> Result<Option<String>, String> {
    crate::services::agent_quality::regression_alerts::resolve_alert_channel_with_env_pg(
        pool,
        "ADK_GITHUB_SYNC_ALERT_CHANNEL",
    )
    .await
    .map_err(|error| format!("resolve github-sync alert target: {error}"))
}

async fn enqueue_terminal_open_alert_pg(
    pool: &PgPool,
    repo: &str,
    issue_number: i64,
    card: &PgCardRecord,
    card_status: &str,
) -> Result<bool, String> {
    let Some(target) = resolve_terminal_open_alert_channel_pg(pool).await? else {
        return Ok(false);
    };
    let session_key = format!(
        "github_sync.terminal_open:{repo}:{issue_number}:{}",
        card.id
    );
    let content = format_terminal_open_alert(repo, issue_number, &card.id, card_status);

    crate::services::message_outbox::enqueue_outbox_pg_with_ttl(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: content.as_str(),
            bot: "notify",
            source: "github_sync",
            reason_code: Some(TERMINAL_OPEN_REASON_CODE),
            session_key: Some(session_key.as_str()),
        },
        TERMINAL_OPEN_ALERT_DEDUPE_TTL_SECS,
    )
    .await
    .map_err(|error| {
        format!(
            "enqueue github-sync terminal-open alert for {repo}#{issue_number}/{}: {error}",
            card.id
        )
    })
}

#[cfg(test)]
mod terminal_open_alert_tests {
    use super::*;

    fn stale_candidate(issue_number: i64, is_terminal: bool) -> StaleIssueCandidate {
        StaleIssueCandidate {
            issue_number,
            is_terminal,
        }
    }

    #[derive(Debug, Default)]
    struct RecordingAdapter {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        async_responses: std::sync::Mutex<std::collections::VecDeque<Result<String, String>>>,
    }

    impl RecordingAdapter {
        fn with_async_responses(async_responses: Vec<Result<String, String>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                async_responses: std::sync::Mutex::new(async_responses.into()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl crate::github::GitHubAdapter for RecordingAdapter {
        fn is_available(&self) -> bool {
            true
        }

        fn run(&self, args: &[&str]) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            Ok(String::new())
        }

        fn run_async<'a>(
            &'a self,
            args: Vec<String>,
            _timeout: std::time::Duration,
            _timeout_context: String,
        ) -> crate::github::GitHubFuture<'a, Result<String, String>> {
            self.calls.lock().unwrap().push(args);
            let response = self
                .async_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()));
            Box::pin(async move { response })
        }
    }

    /// #1946: the alert message must surface enough context for an operator
    /// to find the offending card and issue at a glance, plus point them at
    /// the retro for the longer-form root-cause writeup.
    #[test]
    fn format_terminal_open_alert_contains_actionable_context() {
        let msg = format_terminal_open_alert("itismyfield/AgentDesk", 1812, "card-1812", "done");
        assert!(msg.contains("itismyfield/AgentDesk"), "msg: {msg}");
        assert!(msg.contains("#1812"), "msg: {msg}");
        assert!(msg.contains("card-1812"), "msg: {msg}");
        assert!(msg.contains("done"), "msg: {msg}");
        assert!(msg.contains("#1946"), "msg should reference retro: {msg}");
    }

    #[test]
    fn stale_candidate_selection_rotates_past_old_open_frontier() {
        let candidates = (1..=150)
            .map(|issue_number| stale_candidate(issue_number, false))
            .collect::<Vec<_>>();
        let fetched_issue_numbers = HashSet::new();

        let first = select_stale_issue_numbers_from_candidates(
            &candidates,
            &fetched_issue_numbers,
            None,
            100,
        );
        assert_eq!(first.issue_numbers.first().copied(), Some(1));
        assert_eq!(first.issue_numbers.last().copied(), Some(100));
        assert_eq!(first.next_cursor, Some(100));

        let second = select_stale_issue_numbers_from_candidates(
            &candidates,
            &fetched_issue_numbers,
            first.next_cursor,
            100,
        );
        assert_eq!(second.issue_numbers.first().copied(), Some(101));
        assert!(
            second.issue_numbers.contains(&125),
            "closed ghost behind the first 100 candidates should be reached"
        );
        assert_eq!(second.issue_numbers.last().copied(), Some(50));
        assert_eq!(second.next_cursor, Some(50));
    }

    #[test]
    fn stale_candidate_selection_skips_normal_fetch_window() {
        let candidates = [10, 11, 12, 13, 14]
            .into_iter()
            .map(|issue_number| stale_candidate(issue_number, false))
            .collect::<Vec<_>>();
        let fetched_issue_numbers = [12, 13].into_iter().collect::<HashSet<_>>();

        let selection = select_stale_issue_numbers_from_candidates(
            &candidates,
            &fetched_issue_numbers,
            Some(11),
            10,
        );

        assert_eq!(selection.issue_numbers, vec![14, 10, 11]);
        assert_eq!(selection.next_cursor, Some(11));
    }

    #[test]
    fn stale_candidate_selection_excludes_pipeline_terminal_statuses() {
        let candidates = vec![
            stale_candidate(41, false),
            stale_candidate(42, true),
            stale_candidate(43, false),
        ];
        let fetched_issue_numbers = HashSet::new();

        let selection = select_stale_issue_numbers_from_candidates(
            &candidates,
            &fetched_issue_numbers,
            None,
            10,
        );

        assert_eq!(selection.issue_numbers, vec![41, 43]);
        assert_eq!(
            selection.next_cursor,
            Some(43),
            "cursor should still advance across terminal candidates"
        );
    }

    #[test]
    fn stale_fetch_failure_records_metric_without_dropping_normal_issues() {
        let mut result = SyncResult::default();
        let mut issues = vec![GhIssue {
            number: 1,
            state: "OPEN".to_string(),
            title: "Normal fetched issue".to_string(),
            labels: Vec::new(),
            body: None,
            url: None,
            closed_at: None,
            closed_by_pull_requests_references: Vec::new(),
        }];
        let report = StaleIssueFetchReport {
            batch_count: 1,
            error_count: 1,
            errors: vec!["GraphQL unavailable".to_string()],
            ..StaleIssueFetchReport::default()
        };

        apply_stale_reconcile_fetch_report("owner/repo", &mut result, &mut issues, 3, report);

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 1);
        assert_eq!(result.stale_card_issue_check_count, 3);
        assert_eq!(result.stale_card_issue_batch_count, 1);
        assert_eq!(result.stale_card_issue_error_count, 1);
    }

    #[test]
    fn parses_batched_graphql_issue_response() {
        let issues = parse_issue_number_batch_graphql_response(
            r#"{
                "data": {
                    "repository": {
                        "i0": {
                            "number": 2945,
                            "state": "CLOSED",
                            "title": "Old closed issue",
                            "body": null,
                            "url": "https://github.com/owner/repo/issues/2945",
                            "closedAt": "2026-03-23T00:00:00Z",
                            "closedByPullRequestsReferences": {
                                "nodes": [
                                    {
                                        "number": 3001,
                                        "url": "https://github.com/owner/repo/pull/3001"
                                    }
                                ]
                            }
                        }
                    }
                }
            }"#,
            &[2945],
        )
        .unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 2945);
        assert_eq!(issues[0].state, "CLOSED");
        assert_eq!(issues[0].closed_at.as_deref(), Some("2026-03-23T00:00:00Z"));
        assert_eq!(
            issues[0].closed_by_pull_requests_references[0].number,
            Some(3001)
        );
    }

    #[tokio::test]
    async fn pg_card_driven_reconcile_closes_old_closed_issue_missing_from_fetch_window() {
        let test_pg = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = test_pg.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO kanban_cards (
                id,
                title,
                status,
                repo_id,
                github_issue_number,
                github_issue_url,
                created_at,
                updated_at
             ) VALUES (
                'card-old-closed-2945',
                'Old closed ghost card',
                'backlog',
                'owner/repo',
                2945,
                'https://github.com/owner/repo/issues/2945',
                NOW() - INTERVAL '40 days',
                NOW() - INTERVAL '40 days'
             )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let adapter = RecordingAdapter::with_async_responses(vec![Ok(r#"{
                "data": {
                    "repository": {
                        "i0": {
                            "number": 2945,
                            "state": "CLOSED",
                            "title": "Old closed ghost card",
                            "body": null,
                            "url": "https://github.com/owner/repo/issues/2945",
                            "closedAt": "2026-03-23T00:00:00Z",
                            "closedByPullRequestsReferences": {
                                "nodes": []
                            }
                        }
                    }
                }
            }"#
        .to_string())]);

        let result =
            sync_github_issues_for_repo_pg_with_adapter(&pool, "owner/repo", &[], &adapter)
                .await
                .unwrap();

        assert_eq!(result.closed_count, 1);
        assert_eq!(result.stale_card_issue_check_count, 1);
        assert_eq!(result.stale_card_issue_batch_count, 1);

        let status: String = sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = $1")
            .bind("card-old-closed-2945")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "done");

        let calls = adapter.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "api");
        assert_eq!(calls[0][1], "graphql");
        assert!(
            calls[0]
                .iter()
                .any(|arg| arg.contains("issue(number: 2945)")),
            "expected GraphQL batch query to include issue #2945, got {calls:?}"
        );

        pool.close().await;
        test_pg.drop().await;
    }

    #[tokio::test]
    async fn pg_stale_candidate_selection_excludes_custom_terminal_status() {
        let test_pg = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = test_pg.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO github_repos (id, display_name, sync_enabled, pipeline_config)
             VALUES (
                'owner/repo',
                'owner/repo',
                true,
                $1::jsonb
             )",
        )
        .bind(
            serde_json::json!({
                "states": [
                    {"id": "backlog", "label": "Backlog"},
                    {"id": "archived", "label": "Archived", "terminal": true}
                ],
                "transitions": []
            })
            .to_string(),
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, repo_id, github_issue_number, created_at, updated_at
             ) VALUES
                ('card-custom-terminal-2946', 'Archived terminal', 'archived', 'owner/repo', 2946, NOW(), NOW()),
                ('card-open-backlog-2947', 'Backlog candidate', 'backlog', 'owner/repo', 2947, NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        let repo_override = load_pg_repo_override(&pool, "owner/repo").await.unwrap();
        let mut agent_overrides = HashMap::new();
        let selection = stale_card_issue_numbers_for_reconcile(
            &pool,
            "owner/repo",
            &HashSet::new(),
            repo_override.as_ref(),
            &mut agent_overrides,
        )
        .await
        .unwrap();

        assert_eq!(selection.issue_numbers, vec![2947]);
        assert_eq!(selection.next_cursor, Some(2947));

        pool.close().await;
        test_pg.drop().await;
    }

    /// PG dedupe: re-running the alert enqueue for the same
    /// `(repo, issue, card)` triple within the dedupe TTL must not produce a
    /// second outbox row. Verifies the 24h dedupe window is wired through
    /// `enqueue_outbox_pg_with_ttl`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enqueue_terminal_open_alert_pg_dedupes_within_ttl() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let seed_target = sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ('kanban_human_alert_channel_id', '123456')",
        )
        .execute(&pool)
        .await;
        assert!(seed_target.is_ok(), "seed terminal-open alert target");
        let card = PgCardRecord {
            id: "card-1946-pg".into(),
            status: "done".into(),
            review_status: None,
            latest_dispatch_id: None,
            assigned_agent_id: None,
        };

        let first = enqueue_terminal_open_alert_pg(&pool, "owner/repo", 1812, &card, "done")
            .await
            .expect("first enqueue ok");
        assert!(first, "first alert should be enqueued");

        let second = enqueue_terminal_open_alert_pg(&pool, "owner/repo", 1812, &card, "done")
            .await
            .expect("second enqueue ok");
        assert!(!second, "duplicate alert within TTL should be suppressed");

        let count: i64 = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*)::BIGINT FROM message_outbox WHERE reason_code = $1",
        )
        .bind(TERMINAL_OPEN_REASON_CODE)
        .fetch_one(&pool)
        .await
        .expect("count outbox rows");
        assert_eq!(count, 1, "exactly one outbox row should be persisted");

        // Different (repo, issue, card) triple must NOT be dedupe-suppressed.
        let other_card = PgCardRecord {
            id: "card-1813-pg".into(),
            status: "done".into(),
            review_status: None,
            latest_dispatch_id: None,
            assigned_agent_id: None,
        };
        let third = enqueue_terminal_open_alert_pg(&pool, "owner/repo", 1813, &other_card, "done")
            .await
            .expect("third enqueue ok");
        assert!(third, "different card should produce a new alert");

        pool.close().await;
        pg_db.drop().await;
    }
}
