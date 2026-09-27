//! Durable, revision-fenced campaign DAGs on the canonical PostgreSQL pool.
//! This ledger records checkpoints; it does not dispatch or replay work.
use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, types::Json};

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
