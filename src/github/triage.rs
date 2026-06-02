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
    Unrouted {
        reason: UnroutedReason,
        matches: Vec<AgentRoutingMatch>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentRoutingMatch {
    agent_id: &'static str,
    signal: &'static str,
    confidence: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnroutedReason {
    NoMatch,
    Ambiguous,
    ExistingCardAssigned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingIssueCardAssignment {
    Missing,
    Unassigned,
    Assigned,
}

impl ExistingIssueCardAssignment {
    fn allows_inferred_routing(self) -> bool {
        matches!(
            self,
            ExistingIssueCardAssignment::Missing | ExistingIssueCardAssignment::Unassigned
        )
    }
}

#[derive(Debug, Default)]
struct ValidatedAgentRouting {
    assigned_agent_id: Option<String>,
    unknown_agent_id: Option<String>,
}

#[derive(Debug, Default)]
struct TriageRoutingStats {
    open_issues: usize,
    explicit_agent_label: usize,
    inferred_agent_label: usize,
    pmd_no_match: usize,
    pmd_ambiguous: usize,
    skipped_existing_assigned: usize,
    unknown_agent: usize,
}

impl TriageRoutingStats {
    fn record(&mut self, outcome: TriageRoutingOutcome) {
        self.open_issues += 1;
        match outcome {
            TriageRoutingOutcome::Explicit => self.explicit_agent_label += 1,
            TriageRoutingOutcome::Inferred => self.inferred_agent_label += 1,
            TriageRoutingOutcome::PmdNoMatch => self.pmd_no_match += 1,
            TriageRoutingOutcome::PmdAmbiguous => self.pmd_ambiguous += 1,
            TriageRoutingOutcome::SkippedExistingAssigned => self.skipped_existing_assigned += 1,
            TriageRoutingOutcome::UnknownAgent => self.unknown_agent += 1,
        }
    }

    fn pmd_fallbacks(&self) -> usize {
        self.pmd_no_match + self.pmd_ambiguous
    }

    fn deterministic_routes(&self) -> usize {
        self.explicit_agent_label + self.inferred_agent_label
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriageRoutingOutcome {
    Explicit,
    Inferred,
    PmdNoMatch,
    PmdAmbiguous,
    SkippedExistingAssigned,
    UnknownAgent,
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

        let existing_assignment =
            existing_issue_card_assignment_pg(pool, repo, issue.number).await?;
        let routing = resolve_agent_routing(issue, existing_assignment.allows_inferred_routing());
        let validated = validate_agent_routing_pg(pool, repo, issue, &routing).await?;
        let assigned_agent_id = match &routing {
            AgentRoutingResolution::Explicit(_) => validated.assigned_agent_id.clone(),
            _ => None,
        };
        let metadata = labels_metadata_json(issue, None);
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

        let routing_outcome = finalize_inferred_routing_pg(
            pool,
            repo,
            issue,
            &upserted.card_id,
            &routing,
            &validated,
        )
        .await?;
        routing_stats.record(routing_outcome);

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

fn labels_metadata_json(issue: &GhIssue, inferred_agent_id: Option<&str>) -> Option<String> {
    let mut labels = issue
        .labels
        .iter()
        .map(|label| label.name.trim())
        .filter(|label| !label.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    if let Some(agent_id) = inferred_agent_id {
        let inferred_label = format!("{AGENT_LABEL_PREFIX}{agent_id}");
        if !labels.iter().any(|label| label == &inferred_label) {
            labels.push(inferred_label);
        }
    }

    if labels.is_empty() {
        None
    } else {
        Some(serde_json::json!({ "labels": labels.join(",") }).to_string())
    }
}

async fn existing_issue_card_assignment_pg(
    pool: &PgPool,
    repo: &str,
    issue_number: i64,
) -> Result<ExistingIssueCardAssignment, String> {
    let assigned_agent_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT assigned_agent_id
         FROM kanban_cards
         WHERE repo_id = $1
           AND github_issue_number = $2",
    )
    .bind(repo)
    .bind(issue_number)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("read existing issue card {repo}#{issue_number}: {error}"))?;

    Ok(match assigned_agent_id {
        None => ExistingIssueCardAssignment::Missing,
        Some(Some(agent_id)) if !agent_id.trim().is_empty() => {
            ExistingIssueCardAssignment::Assigned
        }
        Some(_) => ExistingIssueCardAssignment::Unassigned,
    })
}

async fn validate_agent_routing_pg(
    pool: &PgPool,
    repo: &str,
    issue: &GhIssue,
    routing: &AgentRoutingResolution,
) -> Result<ValidatedAgentRouting, String> {
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
        AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::NoMatch,
            ..
        } => {
            tracing::info!(
                "[triage] PMD fallback for {repo}#{}: no agent routing signals matched",
                issue.number
            );
            return Ok(ValidatedAgentRouting::default());
        }
        AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::Ambiguous,
            matches,
        } => {
            tracing::info!(
                "[triage] PMD fallback for {repo}#{}: ambiguous agent routing signals: {}",
                issue.number,
                routing_matches_summary(matches)
            );
            return Ok(ValidatedAgentRouting::default());
        }
        AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::ExistingCardAssigned,
            ..
        } => {
            tracing::debug!(
                "[triage] Skipping inferred routing for {repo}#{}: existing card already assigned",
                issue.number
            );
            return Ok(ValidatedAgentRouting::default());
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
        return Ok(ValidatedAgentRouting {
            assigned_agent_id: None,
            unknown_agent_id: Some(agent_id.to_string()),
        });
    }

    Ok(ValidatedAgentRouting {
        assigned_agent_id: exists,
        unknown_agent_id: None,
    })
}

async fn finalize_inferred_routing_pg(
    pool: &PgPool,
    repo: &str,
    issue: &GhIssue,
    card_id: &str,
    routing: &AgentRoutingResolution,
    validated: &ValidatedAgentRouting,
) -> Result<TriageRoutingOutcome, String> {
    if validated.unknown_agent_id.is_some() {
        return Ok(TriageRoutingOutcome::UnknownAgent);
    }

    match routing {
        AgentRoutingResolution::Explicit(_) => Ok(TriageRoutingOutcome::Explicit),
        AgentRoutingResolution::Inferred { .. } => {
            let Some(agent_id) = validated.assigned_agent_id.as_deref() else {
                return Ok(TriageRoutingOutcome::UnknownAgent);
            };
            let assigned = assign_card_if_unassigned_pg(pool, card_id, agent_id).await?;
            if !assigned {
                tracing::info!(
                    "[triage] Skipped inferred agent:{} for {repo}#{}: card {} is no longer unassigned",
                    agent_id,
                    issue.number,
                    card_id
                );
                return Ok(TriageRoutingOutcome::SkippedExistingAssigned);
            }

            update_card_metadata_with_inferred_agent_label_pg(pool, card_id, issue, agent_id)
                .await?;

            // Source-of-truth write-back is deliberately after successful DB
            // assignment so a GitHub label never advertises a route we did not
            // persist locally.
            if let Err(error) = write_back_inferred_agent_label(repo, issue.number, agent_id).await
            {
                tracing::warn!(
                    "[triage] Failed to write back inferred agent:{} label for {repo}#{}: {error}",
                    agent_id,
                    issue.number
                );
            }

            Ok(TriageRoutingOutcome::Inferred)
        }
        AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::NoMatch,
            ..
        } => Ok(TriageRoutingOutcome::PmdNoMatch),
        AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::Ambiguous,
            ..
        } => Ok(TriageRoutingOutcome::PmdAmbiguous),
        AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::ExistingCardAssigned,
            ..
        } => Ok(TriageRoutingOutcome::SkippedExistingAssigned),
    }
}

async fn assign_card_if_unassigned_pg(
    pool: &PgPool,
    card_id: &str,
    agent_id: &str,
) -> Result<bool, String> {
    let updated_id = sqlx::query_scalar::<_, String>(
        "UPDATE kanban_cards
         SET assigned_agent_id = $2,
             updated_at = NOW()
         WHERE id = $1
           AND NULLIF(BTRIM(assigned_agent_id), '') IS NULL
         RETURNING id",
    )
    .bind(card_id)
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("assign inferred agent {agent_id} to card {card_id}: {error}"))?;

    Ok(updated_id.is_some())
}

async fn update_card_metadata_with_inferred_agent_label_pg(
    pool: &PgPool,
    card_id: &str,
    issue: &GhIssue,
    agent_id: &str,
) -> Result<(), String> {
    let metadata = labels_metadata_json(issue, Some(agent_id));
    sqlx::query(
        "UPDATE kanban_cards
         SET metadata = CAST($2 AS jsonb),
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .bind(metadata.as_deref())
    .execute(pool)
    .await
    .map_err(|error| format!("update inferred routing metadata for card {card_id}: {error}"))?;

    Ok(())
}

fn resolve_agent_routing(issue: &GhIssue, allow_inferred_routing: bool) -> AgentRoutingResolution {
    if let Some(agent_id) = explicit_agent_label(issue) {
        return AgentRoutingResolution::Explicit(agent_id);
    }

    if !allow_inferred_routing {
        return AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::ExistingCardAssigned,
            matches: Vec::new(),
        };
    }

    let mut matches = Vec::new();
    let mut owners = BTreeSet::new();

    for rule in AGENT_ROUTING_RULES {
        for signal in rule.signals {
            if issue_matches_signal(issue, signal) {
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
        0 => AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::NoMatch,
            matches,
        },
        1 => AgentRoutingResolution::Inferred {
            agent_id: owners.into_iter().next().expect("one owner"),
            matches,
        },
        _ => AgentRoutingResolution::Unrouted {
            reason: UnroutedReason::Ambiguous,
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

fn issue_matches_signal(issue: &GhIssue, signal: &str) -> bool {
    let signal = signal.to_ascii_lowercase();
    if issue
        .labels
        .iter()
        .any(|label| label.name.trim().eq_ignore_ascii_case(&signal))
    {
        return true;
    }

    let text = issue_signal_text(issue);
    if signal.contains('/') {
        text.contains(&signal)
    } else {
        contains_bounded_signal(&text, &signal)
    }
}

fn contains_bounded_signal(text: &str, signal: &str) -> bool {
    if signal.is_empty() {
        return false;
    }

    let mut search_from = 0;
    while let Some(relative_start) = text[search_from..].find(signal) {
        let start = search_from + relative_start;
        let end = start + signal.len();
        if is_signal_start_boundary(text.as_bytes(), start)
            && is_signal_end_boundary(text.as_bytes(), end)
        {
            return true;
        }
        search_from = end;
    }
    false
}

fn is_signal_start_boundary(bytes: &[u8], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    !matches!(bytes[index - 1], b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
}

fn is_signal_end_boundary(bytes: &[u8], index: usize) -> bool {
    if index >= bytes.len() {
        return true;
    }
    !matches!(bytes[index], b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
}

fn issue_signal_text(issue: &GhIssue) -> String {
    let mut text = String::new();
    text.push_str(&issue.title.to_ascii_lowercase());
    text.push('\n');
    if let Some(body) = issue.body.as_deref() {
        text.push_str(&body.to_ascii_lowercase());
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
        "[triage] Routing coverage for {repo}: open={}, deterministic={} ({:.1}%), explicit={}, inferred={}, pmd_fallback={} ({:.1}%), no_match={}, ambiguous={}, skipped_existing_assigned={}, unknown_agent={}",
        stats.open_issues,
        deterministic,
        deterministic_pct,
        stats.explicit_agent_label,
        stats.inferred_agent_label,
        pmd_fallbacks,
        pmd_fallback_pct,
        stats.pmd_no_match,
        stats.pmd_ambiguous,
        stats.skipped_existing_assigned,
        stats.unknown_agent
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
            resolve_agent_routing(&issue, true),
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

        let resolution = resolve_agent_routing(&issue, true);
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
            resolve_agent_routing(&issue, true),
            AgentRoutingResolution::Unrouted {
                reason: UnroutedReason::NoMatch,
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

        let resolution = resolve_agent_routing(&issue, true);
        match resolution {
            AgentRoutingResolution::Unrouted {
                reason: UnroutedReason::Ambiguous,
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
    fn existing_assigned_card_disables_inference_without_agent_label() {
        let issue = issue("Discord relay watcher", None, &[]);

        assert_eq!(
            resolve_agent_routing(&issue, false),
            AgentRoutingResolution::Unrouted {
                reason: UnroutedReason::ExistingCardAssigned,
                matches: Vec::new()
            }
        );
    }

    #[test]
    fn broad_signals_require_boundaries() {
        let issue = issue(
            "Improve intuitive onboarding",
            Some("Polish tokenizer and usagebased copy."),
            &[],
        );

        assert_eq!(
            resolve_agent_routing(&issue, true),
            AgentRoutingResolution::Unrouted {
                reason: UnroutedReason::NoMatch,
                matches: Vec::new()
            }
        );
    }

    #[test]
    fn path_and_label_signals_still_route() {
        let dashboard_issue = issue("Layout polish", Some("Touched dashboard/src/App.tsx"), &[]);
        let security_issue = issue("Secret exposure", None, &["area:security"]);

        assert!(matches!(
            resolve_agent_routing(&dashboard_issue, true),
            AgentRoutingResolution::Inferred {
                agent_id: "adk-dashboard",
                ..
            }
        ));
        assert!(matches!(
            resolve_agent_routing(&security_issue, true),
            AgentRoutingResolution::Inferred {
                agent_id: "project-agentdesk",
                ..
            }
        ));
    }

    #[test]
    fn unknown_agent_bucket_is_not_deterministic_coverage() {
        let mut stats = TriageRoutingStats::default();

        stats.record(TriageRoutingOutcome::UnknownAgent);
        stats.record(TriageRoutingOutcome::Inferred);

        assert_eq!(stats.open_issues, 2);
        assert_eq!(stats.unknown_agent, 1);
        assert_eq!(stats.deterministic_routes(), 1);
    }

    #[test]
    fn inferred_agent_label_is_added_to_local_metadata_after_assignment() {
        let issue = issue("Discord relay watcher", None, &["bug"]);

        assert_eq!(
            labels_metadata_json(&issue, Some("project-agentdesk")),
            Some(r#"{"labels":"bug,agent:project-agentdesk"}"#.to_string())
        );
        assert_eq!(
            labels_metadata_json(&issue, None),
            Some(r#"{"labels":"bug"}"#.to_string())
        );
    }
}
