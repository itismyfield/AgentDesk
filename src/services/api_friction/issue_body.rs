use sqlx::PgPool;

use super::patterns::ApiFrictionPattern;

const MAX_ISSUE_EVIDENCE_ITEMS: usize = 5;

pub(super) async fn build_issue_body_pg(
    pg_pool: &PgPool,
    pattern: &ApiFrictionPattern,
) -> Result<String, String> {
    let evidence = load_pattern_evidence_pg(pg_pool, &pattern.fingerprint).await?;
    build_issue_body_from_evidence(pattern, evidence)
}

fn build_issue_body_from_evidence(
    pattern: &ApiFrictionPattern,
    evidence: Vec<PatternEvidence>,
) -> Result<String, String> {
    let mut lines = vec![
        "## Summary".to_string(),
        format!("- Endpoint/Surface: `{}`", pattern.endpoint),
        format!("- Friction type: `{}`", pattern.friction_type),
        format!("- Repeated count: {}", pattern.event_count),
    ];
    if let Some(docs_category) = pattern.docs_category.as_deref() {
        lines.push(format!("- Docs category: `{docs_category}`"));
    }
    if let Some(task_summary) = pattern.task_summary.as_deref() {
        lines.push(format!("- Latest task: {}", task_summary));
    }

    lines.extend([
        String::new(),
        "## Friction Pattern".to_string(),
        format!("- Summary: {}", pattern.summary),
        format!(
            "- Workaround: {}",
            pattern.workaround.as_deref().unwrap_or("not provided")
        ),
        format!(
            "- Proposed improvement: {}",
            pattern
                .suggested_fix
                .as_deref()
                .unwrap_or("Provide a clearer single API path or docs entry")
        ),
        String::new(),
        "## Evidence".to_string(),
    ]);

    if evidence.is_empty() {
        lines.push("- No card-linked evidence was captured.".to_string());
    } else {
        for item in evidence {
            let mut parts = Vec::new();
            if let Some(repo_id) = item.repo_id.as_deref() {
                if let Some(issue_number) = item.issue_number {
                    parts.push(format!("{repo_id}#{issue_number}"));
                } else {
                    parts.push(repo_id.to_string());
                }
            } else if let Some(card_id) = item.card_id.as_deref() {
                parts.push(format!("card {card_id}"));
            }
            if let Some(dispatch_id) = item.dispatch_id.as_deref() {
                parts.push(format!("dispatch {dispatch_id}"));
            }
            if parts.is_empty() {
                parts.push("runtime observation".to_string());
            }
            lines.push(format!("- {}: {}", parts.join(", "), item.summary));
        }
    }

    lines.extend([
        String::new(),
        "## Suggested Next Step".to_string(),
        "- Add or clarify the canonical `/api` endpoint/docs path so agents do not need trial-and-error or DB bypass.".to_string(),
    ]);

    Ok(lines.join("\n"))
}

#[derive(Clone, Debug)]
struct PatternEvidence {
    repo_id: Option<String>,
    issue_number: Option<i64>,
    card_id: Option<String>,
    dispatch_id: Option<String>,
    summary: String,
}

async fn load_pattern_evidence_pg(
    pg_pool: &PgPool,
    fingerprint: &str,
) -> Result<Vec<PatternEvidence>, String> {
    sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            String,
        ),
    >(
        "SELECT repo_id, github_issue_number::BIGINT, card_id, dispatch_id, summary
         FROM api_friction_events
         WHERE fingerprint = $1
         ORDER BY created_at DESC, id DESC
         LIMIT $2",
    )
    .bind(fingerprint)
    .bind(MAX_ISSUE_EVIDENCE_ITEMS as i64)
    .fetch_all(pg_pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(repo_id, issue_number, card_id, dispatch_id, summary)| PatternEvidence {
                    repo_id,
                    issue_number,
                    card_id,
                    dispatch_id,
                    summary,
                },
            )
            .collect()
    })
    .map_err(|err| format!("query api_friction evidence: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern() -> ApiFrictionPattern {
        ApiFrictionPattern {
            fingerprint: "api-docs-kanban::docs-bypass".to_string(),
            endpoint: "/api/docs/kanban".to_string(),
            friction_type: "docs-bypass".to_string(),
            docs_category: Some("kanban".to_string()),
            summary: "missing kanban docs".to_string(),
            workaround: Some("read source".to_string()),
            suggested_fix: Some("document the endpoint".to_string()),
            repo_id: "itismyfield/AgentDesk".to_string(),
            event_count: 3,
            first_seen_at: "2026-05-07 00:00:00+00".to_string(),
            last_seen_at: "2026-05-07 00:03:00+00".to_string(),
            task_summary: Some("latest task".to_string()),
            github_issue_number: None,
            issue_url: None,
            last_error: None,
        }
    }

    #[test]
    fn issue_body_fallback_copy_stays_stable_without_evidence() {
        let mut pattern = pattern();
        pattern.docs_category = None;
        pattern.task_summary = None;
        pattern.workaround = None;
        pattern.suggested_fix = None;

        let body = build_issue_body_from_evidence(&pattern, Vec::new()).unwrap();

        assert!(body.contains("- Workaround: not provided"));
        assert!(
            body.contains(
                "- Proposed improvement: Provide a clearer single API path or docs entry"
            ),
            "{body}"
        );
        assert!(body.contains("- No card-linked evidence was captured."));
        assert!(body.contains("## Suggested Next Step"));
    }

    #[test]
    fn issue_body_evidence_formatting_stays_stable() {
        let body = build_issue_body_from_evidence(
            &pattern(),
            vec![
                PatternEvidence {
                    repo_id: Some("itismyfield/AgentDesk".to_string()),
                    issue_number: Some(1835),
                    card_id: Some("card-ignored-when-repo-present".to_string()),
                    dispatch_id: Some("dispatch-1".to_string()),
                    summary: "repo issue evidence".to_string(),
                },
                PatternEvidence {
                    repo_id: None,
                    issue_number: None,
                    card_id: Some("card-1".to_string()),
                    dispatch_id: Some("dispatch-2".to_string()),
                    summary: "card evidence".to_string(),
                },
                PatternEvidence {
                    repo_id: None,
                    issue_number: None,
                    card_id: None,
                    dispatch_id: None,
                    summary: "runtime evidence".to_string(),
                },
            ],
        )
        .unwrap();

        assert!(body.contains("- Docs category: `kanban`"));
        assert!(body.contains("- Latest task: latest task"));
        assert!(
            body.contains("- itismyfield/AgentDesk#1835, dispatch dispatch-1: repo issue evidence")
        );
        assert!(body.contains("- card card-1, dispatch dispatch-2: card evidence"));
        assert!(body.contains("- runtime observation: runtime evidence"));
    }

    #[test]
    fn issue_body_evidence_query_cap_stays_stable() {
        assert_eq!(MAX_ISSUE_EVIDENCE_ITEMS, 5);
    }
}
