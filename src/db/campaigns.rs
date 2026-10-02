//! Durable, revision-fenced campaign DAGs on the canonical PostgreSQL pool.
//! The server never writes node state; `auto_queue` lets auto-queue run ready nodes.
use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, types::Json};

use crate::services::stale_turn_reconciler::STALE_TURN_GRACE;
use crate::utils::github_links::{normalize_github_issue_url, normalize_github_repo_id};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CampaignStatus {
    Planned,
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    Blocked,
    Completed,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct EvidenceRecord {
    pub summary: String,
    pub command: Option<String>,
    pub result: Option<String>,
    pub head_sha: Option<String>,
    pub recorded_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub references: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NodeInput {
    pub id: String,
    pub title: String,
    pub status: NodeStatus,
    pub stage: String,
    #[serde(default)]
    pub group: Option<String>,
    pub round: u32,
    pub assignee: Option<String>,
    pub session_id: Option<String>,
    pub provider: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub issue_url: Option<String>,
    pub pr_url: Option<String>,
    pub head_sha: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub next_action: Option<String>,
    pub blocker: Option<String>,
    /// Plain-language one-line gist and expected benefit for the dashboard's first screen.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub benefit: Option<String>,
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub evidence_records: Vec<EvidenceRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CampaignInput {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: CampaignStatus,
    pub round: u32,
    #[serde(default)]
    pub nodes: Vec<NodeInput>,
    /// Hand ready nodes to auto-queue on every campaign event; omitted keeps the stored value.
    #[serde(default)]
    pub auto_queue: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Node {
    #[serde(flatten)]
    pub input: NodeInput,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Campaign {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: CampaignStatus,
    pub round: u32,
    pub revision: i64,
    #[serde(default)]
    pub auto_queue: bool,
    pub nodes: Vec<Node>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum CampaignError {
    #[error("{0}")]
    Validation(String),
    #[error("campaign not found")]
    NotFound,
    #[error("campaign revision conflict; reload before retrying")]
    Conflict,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

fn invalid(message: impl Into<String>) -> CampaignError {
    CampaignError::Validation(message.into())
}

pub fn validate_id(id: &str) -> Result<(), CampaignError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(invalid(
            "id must be 1..128 ASCII letters, digits, '-', '_' or '.'",
        ));
    }
    Ok(())
}

pub fn validate(input: &CampaignInput) -> Result<(), CampaignError> {
    if input.title.trim().is_empty() || input.title.len() > 512 || input.round == 0 {
        return Err(invalid(
            "title must be 1..512 bytes and round must be positive",
        ));
    }
    if input.nodes.len() > 1000 || input.description.len() > 64_000 {
        return Err(invalid(
            "campaign exceeds 1000 nodes or 64000 description bytes",
        ));
    }
    let mut ids = HashMap::new();
    for (index, node) in input.nodes.iter().enumerate() {
        validate_id(&node.id)?;
        if ids.insert(node.id.as_str(), index).is_some() {
            return Err(invalid(format!("duplicate node id: {}", node.id)));
        }
        if node.title.trim().is_empty()
            || node.title.len() > 512
            || node.stage.trim().is_empty()
            || node.stage.len() > 128
            || node.round == 0
        {
            return Err(invalid(format!(
                "node {} requires title, stage and positive round",
                node.id
            )));
        }
    }
    let mut incoming = vec![0usize; input.nodes.len()];
    let mut dependents = vec![Vec::new(); input.nodes.len()];
    for (index, node) in input.nodes.iter().enumerate() {
        let mut seen = HashSet::new();
        for dependency in &node.dependencies {
            let Some(&dependency_index) = ids.get(dependency.as_str()) else {
                return Err(invalid(format!(
                    "node {} has missing dependency {dependency}",
                    node.id
                )));
            };
            if !seen.insert(dependency) {
                return Err(invalid(format!(
                    "node {} repeats dependency {dependency}",
                    node.id
                )));
            }
            incoming[index] += 1;
            dependents[dependency_index].push(index);
        }
    }
    let mut ready: VecDeque<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(i, &count)| (count == 0).then_some(i))
        .collect();
    let mut visited = 0;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &next in &dependents[index] {
            incoming[next] -= 1;
            if incoming[next] == 0 {
                ready.push_back(next);
            }
        }
    }
    if visited != input.nodes.len() {
        return Err(invalid("campaign dependencies contain a cycle"));
    }
    if input.status == CampaignStatus::Completed
        && (input.nodes.is_empty()
            || input
                .nodes
                .iter()
                .any(|n| !matches!(n.status, NodeStatus::Completed | NodeStatus::Skipped)))
    {
        return Err(invalid(
            "completed campaign requires all nodes completed or skipped",
        ));
    }
    Ok(())
}

fn checkpoint(id: String, mut input: CampaignInput, previous: Option<&Campaign>) -> Campaign {
    for node in &mut input.nodes {
        node.group = node
            .group
            .as_deref()
            .map(str::trim)
            .filter(|group| !group.is_empty())
            .map(str::to_owned);
    }
    let now = Utc::now();
    let previous_nodes: HashMap<_, _> = previous
        .into_iter()
        .flat_map(|p| &p.nodes)
        .map(|node| (node.input.id.as_str(), node))
        .collect();
    Campaign {
        id,
        title: input.title,
        description: input.description,
        status: input.status,
        round: input.round,
        revision: previous.map_or(1, |p| p.revision + 1),
        auto_queue: input
            .auto_queue
            .unwrap_or_else(|| previous.is_some_and(|p| p.auto_queue)),
        nodes: input
            .nodes
            .into_iter()
            .map(|input| {
                let updated_at = previous_nodes
                    .get(input.id.as_str())
                    .filter(|old| old.input == input)
                    .map_or(now, |old| old.updated_at);
                Node { input, updated_at }
            })
            .collect(),
        created_at: previous.map_or(now, |p| p.created_at),
        updated_at: now,
    }
}

pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<Campaign>, CampaignError> {
    let rows: Vec<Json<Campaign>> = sqlx::query_scalar(
        "SELECT document FROM campaigns ORDER BY updated_at DESC, id LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}

pub async fn get(pool: &PgPool, id: &str) -> Result<Campaign, CampaignError> {
    sqlx::query_scalar::<_, Json<Campaign>>("SELECT document FROM campaigns WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(|v| v.0)
        .ok_or(CampaignError::NotFound)
}

/// What the card behind a node's `issue_url` is doing now, read per request and never
/// stored. A dispatch row can outlive its session, so only `running` claims live work.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, sqlx::FromRow)]
pub struct NodeLiveStatus {
    pub card_id: String,
    pub card_status: String,
    /// Latest dispatch and its session, independently of any older work still running.
    pub dispatch_id: Option<String>,
    pub dispatch_type: Option<String>,
    pub dispatch_status: Option<String>,
    /// The session holding that dispatch, if any, and its last heartbeat.
    pub session_status: Option<String>,
    pub session_seen_at: Option<DateTime<Utc>>,
    /// A dispatched task has a mid-turn session inside the stale-turn grace window.
    pub running: bool,
    /// A currently working dispatch/session pair; newer sidecars cannot hide it.
    pub working_dispatch_id: Option<String>,
    pub working_dispatch_type: Option<String>,
    pub working_session_id: Option<String>,
    pub working_session_status: Option<String>,
    pub working_session_seen_at: Option<DateTime<Utc>>,
    pub queue_status: Option<String>,
}

#[derive(sqlx::FromRow)]
struct LiveRow {
    repo_id: String,
    issue_number: i64,
    #[sqlx(flatten)]
    status: NodeLiveStatus,
}

/// `(owner/repo, issue number)` for a GitHub issue link.
pub(crate) fn issue_ref(raw: &str) -> Option<(String, i64)> {
    let url = normalize_github_issue_url(raw)?;
    let number = url.rsplit('/').next()?.parse().ok()?;
    Some((normalize_github_repo_id(&url)?, number))
}

/// Campaign id -> node id -> live status. Nodes without a GitHub issue link, or
/// whose issue has no card, are absent.
pub async fn live_status(
    pool: &PgPool,
    campaigns: &[Campaign],
) -> Result<HashMap<String, HashMap<String, NodeLiveStatus>>, CampaignError> {
    let mut nodes_by_issue: HashMap<(String, i64), Vec<(&str, &str)>> = HashMap::new();
    for campaign in campaigns {
        for node in &campaign.nodes {
            if let Some((repo, number)) = node.input.issue_url.as_deref().and_then(issue_ref) {
                nodes_by_issue
                    .entry((repo.to_lowercase(), number))
                    .or_default()
                    .push((&campaign.id, &node.input.id));
            }
        }
    }
    let mut live: HashMap<String, HashMap<String, NodeLiveStatus>> = HashMap::new();
    if nodes_by_issue.is_empty() {
        return Ok(live);
    }
    let (repos, numbers): (Vec<String>, Vec<i64>) = nodes_by_issue.keys().cloned().unzip();
    let rows: Vec<LiveRow> = sqlx::query_as(
        "SELECT k.repo_id, k.issue_number, c.id AS card_id,
                COALESCE(c.status, 'backlog') AS card_status,
                d.id AS dispatch_id, d.dispatch_type, d.status AS dispatch_status,
                s.status AS session_status, s.last_heartbeat AS session_seen_at,
                w.dispatch_id IS NOT NULL AS running,
                w.dispatch_id AS working_dispatch_id, w.dispatch_type AS working_dispatch_type,
                w.session_id AS working_session_id, w.session_status AS working_session_status,
                w.session_seen_at AS working_session_seen_at,
                q.status AS queue_status
         FROM UNNEST($1::TEXT[], $2::BIGINT[]) AS k(repo_id, issue_number)
         JOIN kanban_cards c
           ON LOWER(c.repo_id) = k.repo_id AND c.github_issue_number = k.issue_number
         LEFT JOIN LATERAL (
             SELECT id, dispatch_type, status FROM task_dispatches
             WHERE kanban_card_id = c.id ORDER BY created_at DESC, id DESC LIMIT 1
         ) d ON TRUE
         LEFT JOIN LATERAL (
             SELECT status, last_heartbeat FROM sessions
             WHERE active_dispatch_id = d.id ORDER BY last_heartbeat DESC NULLS LAST LIMIT 1
         ) s ON TRUE
         LEFT JOIN LATERAL (
             SELECT wd.id AS dispatch_id, wd.dispatch_type, ws.id::TEXT AS session_id,
                    ws.status AS session_status, ws.last_heartbeat AS session_seen_at
             FROM task_dispatches wd
             JOIN sessions ws ON ws.active_dispatch_id = wd.id
             WHERE wd.kanban_card_id = c.id AND wd.status = 'dispatched'
               AND ws.status IN ('turn_active', 'awaiting_bg')
               AND ws.last_heartbeat >= NOW() - ($3::BIGINT * INTERVAL '1 second')
             ORDER BY ws.last_heartbeat DESC, wd.created_at DESC, wd.id DESC, ws.id DESC
             LIMIT 1
         ) w ON TRUE
         LEFT JOIN LATERAL (
             SELECT status FROM auto_queue_entries
             WHERE kanban_card_id = c.id ORDER BY created_at DESC, id DESC LIMIT 1
         ) q ON TRUE",
    )
    .bind(&repos)
    .bind(&numbers)
    .bind(STALE_TURN_GRACE.as_secs() as i64)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let key = (row.repo_id, row.issue_number);
        for (campaign_id, node_id) in nodes_by_issue.get(&key).into_iter().flatten() {
            live.entry((*campaign_id).to_owned())
                .or_default()
                .insert((*node_id).to_owned(), row.status.clone());
        }
    }
    Ok(live)
}

/// Active campaigns that asked to hand their ready nodes to auto-queue.
pub async fn list_auto_queue_active(pool: &PgPool) -> Result<Vec<Campaign>, CampaignError> {
    let rows: Vec<Json<Campaign>> = sqlx::query_scalar(
        "SELECT document FROM campaigns WHERE document->>'status' = 'active' \
         AND document->>'auto_queue' = 'true' ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}

/// Every revision snapshots the whole DAG, so long campaigns keep only the newest ones.
pub const REVISION_RETENTION: i64 = 10;

/// A retention below one empties the keep-set, so the prune would delete the
/// revision its own transaction just inserted. Refuse to build such a binary.
const _: () = assert!(REVISION_RETENTION > 0);

pub async fn history(pool: &PgPool, id: &str) -> Result<Vec<Campaign>, CampaignError> {
    get(pool, id).await?;
    let rows: Vec<Json<Campaign>> = sqlx::query_scalar(
        "SELECT document FROM campaign_revisions WHERE campaign_id = $1 ORDER BY revision DESC LIMIT $2")
        .bind(id).bind(REVISION_RETENTION).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}

/// Drops every snapshot outside the newest `REVISION_RETENTION` by rank, so a gap in
/// revision numbers cannot widen the deletion.
async fn prune_revisions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: &str,
) -> Result<u64, CampaignError> {
    Ok(sqlx::query(
        "DELETE FROM campaign_revisions WHERE campaign_id = $1 AND revision NOT IN \
         (SELECT revision FROM campaign_revisions WHERE campaign_id = $1 \
          ORDER BY revision DESC LIMIT $2)",
    )
    .bind(id)
    .bind(REVISION_RETENTION)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

pub async fn create(
    pool: &PgPool,
    id: String,
    input: CampaignInput,
) -> Result<Campaign, CampaignError> {
    validate_id(&id)?;
    validate(&input)?;
    let campaign = checkpoint(id, input, None);
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO campaigns (id, revision, document, updated_at) VALUES ($1, 1, $2, $3) ON CONFLICT (id) DO NOTHING")
        .bind(&campaign.id).bind(Json(&campaign)).bind(campaign.updated_at)
        .execute(&mut *tx).await?.rows_affected();
    if inserted == 0 {
        return Err(CampaignError::Conflict);
    }
    sqlx::query(
        "INSERT INTO campaign_revisions (campaign_id, revision, document) VALUES ($1, 1, $2)",
    )
    .bind(&campaign.id)
    .bind(Json(&campaign))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(campaign)
}

pub async fn replace(
    pool: &PgPool,
    id: &str,
    expected_revision: i64,
    input: CampaignInput,
) -> Result<Campaign, CampaignError> {
    validate(&input)?;
    if expected_revision < 1 || expected_revision == i64::MAX {
        return Err(invalid(
            "expected_revision must be a positive incrementable integer",
        ));
    }
    let mut tx = pool.begin().await?;
    let previous = sqlx::query_scalar::<_, Json<Campaign>>(
        "SELECT document FROM campaigns WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(CampaignError::NotFound)?
    .0;
    if previous.revision != expected_revision {
        return Err(CampaignError::Conflict);
    }
    let campaign = checkpoint(id.to_owned(), input, Some(&previous));
    let affected = sqlx::query(
        "UPDATE campaigns SET revision = $2, document = $3, updated_at = $4 WHERE id = $1 AND revision = $5")
        .bind(id).bind(campaign.revision).bind(Json(&campaign)).bind(campaign.updated_at)
        .bind(expected_revision).execute(&mut *tx).await?.rows_affected();
    if affected != 1 {
        return Err(CampaignError::Conflict);
    }
    sqlx::query(
        "INSERT INTO campaign_revisions (campaign_id, revision, document) VALUES ($1, $2, $3)",
    )
    .bind(id)
    .bind(campaign.revision)
    .bind(Json(&campaign))
    .execute(&mut *tx)
    .await?;
    prune_revisions(&mut tx, id).await?;
    tx.commit().await?;
    Ok(campaign)
}

#[cfg(test)]
mod tests;
