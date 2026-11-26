use std::collections::BTreeMap;

use super::*;
use crate::domain::campaign::{decision_item::ApprovalBinding, definition::ValidatedDefinition};

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionRevision {
    pub revision: u64,
    pub definition: ValidatedDefinition,
    pub change: Option<ChangeReason>,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub sequence: u64,
    pub run: Run,
    pub nodes: BTreeMap<String, NodeRuntime>,
    pub definition_pin: ValidatedDefinition,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt: Attempt,
    pub definition_pin: ValidatedDefinition,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub binding: AttemptBinding,
    pub kind: String,
    pub source: String,
    pub document: serde_json::Value,
    pub recorded_at: DateTime<Utc>,
}

// One joined statement gives a coherent projection even during command commits.
const PROJECTION_BY_ID: &str = "SELECT d.*, r.document AS run_document FROM server_campaign_definitions d \
    LEFT JOIN server_campaign_runs r ON r.campaign_id = d.campaign_id AND r.run_id = d.run_id \
    WHERE d.campaign_id = $1";
const PROJECTION_PAGE: &str = "SELECT d.*, r.document AS run_document FROM server_campaign_definitions d \
    LEFT JOIN server_campaign_runs r ON r.campaign_id = d.campaign_id AND r.run_id = d.run_id \
    ORDER BY d.campaign_id LIMIT $1 OFFSET $2";

#[derive(sqlx::FromRow)]
struct Projection {
    campaign_id: String,
    revision: i64,
    definition: Json<ValidatedDefinition>,
    control: Json<CampaignControl>,
    run_document: Option<Json<Run>>,
    nodes: Json<BTreeMap<String, NodeRuntime>>,
    event_sequence: i64,
    last_event_at: Option<DateTime<Utc>>,
}

fn decode(row: Projection) -> StoreResult<CampaignState> {
    Ok(CampaignState {
        id: row.campaign_id,
        definition_revision: u64::try_from(row.revision)
            .map_err(|_| StoreError::InconsistentState)?,
        definition: row.definition.0,
        control: row.control.0,
        run: row.run_document.map(|v| v.0),
        nodes: row.nodes.0,
        event_sequence: u64::try_from(row.event_sequence)
            .map_err(|_| StoreError::InconsistentState)?,
        last_event_at: row.last_event_at,
    })
}

pub async fn get(pool: &PgPool, id: &str) -> StoreResult<Option<CampaignState>> {
    sqlx::query_as::<_, Projection>(PROJECTION_BY_ID)
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(decode)
        .transpose()
}

pub(super) async fn get_in(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> StoreResult<Option<CampaignState>> {
    sqlx::query_as::<_, Projection>(PROJECTION_BY_ID)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .map(decode)
        .transpose()
}

// Host initiation and command decisions serialize on the same campaign row. The caller
// keeps this transaction until it finishes using the snapshot at the host gate.
pub async fn get_for_worker(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
) -> StoreResult<Option<CampaignState>> {
    let key = sqlx::query(
        "SELECT campaign_id FROM server_campaign_keys WHERE campaign_id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if key.is_none() {
        return Ok(None);
    }
    get_in(tx, id).await
}

pub async fn list(pool: &PgPool, limit: i64, offset: i64) -> StoreResult<Vec<CampaignState>> {
    sqlx::query_as::<_, Projection>(PROJECTION_PAGE)
        .bind(limit.clamp(1, 100))
        .bind(offset.max(0))
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(decode)
        .collect()
}

#[cfg(test)]
pub async fn revisions(pool: &PgPool, id: &str) -> StoreResult<Vec<DefinitionRevision>> {
    let rows = sqlx::query_as::<_, (i64, Json<ValidatedDefinition>, Option<Json<ChangeReason>>)>(
        "SELECT revision, definition, change FROM server_campaign_revisions WHERE campaign_id = $1 ORDER BY revision DESC LIMIT 10",
    ).bind(id).fetch_all(pool).await?;
    rows.into_iter()
        .map(|(revision, Json(definition), change)| {
            Ok(DefinitionRevision {
                revision: u64::try_from(revision).map_err(|_| StoreError::InconsistentState)?,
                definition,
                change: change.map(|v| v.0),
            })
        })
        .collect()
}

#[derive(Serialize)]
pub struct ApprovalOpeningRequest<'a> {
    pub campaign_id: &'a str,
    pub event_sequence: u64,
    pub binding: &'a ApprovalBinding,
}

pub(crate) type ApprovalOpeningTimes = BTreeMap<String, BTreeMap<String, DateTime<Utc>>>;

pub async fn approval_opening_times(
    pool: &PgPool,
    requests: &[ApprovalOpeningRequest<'_>],
) -> StoreResult<ApprovalOpeningTimes> {
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    // The projection preceded this query. The event sequence pins each lookup to that snapshot.
    let openings: Vec<(String, String, Json<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT snapshot.campaign_id, snapshot.binding->>'node_id', opened.document->'observed_at'
         FROM jsonb_to_recordset($1) AS snapshot(campaign_id text, event_sequence bigint, binding jsonb)
         CROSS JOIN LATERAL (
             SELECT document FROM server_campaign_events
             WHERE campaign_id = snapshot.campaign_id
                 AND run_id = snapshot.binding->>'run_id'
                 AND sequence <= snapshot.event_sequence
                 AND document #>> '{kind,event}' = 'approval_gate_opened'
                 AND document #> '{kind,binding}' = snapshot.binding
             ORDER BY sequence DESC LIMIT 1
         ) AS opened",
    )
    .bind(Json(requests))
    .fetch_all(pool)
    .await?;
    let mut times = ApprovalOpeningTimes::new();
    for (campaign_id, node_id, Json(at)) in openings {
        times.entry(campaign_id).or_default().insert(node_id, at);
    }
    Ok(times)
}

pub async fn events(
    pool: &PgPool,
    id: &str,
    run: Option<&str>,
    after: i64,
    limit: i64,
) -> StoreResult<Vec<Event>> {
    let rows: Vec<Json<Event>> = sqlx::query_scalar(
        "SELECT document FROM server_campaign_events WHERE campaign_id = $1 AND sequence > $2 \
         AND ($3::text IS NULL OR run_id = $3) ORDER BY sequence LIMIT $4",
    )
    .bind(id)
    .bind(after.max(0))
    .bind(run)
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}

#[cfg(test)]
pub async fn run_history(
    pool: &PgPool,
    id: &str,
    run: &str,
    after: i64,
    limit: i64,
) -> StoreResult<Vec<RunSnapshot>> {
    let rows = sqlx::query_as::<
        _,
        (
            i64,
            Json<Run>,
            Json<BTreeMap<String, NodeRuntime>>,
            Json<ValidatedDefinition>,
        ),
    >(
        "SELECT sequence, document, nodes, definition_pin FROM server_campaign_run_history \
         WHERE campaign_id = $1 AND run_id = $2 AND sequence > $3 ORDER BY sequence LIMIT $4",
    )
    .bind(id)
    .bind(run)
    .bind(after.max(0))
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(sequence, Json(run), Json(nodes), Json(definition_pin))| {
            Ok(RunSnapshot {
                sequence: u64::try_from(sequence).map_err(|_| StoreError::InconsistentState)?,
                run,
                nodes,
                definition_pin,
            })
        })
        .collect()
}

#[cfg(test)]
pub async fn attempts(pool: &PgPool, id: &str, run: &str) -> StoreResult<Vec<AttemptRecord>> {
    let rows = sqlx::query_as::<_, (Json<Attempt>, Json<ValidatedDefinition>)>(
        "SELECT document, definition_pin FROM server_campaign_attempts WHERE campaign_id = $1 AND run_id = $2 ORDER BY attempt_id",
    ).bind(id).bind(run).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(Json(attempt), Json(definition_pin))| AttemptRecord {
            attempt,
            definition_pin,
        })
        .collect())
}

#[cfg(test)]
pub async fn action_history(
    pool: &PgPool,
    id: &str,
    run: &str,
    action: &str,
    after: i64,
    limit: i64,
) -> StoreResult<Vec<Action>> {
    let rows: Vec<Json<Action>> = sqlx::query_scalar(
        "SELECT document FROM server_campaign_action_history WHERE campaign_id = $1 AND run_id = $2 \
         AND action_id = $3 AND sequence > $4 ORDER BY sequence LIMIT $5",
    ).bind(id).bind(run).bind(action).bind(after.max(0)).bind(limit.clamp(1, 500)).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}

#[cfg(test)]
pub async fn evidence(pool: &PgPool, id: &str, run: &str) -> StoreResult<Vec<EvidenceRecord>> {
    let rows: Vec<Json<EvidenceRecord>> = sqlx::query_scalar(
        "SELECT document FROM server_campaign_evidence WHERE campaign_id = $1 AND run_id = $2 ORDER BY evidence_id",
    ).bind(id).bind(run).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|v| v.0).collect())
}
