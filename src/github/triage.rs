//! Issue auto-triage: create kanban backlog cards for new GitHub issues.

use crate::db::kanban::{IssueCardUpsert, upsert_card_from_issue_pg};
use sqlx::PgPool;
use std::collections::BTreeSet;
use std::time::Duration;

use super::sync::GhIssue;

const AGENT_LABEL_PREFIX: &str = "agent:";

/// Declarative signal table for deterministic issue routing.
///
/// Multiple matching rules may share the same owner. If signals point at more
/// than one owner, the issue remains unassigned so the existing PMD fallback can
/// classify it.
const AGENT_ROUTING_RULES: &[AgentRoutingRule] = &[
    AgentRoutingRule {
        agent_id: "adk-dashboard",
        confidence: 90,
        signals: &["dashboard", "frontend", "kanbanheadersurface", "dashboard/"],
    },
    AgentRoutingRule {
        agent_id: "project-agentdesk",
        confidence: 90,
        signals: &[
            "relay",
            "discord",
            "tui",
            "tmux",
            "codex-tui",
            "turn_bridge",
            "inflight",
            "watcher",
        ],
    },
    AgentRoutingRule {
        agent_id: "token-manager",
        confidence: 90,
        signals: &[
            "token",
            "rate limit",
            "rate_limit",
            "rate-limit",
            "quota",
            "usage",
        ],
    },
    AgentRoutingRule {
        // GitHub currently has no agent:adk-e2e-orchestrator label, so keep
        // E2E routing on the existing AgentDesk owner label.
        agent_id: "project-agentdesk",
        confidence: 85,
        signals: &["e2e", "tui-relay-e2e", "scenario", "tests/e2e/"],
    },
    AgentRoutingRule {
        agent_id: "project-agentdesk",
        confidence: 95,
        signals: &["area:security", "ci-red"],
    },
];

#[derive(Debug, Clone, Copy)]
struct AgentRoutingRule {
    agent_id: &'static str,
    confidence: u8,
    signals: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentRoutingResolution {
    Explicit(String),
    Inferred {
        agent_id: &'static str,
        matches: Vec<AgentRoutingMatch>,
    },
    PmdFallback {
        reason: PmdFallbackReason,
        matches: Vec<AgentRoutingMatch>,
    },
}

impl AgentRoutingResolution {
    fn inferred_agent_id(&self) -> Option<&'static str> {
        match self {
            AgentRoutingResolution::Inferred { agent_id, .. } => Some(agent_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentRoutingMatch {
    agent_id: &'static str,
    signal: &'static str,
    confidence: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PmdFallbackReason {
    NoMatch,
    Ambiguous,
}

#[derive(Debug, Default)]
struct TriageRoutingStats {
    open_issues: usize,
    explicit_agent_label: usize,
    inferred_agent_label: usize,
    pmd_no_match: usize,
    pmd_ambiguous: usize,
}

impl TriageRoutingStats {
    fn record(&mut self, resolution: &AgentRoutingResolution) {
        self.open_issues += 1;
        match resolution {
            AgentRoutingResolution::Explicit(_) => self.explicit_agent_label += 1,
            AgentRoutingResolution::Inferred { .. } => self.inferred_agent_label += 1,
            AgentRoutingResolution::PmdFallback {
                reason: PmdFallbackReason::NoMatch,
                ..
            } => self.pmd_no_match += 1,
            AgentRoutingResolution::PmdFallback {
                reason: PmdFallbackReason::Ambiguous,
                ..
            } => self.pmd_ambiguous += 1,
        }
    }

    fn pmd_fallbacks(&self) -> usize {
        self.pmd_no_match + self.pmd_ambiguous
    }

    fn deterministic_routes(&self) -> usize {
        self.explicit_agent_label + self.inferred_agent_label
    }
}

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
    let mut routing_stats = TriageRoutingStats::default();

    for issue in issues {
        if issue.state != "OPEN" {
            continue;
        }

        let routing = resolve_agent_routing(issue);
        routing_stats.record(&routing);
        let assigned_agent_id = resolve_agent_label_pg(pool, repo, issue, &routing).await?;
        let metadata = labels_metadata_json(
            issue,
            routing.inferred_agent_id(),
            assigned_agent_id.as_deref(),
        );
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

    log_triage_routing_stats(repo, &routing_stats);

    Ok(created)
}

fn labels_metadata_json(
    issue: &GhIssue,
    inferred_agent_id: Option<&str>,
    assigned_agent_id: Option<&str>,
) -> Option<String> {
    let mut labels = issue
        .labels
        .iter()
        .map(|label| label.name.trim())
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    if inferred_agent_id.is_some() {
        if let Some(agent_id) = assigned_agent_id {
            let inferred_label = format!("{AGENT_LABEL_PREFIX}{agent_id}");
            if !labels.iter().any(|label| label == &inferred_label) {
                labels.push(inferred_label);
            }
        }
    }

    if labels.is_empty() {
        None
    } else {
        Some(serde_json::json!({ "labels": labels.join(",") }).to_string())
    }
}

async fn resolve_agent_label_pg(
    pool: &PgPool,
    repo: &str,
    issue: &GhIssue,
    routing: &AgentRoutingResolution,
) -> Result<Option<String>, String> {
    let (agent_id, source) = match routing {
        AgentRoutingResolution::Explicit(agent_id) => (agent_id.as_str(), "explicit label"),
        AgentRoutingResolution::Inferred { agent_id, matches } => {
            tracing::info!(
                "[triage] Inferred agent:{} for {repo}#{} from signals: {}",
                agent_id,
                issue.number,
                routing_matches_summary(matches)
            );
            (*agent_id, "inferred routing")
        }
        AgentRoutingResolution::PmdFallback {
            reason: PmdFallbackReason::NoMatch,
            ..
        } => {
            tracing::info!(
                "[triage] PMD fallback for {repo}#{}: no agent routing signals matched",
                issue.number
            );
            return Ok(None);
        }
        AgentRoutingResolution::PmdFallback {
            reason: PmdFallbackReason::Ambiguous,
            matches,
        } => {
            tracing::info!(
                "[triage] PMD fallback for {repo}#{}: ambiguous agent routing signals: {}",
                issue.number,
                routing_matches_summary(matches)
            );
            return Ok(None);
        }
    };

    let exists = sqlx::query_scalar::<_, String>("SELECT id FROM agents WHERE id = $1")
        .bind(agent_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("resolve agent label {agent_id}: {error}"))?;

    if exists.is_none() {
        tracing::warn!(
            "[triage] Ignoring unknown agent '{}' from {} for issue #{}",
            agent_id,
            source,
            issue.number
        );
    }

    if exists.is_some() && matches!(routing, AgentRoutingResolution::Inferred { .. }) {
        if let Err(error) = write_back_inferred_agent_label(repo, issue.number, agent_id).await {
            tracing::warn!(
                "[triage] Failed to write back inferred agent:{} label for {repo}#{}: {error}",
                agent_id,
                issue.number
            );
        }
    }

    Ok(exists)
}

fn resolve_agent_routing(issue: &GhIssue) -> AgentRoutingResolution {
    if let Some(agent_id) = explicit_agent_label(issue) {
        return AgentRoutingResolution::Explicit(agent_id);
    }

    let signal_text = issue_signal_text(issue);
    let mut matches = Vec::new();
    let mut owners = BTreeSet::new();

    for rule in AGENT_ROUTING_RULES {
        for signal in rule.signals {
            if signal_text.contains(signal) {
                matches.push(AgentRoutingMatch {
                    agent_id: rule.agent_id,
                    signal,
                    confidence: rule.confidence,
                });
                owners.insert(rule.agent_id);
            }
        }
    }

    match owners.len() {
        0 => AgentRoutingResolution::PmdFallback {
            reason: PmdFallbackReason::NoMatch,
            matches,
        },
        1 => AgentRoutingResolution::Inferred {
            agent_id: owners.into_iter().next().expect("one owner"),
            matches,
        },
        _ => AgentRoutingResolution::PmdFallback {
            reason: PmdFallbackReason::Ambiguous,
            matches,
        },
    }
}

fn explicit_agent_label(issue: &GhIssue) -> Option<String> {
    issue.labels.iter().find_map(|label| {
        let raw = label.name.trim();
        raw.strip_prefix(AGENT_LABEL_PREFIX)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
    })
}

fn issue_signal_text(issue: &GhIssue) -> String {
    let mut text = String::new();
    text.push_str(&issue.title.to_ascii_lowercase());
    text.push('\n');
    if let Some(body) = issue.body.as_deref() {
        text.push_str(&body.to_ascii_lowercase());
        text.push('\n');
    }
    for label in &issue.labels {
        text.push_str(&label.name.to_ascii_lowercase());
        text.push('\n');
    }
    text
}

fn routing_matches_summary(matches: &[AgentRoutingMatch]) -> String {
    if matches.is_empty() {
        return "none".to_string();
    }

    matches
        .iter()
        .map(|matched| {
            format!(
                "{}->{}@{}",
                matched.signal, matched.agent_id, matched.confidence
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn log_triage_routing_stats(repo: &str, stats: &TriageRoutingStats) {
    if stats.open_issues == 0 {
        return;
    }

    let deterministic = stats.deterministic_routes();
    let pmd_fallbacks = stats.pmd_fallbacks();
    let deterministic_pct = percentage(deterministic, stats.open_issues);
    let pmd_fallback_pct = percentage(pmd_fallbacks, stats.open_issues);

    tracing::info!(
        "[triage] Routing coverage for {repo}: open={}, deterministic={} ({:.1}%), explicit={}, inferred={}, pmd_fallback={} ({:.1}%), no_match={}, ambiguous={}",
        stats.open_issues,
        deterministic,
        deterministic_pct,
        stats.explicit_agent_label,
        stats.inferred_agent_label,
        pmd_fallbacks,
        pmd_fallback_pct,
        stats.pmd_no_match,
        stats.pmd_ambiguous
    );
}

fn percentage(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64) * 100.0
    }
}

async fn write_back_inferred_agent_label(
    repo: &str,
    issue_number: i64,
    agent_id: &str,
) -> Result<(), String> {
    let issue_number_arg = issue_number.to_string();
    let label = format!("{AGENT_LABEL_PREFIX}{agent_id}");
    super::adapter()
        .run_async(
            vec![
                "issue".to_string(),
                "edit".to_string(),
                issue_number_arg,
                "--repo".to_string(),
                repo.to_string(),
                "--add-label".to_string(),
                label,
            ],
            Duration::from_secs(10),
            format!(
                "gh issue edit add inferred agent label timed out after 10s: {repo}#{issue_number}"
            ),
        )
        .await
        .map(|_| ())
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

    fn issue(title: &str, body: Option<&str>, labels: &[&str]) -> GhIssue {
        GhIssue {
            number: 42,
            state: "OPEN".to_string(),
            title: title.to_string(),
            labels: labels
                .iter()
                .map(|name| GhLabel {
                    name: (*name).to_string(),
                })
                .collect(),
            body: body.map(str::to_string),
            url: None,
            closed_at: None,
            closed_by_pull_requests_references: Vec::new(),
        }
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

    #[test]
    fn explicit_agent_label_takes_precedence_over_inference() {
        let issue = issue(
            "Dashboard frontend regression",
            Some("dashboard/ touches KanbanHeaderSurface"),
            &["agent:project-agentdesk"],
        );

        assert_eq!(
            resolve_agent_routing(&issue),
            AgentRoutingResolution::Explicit("project-agentdesk".to_string())
        );
    }

    #[test]
    fn single_owner_signal_infers_agent() {
        let issue = issue(
            "Token quota usage report drift",
            Some("rate_limit budget exceeded"),
            &[],
        );

        let resolution = resolve_agent_routing(&issue);
        match resolution {
            AgentRoutingResolution::Inferred { agent_id, matches } => {
                assert_eq!(agent_id, "token-manager");
                assert!(matches.iter().any(
                    |matched| matched.signal == "quota" && matched.agent_id == "token-manager"
                ));
            }
            other => panic!("expected token-manager inference, got {other:?}"),
        }
    }

    #[test]
    fn no_signal_leaves_pmd_fallback() {
        let issue = issue("Clarify release note wording", Some("copy edit only"), &[]);

        assert_eq!(
            resolve_agent_routing(&issue),
            AgentRoutingResolution::PmdFallback {
                reason: PmdFallbackReason::NoMatch,
                matches: Vec::new()
            }
        );
    }

    #[test]
    fn conflicting_owner_signals_leave_pmd_fallback() {
        let issue = issue(
            "Dashboard token usage panel",
            Some("frontend shows quota metrics"),
            &[],
        );

        let resolution = resolve_agent_routing(&issue);
        match resolution {
            AgentRoutingResolution::PmdFallback {
                reason: PmdFallbackReason::Ambiguous,
                matches,
            } => {
                let owners = matches
                    .iter()
                    .map(|matched| matched.agent_id)
                    .collect::<BTreeSet<_>>();
                assert_eq!(owners, BTreeSet::from(["adk-dashboard", "token-manager"]));
            }
            other => panic!("expected ambiguous PMD fallback, got {other:?}"),
        }
    }

    #[test]
    fn inferred_agent_label_is_added_to_local_metadata_after_agent_validation() {
        let issue = issue("Discord relay watcher", None, &["bug"]);

        assert_eq!(
            labels_metadata_json(&issue, Some("project-agentdesk"), Some("project-agentdesk")),
            Some(r#"{"labels":"bug,agent:project-agentdesk"}"#.to_string())
        );
        assert_eq!(
            labels_metadata_json(&issue, Some("project-agentdesk"), None),
            Some(r#"{"labels":"bug"}"#.to_string())
        );
    }
}
