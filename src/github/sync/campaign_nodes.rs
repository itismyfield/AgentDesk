//! Campaign step of a repo sync: reads linked issues by number together with the
//! `ClosedEvent` that closed them, then lets `campaigns::issue_sync` update the nodes.
use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{GhIssue, STALE_CARD_RECONCILE_TIMEOUT_SECS, parse_repo_owner_name};
use crate::db::campaigns::issue_sync::{self, Closer, ClosingEvent, IssueFact, IssueSyncReport};
use crate::github::GitHubAdapter;

const ISSUE_BATCH_SIZE: usize = 25;
/// A closing event counts as the current closing when it lies this close to `closedAt`.
const CLOSING_EVENT_TOLERANCE_SECS: i64 = 60;

pub(super) async fn sync_campaign_nodes(
    pool: &PgPool,
    repo: &str,
    issues: &[GhIssue],
    adapter: &dyn GitHubAdapter,
) {
    let campaigns = match issue_sync::active_campaigns(pool).await {
        Ok(campaigns) => campaigns,
        Err(error) => {
            tracing::warn!("[github-sync] {repo}: load active campaigns failed: {error}");
            return;
        }
    };
    let linked = issue_sync::linked_numbers(&campaigns, repo);
    if linked.is_empty() {
        return;
    }
    let mut facts: HashMap<i64, IssueFact> = issues
        .iter()
        .filter(|issue| linked.contains(&issue.number))
        .filter_map(|issue| Some((issue.number, listed_fact(issue)?)))
        .collect();
    let targets = issue_sync::fetch_targets(&campaigns, repo, &facts);
    let mut fetch_errors = 0;
    for batch in targets.chunks(ISSUE_BATCH_SIZE) {
        match fetch_issue_batch(adapter, repo, batch).await {
            Ok(fetched) => facts.extend(fetched),
            Err(error) => {
                fetch_errors += 1;
                // Without a fresh read these issues cannot change a node this cycle.
                for number in batch {
                    facts.remove(number);
                }
                tracing::warn!(
                    "[github-sync] {repo}: campaign issue batch of {} failed: {error}",
                    batch.len()
                );
            }
        }
    }
    let ids: Vec<String> = campaigns
        .iter()
        .filter(|campaign| {
            !issue_sync::linked_numbers(std::slice::from_ref(*campaign), repo).is_empty()
        })
        .map(|campaign| campaign.id.clone())
        .collect();
    let report: IssueSyncReport = issue_sync::apply_issue_facts(pool, repo, &facts, &ids).await;
    if report != IssueSyncReport::default() || fetch_errors > 0 {
        tracing::info!(
            "[github-sync] {repo}: campaign nodes changed={}, deferred={}, errors={}, fetched_issues={}, fetch_errors={}",
            report.changed_nodes,
            report.deferred_nodes,
            report.errors,
            targets.len(),
            fetch_errors
        );
    }
}

fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn listed_fact(issue: &GhIssue) -> Option<IssueFact> {
    let open = match issue.state.as_str() {
        "OPEN" => true,
        "CLOSED" => false,
        _ => return None,
    };
    Some(IssueFact {
        open,
        state_reason: None,
        closed_at: parse_time(issue.closed_at.as_deref()),
        closing: None,
        fetched_at: None,
    })
}

fn issue_batch_query(numbers: &[i64]) -> String {
    let fields = numbers
        .iter()
        .enumerate()
        .map(|(index, number)| {
            format!(
                "i{index}: issue(number: {number}) {{ number state stateReason closedAt \
                 timelineItems(itemTypes: [CLOSED_EVENT], last: 1) {{ nodes {{ ... on ClosedEvent {{ \
                 createdAt actor {{ login }} closer {{ __typename \
                 ... on PullRequest {{ number url merged }} ... on Commit {{ oid url }} }} }} }} }} }}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields} }} }}"
    )
}

async fn fetch_issue_batch(
    adapter: &dyn GitHubAdapter,
    repo: &str,
    numbers: &[i64],
) -> Result<HashMap<i64, IssueFact>, String> {
    let (owner, name) = parse_repo_owner_name(repo)?;
    let args = vec![
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("owner={owner}"),
        "-f".to_string(),
        format!("name={name}"),
        "-f".to_string(),
        format!("query={}", issue_batch_query(numbers)),
    ];
    // Taken before the request, so a node written after it is newer than this read.
    let fetched_at = Utc::now();
    let output = adapter
        .run_async(
            args,
            Duration::from_secs(STALE_CARD_RECONCILE_TIMEOUT_SECS),
            format!("github campaign issue read timed out for {repo}"),
        )
        .await?;
    parse_issue_batch(&output, fetched_at)
}

#[derive(serde::Deserialize)]
struct BatchResponse {
    data: Option<BatchData>,
    #[serde(default)]
    errors: Vec<BatchError>,
}

#[derive(serde::Deserialize)]
struct BatchData {
    repository: Option<HashMap<String, Option<BatchIssue>>>,
}

#[derive(serde::Deserialize)]
struct BatchError {
    message: String,
}

#[derive(serde::Deserialize)]
struct BatchIssue {
    number: i64,
    state: String,
    #[serde(rename = "stateReason")]
    state_reason: Option<String>,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(rename = "timelineItems")]
    timeline_items: Option<Timeline>,
}

#[derive(serde::Deserialize)]
struct Timeline {
    #[serde(default)]
    nodes: Vec<Option<ClosedEventNode>>,
}

#[derive(serde::Deserialize)]
struct ClosedEventNode {
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    actor: Option<Actor>,
    closer: Option<CloserNode>,
}

#[derive(serde::Deserialize)]
struct Actor {
    login: String,
}

#[derive(serde::Deserialize)]
struct CloserNode {
    #[serde(rename = "__typename")]
    typename: String,
    number: Option<i64>,
    url: Option<String>,
    merged: Option<bool>,
    oid: Option<String>,
}

fn parse_issue_batch(
    output: &str,
    fetched_at: DateTime<Utc>,
) -> Result<HashMap<i64, IssueFact>, String> {
    let response: BatchResponse = serde_json::from_str(output)
        .map_err(|error| format!("parse campaign issue GraphQL response: {error}"))?;
    if !response.errors.is_empty() {
        let messages: Vec<&str> = response.errors.iter().map(|e| e.message.as_str()).collect();
        return Err(format!(
            "campaign issue GraphQL query failed: {}",
            messages.join("; ")
        ));
    }
    let repository = response
        .data
        .and_then(|data| data.repository)
        .ok_or("campaign issue GraphQL response missing repository")?;
    let mut facts = HashMap::new();
    for issue in repository.into_values().flatten() {
        let open = match issue.state.as_str() {
            "OPEN" => true,
            "CLOSED" => false,
            _ => continue,
        };
        let closed_at = parse_time(issue.closed_at.as_deref());
        let closing = if open {
            None
        } else {
            current_closing(issue.timeline_items, closed_at)
        };
        facts.insert(
            issue.number,
            IssueFact {
                open,
                state_reason: issue.state_reason,
                closed_at,
                closing,
                fetched_at: Some(fetched_at),
            },
        );
    }
    Ok(facts)
}

/// The latest closing event, if it is the one behind `closedAt`.
fn current_closing(
    timeline: Option<Timeline>,
    closed_at: Option<DateTime<Utc>>,
) -> Option<ClosingEvent> {
    let event = timeline?.nodes.pop().flatten()?;
    let at = parse_time(event.created_at.as_deref())?;
    if (at - closed_at?).num_seconds().abs() > CLOSING_EVENT_TOLERANCE_SECS {
        return None;
    }
    let actor = event.actor.map(|actor| actor.login);
    let closer = match event.closer {
        None => Closer::Nobody { actor },
        Some(closer) => match closer.typename.as_str() {
            "PullRequest" => Closer::PullRequest {
                number: closer.number?,
                url: closer.url?,
                merged: closer.merged.unwrap_or(false),
            },
            "Commit" => Closer::Commit {
                oid: closer.oid?,
                url: closer.url?,
            },
            _ => Closer::Nobody { actor },
        },
    };
    Some(ClosingEvent { at, closer })
}

#[cfg(test)]
#[path = "campaign_nodes_tests.rs"]
mod tests;
