//! Exact pre-effects input stays in its original row while migration output adoption waits.

use super::IntakeOutboxRow;
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use crate::services::cluster::intake_worker::BeforeEffects;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use std::path::PathBuf;

const PREFIX: &str = "claude-migration-wait:v1:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Authority {
    pub provider: String,
    pub token_hash: String,
    pub channel_id: u64,
    pub local_instance: String,
    pub runtime_root: PathBuf,
    pub logical_key: String,
    pub session_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerEpoch {
    owner_instance_id: String,
    generation: i64,
    status: String,
    active_count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectorRevision {
    canonical_row: i64,
    session_key: String,
    hosted_raw: Option<Value>,
    cached_sid: Option<String>,
    raw_sid: Option<String>,
    cwd: Option<String>,
    recorded_at: DateTime<Utc>,
    clear_generation: i64,
    owner_epoch: Option<OwnerEpoch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitIntent {
    pub authority: Authority,
    selector: SelectorRevision,
    binding_seq: u64,
    intent_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum WaitOwner {
    Stamped { instance: String, generation: i64 },
    LegacyLocal { instance: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParkOutcome {
    Parked,
    AlreadyParked,
    Reclassified,
    NeedsReclassification(Box<Reclassification>),
    Blocked(String),
    Unknown(String),
}

pub(crate) struct Reclassification {
    proof: BeforeEffects,
    intent: WaitIntent,
    created_at: DateTime<Utc>,
}
impl std::fmt::Debug for Reclassification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reclassification")
            .field("row_id", &self.proof.row().id)
            .finish_non_exhaustive()
    }
}
impl PartialEq for Reclassification {
    fn eq(&self, other: &Self) -> bool {
        self.intent == other.intent
            && self.proof.row().id == other.proof.row().id
            && self.proof.row().claim_owner == other.proof.row().claim_owner
            && self.created_at == other.created_at
    }
}
impl Eq for Reclassification {}

pub(crate) async fn reclassify_before_effects(
    pool: &PgPool,
    request: Box<Reclassification>,
) -> Result<ParkOutcome, String> {
    let Reclassification {
        proof,
        mut intent,
        created_at,
    } = *request;
    let expected = proof.row();
    let mut tx = owner_gate(pool, &intent.authority).await?;
    intent.selector = revision(&mut tx, &intent.authority, intent.selector.canonical_row).await?;
    let s = sqlx::query_as::<_, Snapshot>("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
        .bind(expected.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let Some(s) = s else {
        return Ok(ParkOutcome::Blocked("original row missing".into()));
    };
    let checked = async {
        owner(&s.row, &intent)?;
        if s.row.status != super::IntakeOutboxStatus::Spawned
            || s.row.claim_owner != expected.claim_owner
            || s.row.owner_instance_id != expected.owner_instance_id
            || s.row.owner_generation != expected.owner_generation
        {
            return Err(BlockReason::Owner);
        }
        if super::consent_handoff::payload(&s.row).map_err(|_| BlockReason::Payload)?
            != super::consent_handoff::payload(expected).map_err(|_| BlockReason::Payload)?
        {
            return Err(BlockReason::Payload);
        }
        if s.accepted_at.is_none()
            || s.spawned_at.is_none()
            || s.dispatched_at.is_some()
            || s.completed_at.is_some()
            || s.created_at != created_at
        {
            return Err(BlockReason::Clock);
        }
        let receipts = safety(&mut tx, &s).await?;
        if let Some(error) = s.last_error.as_deref() {
            let encoded = error.strip_prefix(PREFIX).ok_or(BlockReason::Marker)?;
            let prior: Marker = serde_json::from_str(encoded).map_err(|_| BlockReason::Marker)?;
            if prior.intent.authority != intent.authority {
                return Err(BlockReason::Marker);
            }
            marker(&s, &prior.intent, &receipts)?;
        }
        Ok(())
    }
    .await;
    if let Err(reason) = checked {
        return Ok(ParkOutcome::Blocked(format!(
            "migration reclassification blocked: {reason:?}"
        )));
    }
    let changed = sqlx::query("UPDATE intake_outbox SET status='pending',claim_owner=NULL,claimed_at=NULL,
        accepted_at=NULL,spawned_at=NULL,last_error=NULL WHERE id=$1 AND status='spawned' AND claim_owner=$2
        AND last_error IS NOT DISTINCT FROM $3")
        .bind(s.row.id).bind(&s.row.claim_owner).bind(&s.last_error)
        .execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if changed != 1 {
        return Ok(ParkOutcome::Blocked(
            "migration reclassification CAS lost".into(),
        ));
    }
    let committed = tx.commit().await;
    Ok(
        if committed.is_ok() && !LOSE_COMMIT_ACK.try_with(|lost| *lost).unwrap_or(false) {
            ParkOutcome::Reclassified
        } else {
            ParkOutcome::Unknown("migration reclassification commit was not acknowledged".into())
        },
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaitingRef {
    pub row_id: i64,
    pub source_ids: Vec<String>,
    pub payload_digest: String,
    pub owner: WaitOwner,
    pub created_at: DateTime<Utc>,
    pub attempt_no: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BlockReason {
    Owner,
    State,
    Clock,
    Marker,
    Family,
    Replay,
    Home,
    Payload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Blocker {
    pub row_id: i64,
    pub reason: BlockReason,
}

pub(crate) struct VerifiedWaitingSet(Vec<WaitingRef>);
impl VerifiedWaitingSet {
    pub(crate) fn requests(&self) -> &[WaitingRef] {
        &self.0
    }
}

/// This guards only the DB subset; mailbox and output authorities remain separate.
pub(crate) struct FencedWaiting {
    transaction: Transaction<'static, Postgres>,
    intent: WaitIntent,
    waiting: VerifiedWaitingSet,
    blockers: Vec<Blocker>,
    foreign_sessions: i64,
    queued_bodies: i64,
    replay_registrations: Vec<i64>,
}
impl FencedWaiting {
    pub(crate) fn intent(&self) -> &WaitIntent {
        &self.intent
    }
    pub(crate) fn waiting(&self) -> &VerifiedWaitingSet {
        &self.waiting
    }
    pub(crate) fn blockers(&self) -> &[Blocker] {
        &self.blockers
    }
    pub(crate) fn foreign_sessions(&self) -> i64 {
        self.foreign_sessions
    }
    pub(crate) fn queued_bodies(&self) -> i64 {
        self.queued_bodies
    }
    pub(crate) fn replay_registrations(&self) -> &[i64] {
        &self.replay_registrations
    }
    pub(crate) async fn release(self) -> Result<(), String> {
        self.transaction.rollback().await.map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    row: i64,
    created_at: DateTime<Utc>,
    payload: String,
    claim: String,
    owner: WaitOwner,
    intent: WaitIntent,
    receipts: Value,
}

#[derive(sqlx::FromRow)]
struct Snapshot {
    #[sqlx(flatten)]
    row: IntakeOutboxRow,
    claimed_at: Option<DateTime<Utc>>,
    accepted_at: Option<DateTime<Utc>>,
    spawned_at: Option<DateTime<Utc>>,
    dispatched_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    home_epoch: Option<i64>,
    replay_only: bool,
    replay_disposition: Option<String>,
    replay_request_key: Option<String>,
    replay_request_hash: Option<String>,
    replay_episode_nonce: Option<String>,
    replay_owner_incarnation: Option<String>,
    replay_hold_reason: Option<String>,
    replay_preserved: Option<Value>,
    created_at: DateTime<Utc>,
}

async fn owner_gate(
    pool: &PgPool,
    a: &Authority,
) -> Result<Transaction<'static, Postgres>, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let owner = OwnerIdentity::new(&a.provider, &a.channel_id.to_string());
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(owner.advisory_key())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    Ok(tx)
}

async fn revision(
    tx: &mut Transaction<'_, Postgres>,
    a: &Authority,
    id: i64,
) -> Result<SelectorRevision, String> {
    use sqlx::Row;
    if a.provider != "claude"
        || a.channel_id == 0
        || a.local_instance.trim().is_empty()
        || a.token_hash.trim().is_empty()
        || a.logical_key.trim().is_empty()
        || crate::config::runtime_root().and_then(|p| p.canonicalize().ok())
            != a.runtime_root.canonicalize().ok()
        || a.runtime_root.canonicalize().is_err()
    {
        return Err("migration authority/root is incomplete".into());
    }
    let r = sqlx::query("SELECT s.*, COALESCE(b.clear_generation,0) AS migration_clear_generation,
        COALESCE(s.claude_session_id_recorded_at,s.created_at) AS migration_recorded_at
        FROM sessions s LEFT JOIN channel_session_clear_boundaries b ON b.channel_id=s.channel_id WHERE s.id=$1")
        .bind(id).fetch_optional(&mut **tx).await.map_err(|e| e.to_string())?.ok_or("canonical session is missing")?;
    let key: String = r.try_get("session_key").map_err(|e| e.to_string())?;
    let logical = crate::services::discord::session_identity::tmux_name_from_session_key(&key);
    let string = |field| {
        r.try_get::<Option<String>, _>(field)
            .map_err(|e| e.to_string())
    };
    if key != a.session_key
        || logical.as_deref() != Some(a.logical_key.as_str())
        || string("provider")?.as_deref() != Some(a.provider.as_str())
        || string("identity_kind")?.as_deref() != Some("discord_channel")
        || string("discord_token_hash")?.as_deref() != Some(a.token_hash.as_str())
        || string("channel_id")?.as_deref() != Some(a.channel_id.to_string().as_str())
        || string("instance_id")?.as_deref() != Some(a.local_instance.as_str())
    {
        return Err("canonical session authority changed".into());
    }
    let hosted_raw: Option<Value> = r.try_get("hosted_execution").map_err(|e| e.to_string())?;
    if hosted_raw.is_some() {
        return Err("migration wait requires a Legacy session".into());
    }
    let owner_epoch = sqlx::query_as::<_, OwnerEpoch>(
        "SELECT owner_instance_id,generation,status,COUNT(*) FILTER(WHERE status='active') OVER() AS active_count FROM intake_session_owners
        WHERE provider=$1 AND raw_channel_id=$2 ORDER BY generation DESC LIMIT 1",
    )
    .bind(&a.provider)
    .bind(a.channel_id.to_string())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| e.to_string())?;
    if owner_epoch.as_ref().is_some_and(|o| {
        o.owner_instance_id != a.local_instance
            || o.status != "active"
            || o.generation < 0
            || o.active_count != 1
    }) {
        return Err("canonical intake owner is not active local".into());
    }
    Ok(SelectorRevision {
        canonical_row: id,
        session_key: key,
        hosted_raw,
        cached_sid: string("claude_session_id")?,
        raw_sid: string("raw_provider_session_id")?,
        cwd: string("cwd")?,
        recorded_at: r
            .try_get("migration_recorded_at")
            .map_err(|e| e.to_string())?,
        clear_generation: r
            .try_get("migration_clear_generation")
            .map_err(|e| e.to_string())?,
        owner_epoch,
    })
}

pub(crate) async fn capture_intent(
    pool: &PgPool,
    authority: &Authority,
    canonical_row: i64,
    binding_seq: u64,
    intent_id: &str,
) -> Result<WaitIntent, String> {
    if binding_seq == 0 || intent_id.trim().is_empty() {
        return Err("migration intent is incomplete".into());
    }
    let mut tx = owner_gate(pool, authority).await?;
    let selector = revision(&mut tx, authority, canonical_row).await?;
    tx.rollback().await.map_err(|e| e.to_string())?;
    Ok(WaitIntent {
        authority: authority.clone(),
        selector,
        binding_seq,
        intent_id: intent_id.into(),
    })
}

fn owner(row: &IntakeOutboxRow, intent: &WaitIntent) -> Result<WaitOwner, BlockReason> {
    let a = &intent.authority;
    if row.provider != a.provider
        || row.channel_id != a.channel_id.to_string()
        || row.target_instance_id != a.local_instance
    {
        return Err(BlockReason::Owner);
    }
    match (&row.owner_instance_id, row.owner_generation) {
        (None, None)
            if intent.selector.owner_epoch.as_ref().is_none_or(|e| {
                e.status == "active" && e.owner_instance_id == a.local_instance
            }) =>
        {
            Ok(WaitOwner::LegacyLocal {
                instance: a.local_instance.clone(),
            })
        }
        (Some(instance), Some(generation))
            if row.admission_kind == "forwarded"
                && intent.selector.owner_epoch.as_ref().is_some_and(|e| {
                    e.owner_instance_id == *instance
                        && e.generation == generation
                        && e.status == "active"
                }) =>
        {
            Ok(WaitOwner::Stamped {
                instance: instance.clone(),
                generation,
            })
        }
        _ => Err(BlockReason::Owner),
    }
}

fn sources(row: &IntakeOutboxRow) -> Result<Vec<String>, BlockReason> {
    let ids = row
        .replay_source_message_ids
        .clone()
        .unwrap_or_else(|| vec![row.user_msg_id.clone()]);
    if !ids.contains(&row.user_msg_id)
        || ids.iter().any(|s| {
            s.parse::<u64>()
                .ok()
                .is_none_or(|id| id == 0 || id.to_string() != *s)
        })
    {
        return Err(BlockReason::Payload);
    }
    Ok(ids)
}

fn marker(
    s: &Snapshot,
    intent: &WaitIntent,
    receipts: &Value,
) -> Result<Option<Marker>, BlockReason> {
    let Some(error) = s.last_error.as_deref() else {
        return Ok(None);
    };
    let encoded = error.strip_prefix(PREFIX).ok_or(BlockReason::Marker)?;
    let m: Marker = serde_json::from_str(encoded).map_err(|_| BlockReason::Marker)?;
    if m.row != s.row.id
        || m.created_at != s.created_at
        || m.payload != super::consent_handoff::payload(&s.row).map_err(|_| BlockReason::Payload)?
        || m.intent != *intent
        || m.owner != owner(&s.row, intent)?
        || m.claim.is_empty()
        || m.receipts != *receipts
    {
        return Err(BlockReason::Marker);
    }
    Ok(Some(m))
}

async fn registered(
    tx: &mut Transaction<'_, Postgres>,
    s: &Snapshot,
) -> Result<Value, BlockReason> {
    use crate::db::replay_disposition::write::{CanonicalInput, ensure_same_request_on_connection};
    let ids = sources(&s.row)?;
    let user = s
        .row
        .user_msg_id
        .parse()
        .map_err(|_| BlockReason::Payload)?;
    let channel = s.row.channel_id.parse().map_err(|_| BlockReason::Payload)?;
    let parsed = ids
        .iter()
        .map(|id| id.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| BlockReason::Payload)?;
    let mut input =
        CanonicalInput::discord(&s.row.provider, channel, user, parsed, &s.row.user_text);
    input.owner_id = s.row.request_owner_id.clone();
    input.agent_id = s.row.agent_id.clone();
    input.attachments = s.row.attachment_refs.clone();
    input.reply_context = s.row.reply_context.clone();
    if s.replay_disposition.is_none()
        && (s.replay_request_key.is_some()
            || s.replay_request_hash.is_some()
            || s.replay_episode_nonce.is_some()
            || s.replay_owner_incarnation.is_some()
            || s.replay_preserved.is_some()
            || s.replay_hold_reason.is_some())
    {
        return Err(BlockReason::Replay);
    }
    let related = sqlx::query_as::<_, Snapshot>(
        "SELECT * FROM intake_outbox WHERE (replay_only OR replay_disposition IS NOT NULL OR replay_request_key IS NOT NULL)
        AND (replay_request_key=$1 OR (lower(btrim(provider))=$2 AND channel_id=$3
        AND (replay_source_message_ids && $4::TEXT[] OR user_msg_id=ANY($4::TEXT[])))) ORDER BY id",
    )
    .bind(&input.request_key)
    .bind(&input.provider)
    .bind(&input.channel)
    .bind(&input.sources)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_| BlockReason::Replay)?;
    let mut receipts = Vec::new();
    for r in related {
        let preserved = r.replay_preserved.as_ref().and_then(Value::as_object);
        if r.replay_disposition.as_deref() != Some("registered_not_started")
            || !r.replay_only
            || r.row.status != super::IntakeOutboxStatus::Unknown
            || r.row.user_text != s.row.user_text
            || r.row.owner_instance_id.is_some()
            || r.row.owner_generation.is_some()
            || r.replay_hold_reason.is_some()
            || r.home_epoch.is_some()
            || r.claimed_at.is_some()
            || r.accepted_at.is_some()
            || r.spawned_at.is_some()
            || r.dispatched_at.is_some()
            || r.completed_at.is_some()
            || r.row.claim_owner.is_some()
            || r.last_error.is_some()
            || r.row.target_instance_id != s.row.target_instance_id
            || r.replay_owner_incarnation.is_some()
            || r.replay_episode_nonce
                .as_deref()
                .is_none_or(|n| uuid::Uuid::parse_str(n).is_err())
            || preserved.is_none_or(|p| {
                p.len() != 1
                    || !matches!(
                        p.get("provenance").and_then(Value::as_str),
                        Some("new_input" | "reentry")
                    )
            })
        {
            return Err(BlockReason::Replay);
        }
        ensure_same_request_on_connection(&mut **tx, r.row.id, &input)
            .await
            .map_err(|_| BlockReason::Replay)?;
        receipts.push(serde_json::json!([
            r.row.id,
            r.replay_request_key,
            r.replay_request_hash,
            r.replay_episode_nonce,
            r.replay_preserved
        ]));
    }
    Ok(Value::Array(receipts))
}

async fn safety(tx: &mut Transaction<'_, Postgres>, s: &Snapshot) -> Result<Value, BlockReason> {
    if s.home_epoch.is_some() {
        return Err(BlockReason::Home);
    }
    let receipts = registered(tx, s).await?;
    let ids = sources(&s.row)?;
    let unsafe_history: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox
        WHERE id<>$1 AND lower(btrim(provider))=$2 AND channel_id=$3 AND NOT replay_only
        AND (user_msg_id=$4 OR user_msg_id=ANY($5::TEXT[]) OR replay_source_message_ids && $5::TEXT[])
        AND (status<>'failed_pre_accept' OR accepted_at IS NOT NULL OR spawned_at IS NOT NULL
        OR dispatched_at IS NOT NULL OR home_epoch IS NOT NULL OR replay_disposition IS NOT NULL
        OR replay_request_key IS NOT NULL OR last_error LIKE 'herdr-consent-%' OR last_error LIKE 'claude-migration-%'))")
        .bind(s.row.id).bind(&s.row.provider).bind(&s.row.channel_id).bind(&s.row.user_msg_id).bind(ids)
        .fetch_one(&mut **tx).await.map_err(|_| BlockReason::Family)?;
    if unsafe_history {
        return Err(BlockReason::Family);
    }
    let parent_ok = match (s.row.attempt_no, s.row.parent_outbox_id) {
        (1, None) => true,
        (n, Some(parent)) if n > 1 => sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM intake_outbox
            WHERE id=$1 AND provider=$2 AND channel_id=$3 AND user_msg_id=$4 AND attempt_no=$5
            AND status='failed_pre_accept' AND accepted_at IS NULL AND spawned_at IS NULL AND dispatched_at IS NULL)")
            .bind(parent).bind(&s.row.provider).bind(&s.row.channel_id).bind(&s.row.user_msg_id).bind(n-1)
            .fetch_one(&mut **tx).await.map_err(|_| BlockReason::Family)?,
        _ => false,
    };
    if parent_ok {
        Ok(receipts)
    } else {
        Err(BlockReason::Family)
    }
}

fn pending(s: &Snapshot) -> Result<(), BlockReason> {
    if s.row.status != super::IntakeOutboxStatus::Pending || s.row.claim_owner.is_some() {
        return Err(BlockReason::State);
    }
    if s.claimed_at.is_some()
        || s.accepted_at.is_some()
        || s.spawned_at.is_some()
        || s.dispatched_at.is_some()
        || s.completed_at.is_some()
    {
        return Err(BlockReason::Clock);
    }
    Ok(())
}

pub(crate) async fn fenced_waiting(
    pool: &PgPool,
    intent: &WaitIntent,
) -> Result<FencedWaiting, String> {
    let mut tx = owner_gate(pool, &intent.authority).await?;
    sqlx::query("LOCK TABLE intake_outbox IN SHARE MODE")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    if revision(&mut tx, &intent.authority, intent.selector.canonical_row).await? != intent.selector
    {
        return Err("migration selector revision changed".into());
    }
    let facts = crate::db::o_channel_activation::read_activation_rows(
        &mut tx,
        &intent.authority.channel_id.to_string(),
        &intent.authority.local_instance,
    )
    .await
    .map_err(|e| e.to_string())?;
    let queued_bodies = crate::db::o_channel_activation::read_queued_bodies(
        &mut tx,
        &intent.authority.channel_id.to_string(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut replay_registrations = Vec::new();
    let rows = sqlx::query_as::<_, Snapshot>("SELECT * FROM intake_outbox WHERE channel_id=$1
        AND (status IN ('pending','claimed','accepted','spawned','dispatched','unknown','failed_post_accept')
        OR (status='failed_pre_accept' AND (accepted_at IS NOT NULL OR spawned_at IS NOT NULL OR dispatched_at IS NOT NULL)))
        AND NOT replay_only ORDER BY created_at,id")
        .bind(intent.authority.channel_id.to_string()).fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
    let (mut waiting, mut blockers) = (Vec::new(), Vec::new());
    for s in rows {
        let checked = async {
            let own = owner(&s.row, intent)?;
            pending(&s)?;
            let receipts = safety(&mut tx, &s).await?;
            marker(&s, intent, &receipts)?;
            if s.row.attempt_no <= 0 || (s.row.attempt_no > 1 && s.row.parent_outbox_id.is_none()) {
                return Err(BlockReason::Family);
            }
            let registered = receipts
                .as_array()
                .ok_or(BlockReason::Replay)?
                .iter()
                .map(|receipt| {
                    receipt
                        .get(0)
                        .and_then(Value::as_i64)
                        .ok_or(BlockReason::Replay)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                WaitingRef {
                    row_id: s.row.id,
                    source_ids: sources(&s.row)?,
                    payload_digest: super::consent_handoff::payload(&s.row)
                        .map_err(|_| BlockReason::Payload)?,
                    owner: own,
                    created_at: s.created_at,
                    attempt_no: s.row.attempt_no,
                },
                registered,
            ))
        }
        .await;
        match checked {
            Ok((r, registered)) => {
                waiting.push(r);
                replay_registrations.extend(registered);
            }
            Err(reason) => blockers.push(Blocker {
                row_id: s.row.id,
                reason,
            }),
        }
    }
    replay_registrations.sort_unstable();
    replay_registrations.dedup();
    let replay = sqlx::query_scalar::<_, i64>("SELECT id FROM intake_outbox WHERE channel_id=$1 AND replay_only
        AND (replay_disposition IS DISTINCT FROM 'classified_normal' OR status IN ('pending','claimed','accepted','spawned','dispatched','failed_post_accept')) ORDER BY created_at,id")
        .bind(intent.authority.channel_id.to_string()).fetch_all(&mut *tx).await.map_err(|e| e.to_string())?;
    for row_id in replay {
        if !replay_registrations.contains(&row_id) {
            blockers.push(Blocker {
                row_id,
                reason: BlockReason::Replay,
            });
        }
    }
    Ok(FencedWaiting {
        transaction: tx,
        intent: intent.clone(),
        waiting: VerifiedWaitingSet(waiting),
        blockers,
        foreign_sessions: facts.foreign_sessions,
        queued_bodies,
        replay_registrations,
    })
}

tokio::task_local! { static LOSE_COMMIT_ACK: bool; static CLAIM_BEFORE_READBACK: bool; }
pub(crate) async fn with_lost_commit_ack<T>(future: impl std::future::Future<Output = T>) -> T {
    LOSE_COMMIT_ACK.scope(true, future).await
}

pub(crate) async fn with_claim_before_readback<T>(
    future: impl std::future::Future<Output = T>,
) -> T {
    CLAIM_BEFORE_READBACK
        .scope(true, with_lost_commit_ack(future))
        .await
}

pub(crate) async fn park_before_effects(
    pool: &PgPool,
    proof: BeforeEffects,
    intent: &WaitIntent,
) -> Result<ParkOutcome, String> {
    let expected = proof.row();
    if expected.status != super::IntakeOutboxStatus::Spawned
        || expected.claim_owner.as_deref().is_none_or(str::is_empty)
    {
        return Ok(ParkOutcome::Blocked(
            "missing exact before-effects spawned claim".into(),
        ));
    }
    let mut tx = owner_gate(pool, &intent.authority).await?;
    let stale_selector = revision(&mut tx, &intent.authority, intent.selector.canonical_row)
        .await?
        != intent.selector;
    let current =
        sqlx::query_as::<_, Snapshot>("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(expected.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    let Some(s) = current else {
        return Ok(ParkOutcome::Blocked("original row missing".into()));
    };
    let stale_marker = s
        .last_error
        .as_deref()
        .and_then(|value| value.strip_prefix(PREFIX))
        .and_then(|encoded| serde_json::from_str::<Marker>(encoded).ok())
        .is_some_and(|prior| prior.intent.authority == intent.authority && prior.intent != *intent);
    if stale_selector || stale_marker {
        return Ok(ParkOutcome::NeedsReclassification(Box::new(
            Reclassification {
                proof,
                intent: intent.clone(),
                created_at: s.created_at,
            },
        )));
    }
    let checked = async {
        let own = owner(&s.row, intent)?;
        if super::consent_handoff::payload(&s.row).map_err(|_| BlockReason::Payload)?
            != super::consent_handoff::payload(expected).map_err(|_| BlockReason::Payload)?
            || s.row.owner_instance_id != expected.owner_instance_id
            || s.row.owner_generation != expected.owner_generation
        {
            return Err(BlockReason::Payload);
        }
        let receipts = safety(&mut tx, &s).await?;
        let prior = marker(&s, intent, &receipts)?;
        if pending(&s).is_ok()
            && prior
                .as_ref()
                .is_some_and(|m| Some(&m.claim) == expected.claim_owner.as_ref())
        {
            return Ok(None);
        }
        if s.row.status != super::IntakeOutboxStatus::Spawned
            || s.row.claim_owner != expected.claim_owner
        {
            return Err(BlockReason::State);
        }
        if s.accepted_at.is_none()
            || s.spawned_at.is_none()
            || s.dispatched_at.is_some()
            || s.completed_at.is_some()
        {
            return Err(BlockReason::Clock);
        }
        Ok(Some(Marker {
            row: s.row.id,
            created_at: s.created_at,
            payload: super::consent_handoff::payload(&s.row).map_err(|_| BlockReason::Payload)?,
            claim: s.row.claim_owner.clone().ok_or(BlockReason::State)?,
            owner: own,
            intent: intent.clone(),
            receipts,
        }))
    }
    .await;
    let m = match checked {
        Ok(None) => return Ok(ParkOutcome::AlreadyParked),
        Ok(Some(m)) => m,
        Err(reason) => {
            return Ok(ParkOutcome::Blocked(format!(
                "migration wait blocked: {reason:?}"
            )));
        }
    };
    let encoded = format!(
        "{PREFIX}{}",
        serde_json::to_string(&m).map_err(|e| e.to_string())?
    );
    let changed = sqlx::query("UPDATE intake_outbox SET status='pending',claim_owner=NULL,claimed_at=NULL,
        accepted_at=NULL,spawned_at=NULL,last_error=$3 WHERE id=$1 AND status='spawned' AND claim_owner=$2
        AND last_error IS NOT DISTINCT FROM $4")
        .bind(s.row.id).bind(&s.row.claim_owner).bind(&encoded).bind(&s.last_error)
        .execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if changed != 1 {
        return Ok(ParkOutcome::Blocked("migration wait CAS lost".into()));
    }
    let committed = tx.commit().await;
    if committed.is_ok() && !LOSE_COMMIT_ACK.try_with(|lost| *lost).unwrap_or(false) {
        return Ok(ParkOutcome::Parked);
    }
    if CLAIM_BEFORE_READBACK
        .try_with(|claim| *claim)
        .unwrap_or(false)
    {
        let competing = super::claim_pending_for_target_except(
            pool,
            &intent.authority.local_instance,
            &intent.authority.provider,
            "migration-readback-competitor",
            &[],
        )
        .await
        .map_err(|e| e.to_string())?;
        if competing.as_ref().is_none_or(|r| r.id != m.row) {
            return Err("readback fixture did not claim its original row".into());
        }
    }
    let mut tx = owner_gate(pool, &intent.authority).await?;
    sqlx::query("LOCK TABLE intake_outbox IN SHARE MODE")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let revision_ok = revision(&mut tx, &intent.authority, intent.selector.canonical_row)
        .await
        .is_ok_and(|r| r == intent.selector);
    let readback = sqlx::query_as::<_, Snapshot>("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(m.row)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let exact = match readback {
        Some(s) => {
            revision_ok
                && pending(&s).is_ok()
                && s.last_error.as_deref() == Some(encoded.as_str())
                && safety(&mut tx, &s)
                    .await
                    .is_ok_and(|receipts| marker(&s, intent, &receipts).is_ok_and(|m| m.is_some()))
        }
        None => false,
    };
    tx.rollback().await.map_err(|e| e.to_string())?;
    Ok(if exact {
        ParkOutcome::AlreadyParked
    } else {
        ParkOutcome::Unknown("migration wait commit has no exact pending readback".into())
    })
}

#[path = "migration_wait_tests.rs"]
pub(crate) mod tests;
