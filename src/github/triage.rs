//! Issue auto-triage: create kanban backlog cards for new GitHub issues.

use crate::db::kanban::{IssueCardUpsert, upsert_card_from_issue_pg};
use sqlx::PgPool;

use super::sync::GhIssue;

/// Find GitHub issues that don't have kanban cards yet and create backlog cards for them.
///
/// Returns the number of new cards created.
pub fn triage_new_issues(
    _db: &crate::db::Db,
    _repo: &str,
    _issues: &[GhIssue],
) -> Result<usize, String> {
    Err("postgres backend required for GitHub issue triage; use triage_new_issues_pg".to_string())
}

/// PostgreSQL variant of issue auto-triage.
pub async fn triage_new_issues_pg(
    pool: &PgPool,
    repo: &str,
    issues: &[GhIssue],
) -> Result<usize, String> {
    let mut created = 0;

    for issue in issues {
        if issue.state != "OPEN" {
            continue;
        }

        let metadata = labels_metadata_json(issue);
        let assigned_agent_id = resolve_agent_label_pg(pool, issue).await?;
        let github_url = format!("https://github.com/{repo}/issues/{}", issue.number);
        let upserted = upsert_card_from_issue_pg(
            pool,
            IssueCardUpsert {
                repo_id: repo.to_string(),
                issue_number: issue.number,
                issue_url: Some(github_url),
                title: issue.title.clone(),
                description: issue.body.clone(),
                priority: Some(infer_priority(&issue.labels).to_string()),
                assigned_agent_id,
                metadata_json: metadata,
                status_on_create: Some("backlog".to_string()),
            },
        )
        .await?;

        if upserted.created {
            tracing::info!(
                "[triage] Created backlog card for {repo}#{}: {}",
                issue.number,
                issue.title
            );
            created += 1;
        }
    }

    Ok(created)
}

fn labels_metadata_json(issue: &GhIssue) -> Option<String> {
    let labels = issue
        .labels
        .iter()
        .map(|label| label.name.trim())
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();

    if labels.is_empty() {
        None
    } else {
        Some(serde_json::json!({ "labels": labels.join(",") }).to_string())
    }
}

async fn resolve_agent_label_pg(pool: &PgPool, issue: &GhIssue) -> Result<Option<String>, String> {
    let agent_id = issue.labels.iter().find_map(|label| {
        let raw = label.name.trim();
        raw.strip_prefix("agent:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
    });

    let Some(agent_id) = agent_id else {
        return Ok(None);
    };

    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM agents WHERE id = $1")
        .bind(&agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("resolve agent label {agent_id}: {error}"))?;

    if exists.is_none() {
        tracing::warn!(
            "[triage] Ignoring unknown agent label '{}' for issue #{}",
            agent_id,
            issue.number
        );
    }

    Ok(exists)
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
    use crate::github::sync::GhLabel;

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
