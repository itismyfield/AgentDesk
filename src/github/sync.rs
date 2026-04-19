//! GitHub issue state sync: keep kanban cards consistent with GitHub issue state.

use crate::db::Db;
use chrono::{Duration, NaiveDate, Utc};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};

const ISSUE_JSON_FIELDS: &str = "number,state,title,labels,body";
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
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GhLabel {
    pub name: String,
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
    format!("closed:>{}", cutoff.format("%Y-%m-%d"))
}

fn merge_unique_issues(target: &mut Vec<GhIssue>, extras: Vec<GhIssue>) {
    let mut seen: HashSet<i64> = target.iter().map(|issue| issue.number).collect();
    target.extend(extras.into_iter().filter(|issue| seen.insert(issue.number)));
}

/// Sync GitHub issue state with kanban cards for a single repo.
///
/// - If a linked issue is CLOSED on GitHub -> update card to "done"
/// - If a linked issue is OPEN but card is "done" -> log inconsistency
///
/// Returns (closed_count, inconsistency_count).
pub fn sync_github_issues_for_repo(
    db: &Db,
    engine: &crate::engine::PolicyEngine,
    repo: &str,
    issues: &[GhIssue],
) -> Result<SyncResult, String> {
    let mut result = SyncResult::default();

    // Collect cards to close (need to drop conn before calling transition_status)
    let mut cards_to_close: Vec<(String, i64)> = Vec::new();

    {
        let conn = db.lock().map_err(|e| format!("db lock: {e}"))?;
        for issue in issues {
            let mut stmt = conn
                .prepare(
                    "SELECT id, status FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
                )
                .map_err(|e| format!("prepare: {e}"))?;

            let cards: Vec<(String, String)> = stmt
                .query_map(libsql_rusqlite::params![issue.number, repo], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| format!("query: {e}"))?
                .filter_map(|r| r.ok())
                .collect();

            for (card_id, card_status) in &cards {
                // Sync issue body → card description (only if changed)
                if let Some(ref body) = issue.body {
                    let trimmed = body.trim_end();
                    let _ = conn.execute(
                        "UPDATE kanban_cards SET description = ?1 WHERE id = ?2 AND (description IS NULL OR description != ?1)",
                        libsql_rusqlite::params![trimmed, card_id],
                    );
                }

                // Pipeline-driven: terminal states are "done" equivalents
                let is_terminal = crate::pipeline::try_get()
                    .map(|p| p.is_terminal(&card_status))
                    .unwrap_or(card_status == "done" || card_status == "cancelled");
                if issue.state == "CLOSED" && !is_terminal {
                    cards_to_close.push((card_id.clone(), issue.number));
                } else if issue.state == "OPEN" && is_terminal {
                    result.inconsistency_count += 1;
                    tracing::warn!(
                        "[github-sync] {repo}#{}: card {} is 'done' but issue is OPEN",
                        issue.number,
                        card_id
                    );
                }
            }
        }

        // Update last_synced_at
        conn.execute(
            "UPDATE github_repos SET last_synced_at = datetime('now') WHERE id = ?1",
            [repo],
        )
        .map_err(|e| format!("update last_synced_at: {e}"))?;
    } // conn dropped here

    // Process closures via central state machine (outside conn lock)
    // Pipeline-driven: resolve terminal state
    crate::pipeline::ensure_loaded();
    let terminal = crate::pipeline::try_get()
        .map(|p| {
            p.states
                .iter()
                .find(|s| s.terminal)
                .map(|s| s.id.as_str())
                .unwrap_or("done")
        })
        .unwrap_or("done");
    for (card_id, issue_number) in &cards_to_close {
        let _ = crate::kanban::transition_status_with_opts(
            db,
            engine,
            card_id,
            terminal,
            "github-sync",
            true,
        );
        result.closed_count += 1;
        tracing::info!(
            "[github-sync] {repo}#{}: card {} → {} (issue closed)",
            issue_number,
            card_id,
            terminal
        );
    }

    Ok(result)
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
    }

    sqlx::query("UPDATE github_repos SET last_synced_at = NOW() WHERE id = $1")
        .bind(repo)
        .execute(pool)
        .await
        .map_err(|error| format!("update last_synced_at: {error}"))?;

    Ok(result)
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
        true,
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
    crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
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
    let dispatched_rows = sqlx::query(
        "SELECT id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1 AND status = 'dispatched'",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load dispatched auto-queue entries for {card_id}: {error}"))?;

    for row in dispatched_rows {
        let entry_id: String = row
            .try_get("id")
            .map_err(|error| format!("read dispatched auto-queue entry id: {error}"))?;
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

    let mut finalize_run_ids = std::collections::BTreeSet::new();
    for row in pending_rows {
        let entry_id: String = row
            .try_get("id")
            .map_err(|error| format!("read pending auto-queue entry id: {error}"))?;
        let run_id: String = row
            .try_get("run_id")
            .map_err(|error| format!("read pending auto-queue run id: {error}"))?;
        sqlx::query(
            "UPDATE auto_queue_entries
             SET status = 'skipped',
                 dispatch_id = NULL,
                 dispatched_at = NULL,
                 completed_at = NOW()
             WHERE id = $1 AND status = 'pending'",
        )
        .bind(&entry_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("skip pending auto-queue entry {entry_id}: {error}"))?;
        record_auto_queue_transition_on_pg(
            tx,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_PENDING,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "card_terminal_pending_cleanup",
        )
        .await?;
        finalize_run_ids.insert(run_id);
    }

    for run_id in finalize_run_ids {
        maybe_finalize_run_after_terminal_entry_pg(tx, &run_id).await?;
    }

    Ok(())
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

async fn maybe_finalize_run_after_terminal_entry_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let blocking_gate_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_phase_gates
         WHERE run_id = $1 AND status IN ('pending', 'failed')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("check blocking phase gates for run {run_id}: {error}"))?;
    if blocking_gate_count > 0 {
        return Ok(());
    }

    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE run_id = $1 AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count remaining auto-queue entries for run {run_id}: {error}"))?;
    if remaining > 0 {
        return Ok(());
    }

    sqlx::query(
        "UPDATE auto_queue_slots
         SET assigned_run_id = NULL,
             assigned_thread_group = NULL,
             updated_at = NOW()
         WHERE assigned_run_id = $1",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("release auto-queue slots for run {run_id}: {error}"))?;

    sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1 AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("complete auto-queue run {run_id}: {error}"))?;

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
pub fn sync_all_repos(db: &Db, engine: &crate::engine::PolicyEngine) -> Result<SyncResult, String> {
    let repos = super::list_repos(db)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::test_utils::RecordingAdapter;

    fn test_db() -> Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &crate::db::Db) -> crate::engine::PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        crate::engine::PolicyEngine::new(&config, db.clone()).unwrap()
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
                    "closed:>2026-03-13".to_string(),
                ],
            ]
        );
    }

    #[test]
    fn sync_closes_card_when_issue_closed() {
        let db = test_db();

        // Register repo and create a card linked to issue #5
        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_number, created_at, updated_at)
                 VALUES ('c1', 'owner/repo', 'Fix bug', 'in_progress', 'medium', 5, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let issues = vec![GhIssue {
            number: 5,
            state: "CLOSED".to_string(),
            title: "Fix bug".to_string(),
            labels: vec![],
            body: None,
        }];

        let result =
            sync_github_issues_for_repo(&db, &test_engine(&db), "owner/repo", &issues).unwrap();
        assert_eq!(result.closed_count, 1);
        assert_eq!(result.inconsistency_count, 0);

        // Verify card is now done
        let conn = db.lock().unwrap();
        let status: String = conn
            .query_row("SELECT status FROM kanban_cards WHERE id = 'c1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "done");
    }

    #[test]
    fn sync_flags_inconsistency_when_open_issue_has_done_card() {
        let db = test_db();

        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_number, created_at, updated_at)
                 VALUES ('c1', 'owner/repo', 'Feature', 'done', 'medium', 10, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let issues = vec![GhIssue {
            number: 10,
            state: "OPEN".to_string(),
            title: "Feature".to_string(),
            labels: vec![],
            body: None,
        }];

        let result =
            sync_github_issues_for_repo(&db, &test_engine(&db), "owner/repo", &issues).unwrap();
        assert_eq!(result.closed_count, 0);
        assert_eq!(result.inconsistency_count, 1);
    }

    #[test]
    fn sync_skips_already_done_cards() {
        let db = test_db();

        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_number, created_at, updated_at)
                 VALUES ('c1', 'owner/repo', 'Done thing', 'done', 'medium', 7, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let issues = vec![GhIssue {
            number: 7,
            state: "CLOSED".to_string(),
            title: "Done thing".to_string(),
            labels: vec![],
            body: None,
        }];

        let result =
            sync_github_issues_for_repo(&db, &test_engine(&db), "owner/repo", &issues).unwrap();
        assert_eq!(result.closed_count, 0);
        assert_eq!(result.inconsistency_count, 0);
    }

    #[test]
    fn sync_and_triage_flow_can_run_with_mock_adapter() {
        let db = test_db();

        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_number, created_at, updated_at)
                 VALUES ('closed-card', 'owner/repo', 'Already linked', 'in_progress', 'medium', 7, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let adapter = RecordingAdapter::with_sync_responses(vec![
            Ok(
                r#"[
                    {"number":7,"state":"CLOSED","title":"Already linked","labels":[],"body":"Closed body"},
                    {"number":9,"state":"OPEN","title":"New issue","labels":[{"name":"high"}],"body":"Open body"}
                ]"#
                .to_string(),
            ),
            Ok(
                r#"[
                    {"number":7,"state":"CLOSED","title":"Already linked","labels":[],"body":"Closed body"},
                    {"number":210,"state":"CLOSED","title":"Older issue closed recently","labels":[],"body":"Closed later"}
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
        let triaged = crate::github::triage::triage_new_issues(&db, "owner/repo", &issues).unwrap();
        let synced =
            sync_github_issues_for_repo(&db, &test_engine(&db), "owner/repo", &issues).unwrap();

        assert_eq!(triaged, 1);
        assert_eq!(synced.closed_count, 1);

        let conn = db.lock().unwrap();
        let new_card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE github_issue_number = 9",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let closed_card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'closed-card'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_card_status, "backlog");
        assert_eq!(closed_card_status, "done");
    }
}
