//! Makes active campaigns' linked nodes follow their GitHub issue. github-sync supplies
//! the observed issues; each campaign is decided and written under its row lock.
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, types::Json};

use super::{
    AutoClosedFrom, Campaign, CampaignError, CampaignStatus, DriftReason, EvidenceRecord,
    IssueDrift, Node, NodeStatus, issue_identity, store_revision,
};
use crate::utils::github_links::normalize_github_issue_url;

pub const AUTO_COMPLETED_STAGE: &str = "완료: 이슈 닫힘 자동 반영";
pub const AUTO_SKIPPED_STAGE: &str = "건너뜀: 이슈 닫힘 자동 반영";

/// What closed the issue, from its latest `ClosedEvent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Closer {
    PullRequest {
        number: i64,
        url: String,
        merged: bool,
    },
    Commit {
        oid: String,
        url: String,
    },
    /// Closed by hand, or by something that is neither a pull request nor a commit.
    Nobody {
        actor: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosingEvent {
    pub at: DateTime<Utc>,
    pub closer: Closer,
}

/// One observation of a linked issue.
#[derive(Clone, Debug)]
pub struct IssueFact {
    pub open: bool,
    pub state_reason: Option<String>,
    pub closed_at: Option<DateTime<Utc>>,
    /// The closing event of the current closing; `None` when it could not be confirmed.
    pub closing: Option<ClosingEvent>,
    /// Set for reads by number; only those newer than the node may change its status, consume
    /// its automatic-close record or clear its drift report. Listed facts may raise drift.
    pub fetched_at: Option<DateTime<Utc>>,
}

/// `deferred_nodes` waited for a fresher read, which is normal; `errors` (campaign load or
/// write) and `fetch_errors` (GitHub issue batches) are failures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IssueSyncReport {
    pub changed_nodes: usize,
    pub deferred_nodes: usize,
    pub errors: usize,
    pub fetch_errors: usize,
}

impl IssueSyncReport {
    pub fn failed(&self) -> bool {
        self.errors > 0 || self.fetch_errors > 0
    }
}

/// One completed or skipped node whose linked issue is open, for the campaign API.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DriftEntry {
    pub node_id: String,
    pub issue_url: String,
    pub node_status: NodeStatus,
    pub reason: DriftReason,
    pub observed_at: DateTime<Utc>,
}

pub fn drift_entries(campaign: &Campaign) -> Vec<DriftEntry> {
    campaign
        .nodes
        .iter()
        .filter(|node| is_finished(node.input.status))
        .filter_map(|node| {
            let drift = node.issue_drift.as_ref()?;
            Some(DriftEntry {
                node_id: node.input.id.clone(),
                issue_url: drift.issue_url.clone(),
                node_status: node.input.status,
                reason: drift.reason,
                observed_at: drift.observed_at,
            })
        })
        .collect()
}

pub async fn active_campaigns(pool: &PgPool) -> Result<Vec<Campaign>, CampaignError> {
    let rows: Vec<Json<Campaign>> = sqlx::query_scalar(
        "SELECT document FROM campaigns WHERE document->>'status' = 'active' ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}

fn is_finished(status: NodeStatus) -> bool {
    matches!(status, NodeStatus::Completed | NodeStatus::Skipped)
}

/// Issue number of a node linked to an issue of `repo`.
fn linked_number(node: &Node, repo: &str) -> Option<i64> {
    let (node_repo, number) = issue_identity(node.input.issue_url.as_deref())?;
    (node_repo == repo.to_lowercase()).then_some(number)
}

/// Issue numbers of `repo` that some node of these campaigns links to.
pub fn linked_numbers(campaigns: &[Campaign], repo: &str) -> HashSet<i64> {
    campaigns
        .iter()
        .flat_map(|campaign| &campaign.nodes)
        .filter_map(|node| linked_number(node, repo))
        .collect()
}

/// Linked issues to read by number: those outside the listed ones, and listed ones whose
/// nodes may change status (a close needs its closing event, any change a fresh read).
pub fn fetch_targets(
    campaigns: &[Campaign],
    repo: &str,
    listed: &HashMap<i64, IssueFact>,
) -> Vec<i64> {
    let mut targets: Vec<i64> = campaigns
        .iter()
        .flat_map(|campaign| &campaign.nodes)
        .filter_map(|node| {
            let number = linked_number(node, repo)?;
            let Some(fact) = listed.get(&number) else {
                return Some(number);
            };
            let may_change = if fact.open {
                node.auto_closed_from.is_some()
            } else {
                // Only a fresh read may clear a drift report.
                node.issue_drift.is_some()
                    || !is_finished(node.input.status)
                        && node.input.post_merge_hold.is_none()
                        && node
                            .auto_closed_from
                            .as_ref()
                            .is_none_or(|auto| auto.issue_closed_at != fact.closed_at)
            };
            may_change.then_some(number)
        })
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Applies `facts` to each listed active campaign, one locked transaction per campaign.
pub async fn apply_issue_facts(
    pool: &PgPool,
    repo: &str,
    facts: &HashMap<i64, IssueFact>,
    campaign_ids: &[String],
) -> IssueSyncReport {
    let mut report = IssueSyncReport::default();
    for id in campaign_ids {
        match apply_to_campaign(pool, id, repo, facts).await {
            Ok(outcome) => {
                report.changed_nodes += outcome.changed;
                report.deferred_nodes += outcome.deferred;
            }
            Err(error) => {
                report.errors += 1;
                tracing::warn!(campaign = %id, %error, "[github-sync] campaign node sync failed");
            }
        }
    }
    report
}

#[derive(Debug, Default)]
struct PlanOutcome {
    changed: usize,
    deferred: usize,
}

async fn apply_to_campaign(
    pool: &PgPool,
    id: &str,
    repo: &str,
    facts: &HashMap<i64, IssueFact>,
) -> Result<PlanOutcome, CampaignError> {
    let mut tx = pool.begin().await?;
    let Some(Json(mut campaign)) = sqlx::query_scalar::<_, Json<Campaign>>(
        "SELECT document FROM campaigns WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(PlanOutcome::default());
    };
    if campaign.status != CampaignStatus::Active {
        return Ok(PlanOutcome::default());
    }
    let locked_revision = campaign.revision;
    let now = Utc::now();
    let outcome = plan(&mut campaign, repo, facts, now);
    if outcome.changed == 0 {
        return Ok(outcome);
    }
    campaign.revision = locked_revision
        .checked_add(1)
        .ok_or(CampaignError::Conflict)?;
    campaign.updated_at = now;
    store_revision(&mut tx, locked_revision, &campaign).await?;
    tx.commit().await?;
    Ok(outcome)
}

fn plan(
    campaign: &mut Campaign,
    repo: &str,
    facts: &HashMap<i64, IssueFact>,
    now: DateTime<Utc>,
) -> PlanOutcome {
    let mut outcome = PlanOutcome::default();
    for node in &mut campaign.nodes {
        let Some(number) = linked_number(node, repo) else {
            continue;
        };
        let Some(fact) = facts.get(&number) else {
            continue;
        };
        let raw_url = node.input.issue_url.clone().unwrap_or_default();
        let issue_url = normalize_github_issue_url(&raw_url).unwrap_or(raw_url);
        let fresh = fact.fetched_at.is_some_and(|at| node.updated_at < at);
        let decision = if fact.open {
            on_open(node, &issue_url, number, fresh, now)
        } else {
            on_closed(node, &issue_url, number, fact, fresh, now)
        };
        match decision {
            Decision::Unchanged => {}
            Decision::Changed => outcome.changed += 1,
            Decision::Deferred => outcome.deferred += 1,
        }
        if decision == Decision::Changed {
            tracing::info!(
                campaign = %campaign.id,
                node = %node.input.id,
                status = ?node.input.status,
                "[github-sync] {repo}#{number}: campaign node follows its issue"
            );
        }
    }
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Unchanged,
    Changed,
    Deferred,
}

fn on_closed(
    node: &mut Node,
    issue_url: &str,
    number: i64,
    fact: &IssueFact,
    fresh: bool,
    now: DateTime<Utc>,
) -> Decision {
    // A listed or older read must not erase a report made after it.
    let cleared = node
        .issue_drift
        .take_if(|drift| fact.fetched_at.is_some_and(|at| at > drift.observed_at))
        .is_some();
    let unchanged = if cleared {
        Decision::Changed
    } else {
        Decision::Unchanged
    };
    if is_finished(node.input.status) || node.input.post_merge_hold.is_some() {
        return unchanged;
    }
    // A node a person reopened after the automatic close stays open for that closing.
    if node
        .auto_closed_from
        .as_ref()
        .is_some_and(|auto| auto.issue_closed_at == fact.closed_at)
    {
        return unchanged;
    }
    let (true, Some(closing)) = (fresh, fact.closing.as_ref()) else {
        return Decision::Deferred;
    };
    let reason = fact.state_reason.as_deref().unwrap_or("COMPLETED");
    let (to_status, to_stage) = if reason == "COMPLETED" {
        (NodeStatus::Completed, AUTO_COMPLETED_STAGE)
    } else {
        (NodeStatus::Skipped, AUTO_SKIPPED_STAGE)
    };
    node.auto_closed_from = Some(AutoClosedFrom {
        status: node.input.status,
        stage: node.input.stage.clone(),
        updated_at: node.updated_at,
        to_status,
        to_stage: to_stage.to_owned(),
        issue_url: issue_url.to_owned(),
        issue_closed_at: fact.closed_at,
        synced_at: now,
        manually_changed: false,
    });
    node.input.status = to_status;
    node.input.stage = to_stage.to_owned();
    node.input.evidence_records.push(closing_record(
        number, issue_url, reason, closing, to_status,
    ));
    node.updated_at = now;
    Decision::Changed
}

fn on_open(
    node: &mut Node,
    issue_url: &str,
    number: i64,
    fresh: bool,
    now: DateTime<Utc>,
) -> Decision {
    if let Some(auto) = node.auto_closed_from.clone() {
        // The record also stops a second close, so only a read newer than the node consumes it.
        if !fresh {
            return Decision::Deferred;
        }
        // Consuming the record is a write, so older reads must not act on this node any more.
        node.auto_closed_from = None;
        node.updated_at = now;
        let untouched = !auto.manually_changed
            && node.input.status == auto.to_status
            && node.input.stage == auto.to_stage;
        if untouched {
            node.input.status = auto.status;
            node.input.stage = auto.stage.clone();
            node.issue_drift = None;
            node.input.evidence_records.push(EvidenceRecord {
                summary: format!(
                    "GitHub issue #{number} reopened; node restored to {}/{} by github-sync",
                    status_label(auto.status),
                    auto.stage
                ),
                command: None,
                result: Some("restored".into()),
                head_sha: None,
                recorded_at: Some(now),
                references: vec![issue_url.to_owned()],
            });
        } else if is_finished(node.input.status) {
            node.issue_drift = Some(IssueDrift {
                issue_url: issue_url.to_owned(),
                reason: DriftReason::ChangedAfterAutoClose,
                observed_at: now,
            });
        }
        return Decision::Changed;
    }
    if node.input.status == NodeStatus::Completed && node.issue_drift.is_none() {
        node.issue_drift = Some(IssueDrift {
            issue_url: issue_url.to_owned(),
            reason: DriftReason::OpenIssue,
            observed_at: now,
        });
        return Decision::Changed;
    }
    Decision::Unchanged
}

fn status_label(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "pending",
        NodeStatus::Running => "running",
        NodeStatus::Blocked => "blocked",
        NodeStatus::Completed => "completed",
        NodeStatus::Failed => "failed",
        NodeStatus::Skipped => "skipped",
    }
}

fn closing_record(
    number: i64,
    issue_url: &str,
    reason: &str,
    closing: &ClosingEvent,
    to_status: NodeStatus,
) -> EvidenceRecord {
    let mut references = vec![issue_url.to_owned()];
    let mut head_sha = None;
    let by = match &closing.closer {
        Closer::PullRequest {
            number: pr,
            url,
            merged,
        } => {
            references.push(url.clone());
            let merged = if *merged { "merged" } else { "not merged" };
            format!("by PR #{pr} ({merged})")
        }
        Closer::Commit { oid, url } => {
            references.push(url.clone());
            head_sha = Some(oid.clone());
            format!("by commit {}", &oid[..oid.len().min(12)])
        }
        Closer::Nobody { actor: Some(actor) } => {
            format!("without a closing PR or commit by @{actor}")
        }
        Closer::Nobody { actor: None } => "without a closing PR or commit".to_owned(),
    };
    let status = status_label(to_status);
    EvidenceRecord {
        summary: format!(
            "GitHub issue #{number} closed ({reason}) {by}; node {status} by github-sync"
        ),
        command: None,
        result: Some(status.to_owned()),
        head_sha,
        recorded_at: Some(closing.at),
        references,
    }
}
