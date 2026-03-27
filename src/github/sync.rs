//! GitHub issue state sync: keep kanban cards consistent with GitHub issue state.

use crate::db::Db;

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
    let output = super::run_gh(&[
        "issue",
        "list",
        "--repo",
        repo,
        "--json",
        "number,state,title,labels,body",
        "--limit",
        "100",
        "--state",
        "all",
    ])?;

    serde_json::from_str::<Vec<GhIssue>>(&output)
        .map_err(|e| format!("failed to parse gh output: {e}"))
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
                .query_map(rusqlite::params![issue.number, repo], |row| {
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
                        rusqlite::params![trimmed, card_id],
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

/// Sync all registered repos (orchestration function).
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
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
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
}
