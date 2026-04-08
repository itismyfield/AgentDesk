//! Issue auto-triage: create kanban backlog cards for new GitHub issues.

use crate::db::Db;

use super::sync::GhIssue;

/// Find GitHub issues that don't have kanban cards yet and create backlog cards for them.
///
/// Returns the number of new cards created.
pub fn triage_new_issues(db: &Db, repo: &str, issues: &[GhIssue]) -> Result<usize, String> {
    let conn = db.lock().map_err(|e| format!("db lock: {e}"))?;
    let mut created = 0;

    for issue in issues {
        // Only triage open issues
        if issue.state != "OPEN" {
            continue;
        }

        // Check if a kanban card already exists for this issue
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
                rusqlite::params![issue.number, repo],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if exists {
            continue;
        }

        // Create a backlog card
        let card_id = uuid::Uuid::new_v4().to_string();
        let labels_str: String = issue
            .labels
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>()
            .join(",");

        let github_url = format!("https://github.com/{repo}/issues/{}", issue.number);

        let priority = infer_priority(&issue.labels);

        conn.execute(
            "INSERT OR IGNORE INTO kanban_cards (id, repo_id, title, status, priority, github_issue_url, github_issue_number, description, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'backlog', ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))",
            rusqlite::params![
                card_id,
                repo,
                issue.title,
                priority,
                github_url,
                issue.number,
                issue.body,
                if labels_str.is_empty() {
                    None
                } else {
                    Some(serde_json::json!({"labels": labels_str}).to_string())
                },
            ],
        )
        .map_err(|e| format!("insert card: {e}"))?;

        tracing::info!(
            "[triage] Created backlog card for {repo}#{}: {}",
            issue.number,
            issue.title
        );
        created += 1;
    }

    Ok(created)
}

/// Simple priority inference from labels.
fn infer_priority(labels: &[super::sync::GhLabel]) -> &'static str {
    for label in labels {
        let name = label.name.to_lowercase();
        if name.contains("critical") || name.contains("urgent") || name.contains("p0") {
            return "critical";
        }
        if name.contains("high") || name.contains("p1") {
            return "high";
        }
        if name.contains("low") || name.contains("p3") {
            return "low";
        }
    }
    "medium"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::sync::{GhIssue, GhLabel};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn triage_creates_backlog_cards() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
        }

        let issues = vec![
            GhIssue {
                number: 1,
                state: "OPEN".to_string(),
                title: "Bug fix needed".to_string(),
                labels: vec![GhLabel {
                    name: "bug".to_string(),
                }],
                body: Some("Description".to_string()),
            },
            GhIssue {
                number: 2,
                state: "OPEN".to_string(),
                title: "New feature".to_string(),
                labels: vec![],
                body: None,
            },
        ];

        let count = triage_new_issues(&db, "owner/repo", &issues).unwrap();
        assert_eq!(count, 2);

        // Verify cards exist
        let conn = db.lock().unwrap();
        let card_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards WHERE repo_id = 'owner/repo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(card_count, 2);

        // Check first card details (with body)
        let (title, status, issue_num, desc): (String, String, i64, Option<String>) = conn
            .query_row(
                "SELECT title, status, github_issue_number, description FROM kanban_cards WHERE github_issue_number = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(title, "Bug fix needed");
        assert_eq!(status, "backlog");
        assert_eq!(issue_num, 1);
        assert_eq!(desc.as_deref(), Some("Description"));

        // Check second card (no body)
        let desc2: Option<String> = conn
            .query_row(
                "SELECT description FROM kanban_cards WHERE github_issue_number = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(desc2.is_none());
    }

    #[test]
    fn triage_skips_existing_cards() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_number, created_at, updated_at)
                 VALUES ('existing', 'owner/repo', 'Existing', 'backlog', 'medium', 1, datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let issues = vec![GhIssue {
            number: 1,
            state: "OPEN".to_string(),
            title: "Bug fix".to_string(),
            labels: vec![],
            body: None,
        }];

        let count = triage_new_issues(&db, "owner/repo", &issues).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn triage_skips_closed_issues() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute("INSERT INTO github_repos (id) VALUES ('owner/repo')", [])
                .unwrap();
        }

        let issues = vec![GhIssue {
            number: 1,
            state: "CLOSED".to_string(),
            title: "Old bug".to_string(),
            labels: vec![],
            body: None,
        }];

        let count = triage_new_issues(&db, "owner/repo", &issues).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn priority_inference_from_labels() {
        assert_eq!(
            infer_priority(&[GhLabel {
                name: "P0-critical".to_string()
            }]),
            "critical"
        );
        assert_eq!(
            infer_priority(&[GhLabel {
                name: "priority:high".to_string()
            }]),
            "high"
        );
        assert_eq!(
            infer_priority(&[GhLabel {
                name: "p3-low".to_string()
            }]),
            "low"
        );
        assert_eq!(
            infer_priority(&[GhLabel {
                name: "enhancement".to_string()
            }]),
            "medium"
        );
        assert_eq!(infer_priority(&[]), "medium");
    }
}
