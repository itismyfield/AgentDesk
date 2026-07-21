//! Repository layer for the scheduled-message reservation pool.
//!
//! All raw SQL for `scheduled_messages` and `scheduled_message_deliveries`
//! lives here. Route handlers and the scheduler worker delegate to these
//! functions and never issue SQL directly.
//!
//! Design: docs/design/scheduled-messages.md — definition + delivery rows
//! (routines/routine_runs pattern), at-most-once firing per
//! (message, fire slot) via `uq_smdel_fire_slot` and `FOR UPDATE SKIP LOCKED`.

use chrono::{DateTime, Utc};
use serde_json::{Value as JsonValue, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, Transaction};
use uuid::Uuid;

mod agent;
mod outbox;
mod writes;
pub use agent::{
    RunningAgentDelivery, commit_delivery_agent_launch_pg, defer_delivery_without_retry_pg,
    list_running_agent_deliveries_pg, mark_delivery_agent_turn_started_pg,
    record_delivery_agent_turn_intent_pg, recover_expired_leases_pg,
    release_agent_delivery_to_poller_pg,
};
pub use outbox::outbox_statuses_for_deliveries_pg;
pub use writes::{insert_scheduled_message_pg, insert_scheduled_message_tx};

#[cfg(test)]
mod postgres_tests;

pub const STATUS_SCHEDULED: &str = "scheduled";
pub const STATUS_FIRING: &str = "firing";
pub const STATUS_SENT: &str = "sent";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_EXPIRED: &str = "expired";

pub const DELIVERY_SENT: &str = "sent";
pub const DELIVERY_FAILED: &str = "failed";
pub const DELIVERY_INTERRUPTED: &str = "interrupted";

pub const KIND_PUSH: &str = "push";
pub const KIND_AGENT: &str = "agent";

const DEFINITION_COLUMNS: &str = "id, content, title, target_channel_id, bot, delivery_kind, \
     agent_id, agent_instruction, on_agent_failure, scheduled_at, schedule, timezone, \
     expires_at, status, in_flight_delivery_id, fire_count, last_fired_at, last_error, \
     source, created_by, dedupe_key, context_strategy, context_snapshot_id, \
     on_context_failure, created_at, updated_at";

pub const CONTEXT_STRATEGY_FRESH: &str = "fresh";
pub const CONTEXT_STRATEGY_SNAPSHOT: &str = "snapshot";
pub const ON_CONTEXT_FAILURE_FAIL: &str = "fail";
pub const ON_CONTEXT_FAILURE_FRESH: &str = "fresh";

// ── Row types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduledMessageRow {
    pub id: String,
    pub content: String,
    pub title: Option<String>,
    pub target_channel_id: Option<String>,
    pub bot: String,
    pub delivery_kind: String,
    pub agent_id: Option<String>,
    pub agent_instruction: Option<String>,
    pub on_agent_failure: String,
    pub scheduled_at: DateTime<Utc>,
    pub schedule: Option<String>,
    pub timezone: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub status: String,
    pub in_flight_delivery_id: Option<String>,
    pub fire_count: i64,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub source: String,
    pub created_by: Option<String>,
    pub dedupe_key: Option<String>,
    /// #4658: 'fresh' (default) or 'snapshot'. Snapshot definitions reference an
    /// immutable context row and run in an isolated fresh provider session.
    pub context_strategy: String,
    pub context_snapshot_id: Option<String>,
    /// #4658: 'fail' (default, fail-closed) or 'fresh' (opt-in degrade) when the
    /// snapshot cannot be validated at fire time.
    pub on_context_failure: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ScheduledMessageRow {
    pub fn to_api_json(&self) -> JsonValue {
        json!({
            "id": self.id,
            "content": self.content,
            "title": self.title,
            "targetChannelId": self.target_channel_id,
            "bot": self.bot,
            "deliveryKind": self.delivery_kind,
            "agentId": self.agent_id,
            "agentInstruction": self.agent_instruction,
            "onAgentFailure": self.on_agent_failure,
            "scheduledAt": self.scheduled_at.to_rfc3339(),
            "schedule": self.schedule,
            "timezone": self.timezone,
            "expiresAt": self.expires_at.map(|v| v.to_rfc3339()),
            "status": self.status,
            "inFlightDeliveryId": self.in_flight_delivery_id,
            "fireCount": self.fire_count,
            "lastFiredAt": self.last_fired_at.map(|v| v.to_rfc3339()),
            "lastError": self.last_error,
            "source": self.source,
            "createdBy": self.created_by,
            "dedupeKey": self.dedupe_key,
            "contextStrategy": self.context_strategy,
            "contextSnapshotId": self.context_snapshot_id,
            "onContextFailure": self.on_context_failure,
            "createdAt": self.created_at.to_rfc3339(),
            "updatedAt": self.updated_at.to_rfc3339(),
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeliveryRow {
    pub id: String,
    pub scheduled_message_id: String,
    pub fire_scheduled_at: DateTime<Utc>,
    pub delivery_kind: String,
    pub status: String,
    pub claim_owner: Option<String>,
    pub outbox_id: Option<i64>,
    pub turn_id: Option<String>,
    pub fallback_outbox_id: Option<i64>,
    pub retry_count: i32,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl DeliveryRow {
    pub fn to_api_json(&self) -> JsonValue {
        json!({
            "id": self.id,
            "scheduledMessageId": self.scheduled_message_id,
            "fireScheduledAt": self.fire_scheduled_at.to_rfc3339(),
            "deliveryKind": self.delivery_kind,
            "status": self.status,
            "claimOwner": self.claim_owner,
            "outboxId": self.outbox_id,
            "turnId": self.turn_id,
            "fallbackOutboxId": self.fallback_outbox_id,
            "retryCount": self.retry_count,
            "error": self.error,
            "startedAt": self.started_at.to_rfc3339(),
            "finishedAt": self.finished_at.map(|v| v.to_rfc3339()),
            "createdAt": self.created_at.to_rfc3339(),
        })
    }
}

/// A definition claimed for firing together with its delivery slot row.
#[derive(Debug, Clone)]
pub struct ClaimedFire {
    pub message: ScheduledMessageRow,
    pub delivery_id: String,
    pub claim_token: String,
    pub fire_scheduled_at: DateTime<Utc>,
    pub retry_count: i32,
}

#[derive(Debug, Clone)]
pub struct NewScheduledMessage {
    pub content: String,
    pub title: Option<String>,
    pub target_channel_id: Option<String>,
    pub bot: String,
    pub delivery_kind: String,
    pub agent_id: Option<String>,
    pub agent_instruction: Option<String>,
    pub on_agent_failure: String,
    pub scheduled_at: DateTime<Utc>,
    pub schedule: Option<String>,
    pub timezone: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub source: String,
    pub created_by: Option<String>,
    pub dedupe_key: Option<String>,
    /// #4658: 'fresh' (default) or 'snapshot'. Defaulted by the route.
    pub context_strategy: String,
    /// Snapshot id captured before insert (snapshot strategy only). NULL for fresh.
    pub context_snapshot_id: Option<String>,
    /// #4658: 'fail' (default) or 'fresh'.
    pub on_context_failure: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScheduledMessagePatch {
    pub content: Option<String>,
    pub title: Option<Option<String>>,
    pub target_channel_id: Option<Option<String>>,
    pub bot: Option<String>,
    pub agent_id: Option<Option<String>>,
    pub agent_instruction: Option<Option<String>>,
    pub on_agent_failure: Option<String>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub schedule: Option<Option<String>>,
    pub timezone: Option<String>,
    pub expires_at: Option<Option<DateTime<Utc>>>,
}

#[derive(Debug, Clone, Default)]
pub struct ListFilters {
    pub status: Option<String>,
    pub delivery_kind: Option<String>,
    pub agent_id: Option<String>,
    pub target_channel_id: Option<String>,
    pub due_before: Option<DateTime<Utc>>,
    pub due_after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub limit: i64,
}

// ── Definition CRUD ─────────────────────────────────────────────────────────

pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(
        error.as_database_error().and_then(|db| db.code()),
        Some(code) if code == "23505"
    )
}

pub async fn get_scheduled_message_pg(
    pool: &PgPool,
    id: &str,
) -> Result<Option<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(&format!(
        "SELECT {DEFINITION_COLUMNS} FROM scheduled_messages WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn find_active_by_dedupe_key_pg(
    pool: &PgPool,
    dedupe_key: &str,
) -> Result<Option<ScheduledMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, ScheduledMessageRow>(&format!(
        "SELECT {DEFINITION_COLUMNS} FROM scheduled_messages
         WHERE dedupe_key = $1 AND status IN ('scheduled', 'firing')
         LIMIT 1"
    ))
    .bind(dedupe_key)
    .fetch_optional(pool)
    .await
}

pub async fn list_scheduled_messages_pg(
    pool: &PgPool,
    filters: &ListFilters,
) -> Result<Vec<ScheduledMessageRow>, sqlx::Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {DEFINITION_COLUMNS} FROM scheduled_messages WHERE 1=1"
    ));
    if let Some(status) = &filters.status {
        builder.push(" AND status = ").push_bind(status);
    }
    if let Some(kind) = &filters.delivery_kind {
        builder.push(" AND delivery_kind = ").push_bind(kind);
    }
    if let Some(agent_id) = &filters.agent_id {
        builder.push(" AND agent_id = ").push_bind(agent_id);
    }
    if let Some(channel) = &filters.target_channel_id {
        builder.push(" AND target_channel_id = ").push_bind(channel);
    }
    if let Some(due_before) = filters.due_before {
        builder.push(" AND scheduled_at <= ").push_bind(due_before);
    }
    if let Some(due_after) = filters.due_after {
        builder.push(" AND scheduled_at >= ").push_bind(due_after);
    }
    if let Some(before) = filters.before {
        builder.push(" AND created_at < ").push_bind(before);
    }
    builder
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(filters.limit.clamp(1, 200));
    builder.build_query_as().fetch_all(pool).await
}

/// Apply a patch to a definition; only rows still in `scheduled` are editable.
/// Returns the updated row, or `None` when the row is missing or not editable.
pub async fn update_scheduled_message_pg(
    pool: &PgPool,
    id: &str,
    patch: &ScheduledMessagePatch,
) -> Result<Option<ScheduledMessageRow>, sqlx::Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "UPDATE scheduled_messages
         SET updated_at = NOW(), runtime_defer_until = NULL",
    );
    if let Some(content) = &patch.content {
        builder.push(", content = ").push_bind(content);
    }
    if let Some(title) = &patch.title {
        builder.push(", title = ").push_bind(title);
    }
    if let Some(channel) = &patch.target_channel_id {
        builder.push(", target_channel_id = ").push_bind(channel);
    }
    if let Some(bot) = &patch.bot {
        builder.push(", bot = ").push_bind(bot);
    }
    if let Some(agent_id) = &patch.agent_id {
        builder.push(", agent_id = ").push_bind(agent_id);
    }
    if let Some(instruction) = &patch.agent_instruction {
        builder
            .push(", agent_instruction = ")
            .push_bind(instruction);
    }
    if let Some(on_failure) = &patch.on_agent_failure {
        builder.push(", on_agent_failure = ").push_bind(on_failure);
    }
    if let Some(scheduled_at) = patch.scheduled_at {
        builder.push(", scheduled_at = ").push_bind(scheduled_at);
    }
    if let Some(schedule) = &patch.schedule {
        builder.push(", schedule = ").push_bind(schedule);
    }
    if let Some(timezone) = &patch.timezone {
        builder.push(", timezone = ").push_bind(timezone);
    }
    if let Some(expires_at) = &patch.expires_at {
        builder.push(", expires_at = ").push_bind(expires_at);
    }
    builder
        .push(" WHERE id = ")
        .push_bind(id)
        .push(" AND status = 'scheduled' RETURNING ")
        .push(DEFINITION_COLUMNS);
    builder.build_query_as().fetch_optional(pool).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    NotFound,
    /// Row was already terminal; contains the terminal status.
    AlreadyTerminal(String),
    Canceled {
        /// True when a firing delivery was marked interrupted; the message may
        /// already be past the outbox/turn handoff point.
        was_firing: bool,
        /// True only when the active delivery had already recorded an outbox
        /// handoff or committed an agent launch before cancellation won.
        handoff_started: bool,
    },
}

pub async fn cancel_scheduled_message_pg(
    pool: &PgPool,
    id: &str,
) -> Result<CancelOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT status, in_flight_delivery_id FROM scheduled_messages
         WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(CancelOutcome::NotFound);
    };
    let status: String = row.try_get("status")?;
    let in_flight: Option<String> = row.try_get("in_flight_delivery_id")?;
    if status != STATUS_SCHEDULED && status != STATUS_FIRING {
        return Ok(CancelOutcome::AlreadyTerminal(status));
    }
    let was_firing = status == STATUS_FIRING;
    let mut handoff_started = false;
    if let Some(delivery_id) = in_flight.as_deref() {
        let handoff = sqlx::query(
            "SELECT outbox_id, fallback_outbox_id, turn_id,
                    turn_intent_at, launch_committed_at
             FROM scheduled_message_deliveries
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(delivery_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(handoff) = handoff {
            handoff_started = handoff.try_get::<Option<i64>, _>("outbox_id")?.is_some()
                || handoff
                    .try_get::<Option<i64>, _>("fallback_outbox_id")?
                    .is_some()
                || handoff
                    .try_get::<Option<DateTime<Utc>>, _>("launch_committed_at")?
                    .is_some()
                // Rolling-deploy compatibility: an old writer can persist a
                // turn id after 0086 without the new intent/commit markers.
                // Its launch state is unknowable, so report/adopt it as handed
                // off rather than risk a replacement.
                || (handoff.try_get::<Option<String>, _>("turn_id")?.is_some()
                    && handoff
                        .try_get::<Option<DateTime<Utc>>, _>("turn_intent_at")?
                        .is_none());
        }
        sqlx::query(
            "UPDATE scheduled_message_deliveries
             SET status = 'interrupted', error = 'canceled by operator',
                 finished_at = NOW(), updated_at = NOW()
             WHERE id = $1 AND status = 'running'",
        )
        .bind(delivery_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE scheduled_messages
         SET status = 'canceled', in_flight_delivery_id = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CancelOutcome::Canceled {
        was_firing,
        handoff_started,
    })
}

// ── Deliveries ──────────────────────────────────────────────────────────────

pub async fn list_deliveries_pg(
    pool: &PgPool,
    scheduled_message_id: &str,
    limit: i64,
    before: Option<DateTime<Utc>>,
) -> Result<Vec<DeliveryRow>, sqlx::Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT * FROM scheduled_message_deliveries WHERE scheduled_message_id = ",
    );
    builder.push_bind(scheduled_message_id);
    if let Some(before) = before {
        builder.push(" AND created_at < ").push_bind(before);
    }
    builder
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit.clamp(1, 100));
    builder.build_query_as().fetch_all(pool).await
}

// ── Firing (worker) ─────────────────────────────────────────────────────────

/// Claim up to `batch` due definitions for firing. For each claimed row a
/// delivery slot row is created (or an interrupted one from a prior attempt is
/// re-armed). Multi-node safe: `FOR UPDATE SKIP LOCKED` on the definition and
/// `uq_smdel_fire_slot` on the delivery keep each fire slot at-most-once.
pub async fn claim_due_fires_pg(
    pool: &PgPool,
    claim_owner: &str,
    delivery_runtime_available: bool,
    batch: i64,
    lease_secs: i64,
    now: DateTime<Utc>,
) -> Result<Vec<ClaimedFire>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let due = sqlx::query_as::<_, ScheduledMessageRow>(&format!(
        "SELECT {DEFINITION_COLUMNS} FROM scheduled_messages
         WHERE status = 'scheduled' AND scheduled_at <= $1
           AND $2
           AND (runtime_defer_until IS NULL OR runtime_defer_until <= $1)
           AND NOT EXISTS (
               SELECT 1
               FROM scheduled_message_deliveries AS retry
               WHERE retry.scheduled_message_id = scheduled_messages.id
                 AND retry.fire_scheduled_at = scheduled_messages.scheduled_at
                 AND retry.status = 'interrupted'
                 AND retry.next_attempt_at > $1
           )
         ORDER BY scheduled_at
         LIMIT $3
         FOR UPDATE SKIP LOCKED"
    ))
    .bind(now)
    .bind(delivery_runtime_available)
    .bind(batch)
    .fetch_all(&mut *tx)
    .await?;

    let mut claimed = Vec::with_capacity(due.len());
    for message in due {
        let Some(fire) = arm_delivery_slot_tx(
            &mut tx,
            &message,
            message.scheduled_at,
            message.scheduled_at,
            claim_owner,
            lease_secs,
            now,
        )
        .await?
        else {
            continue;
        };
        claimed.push(fire);
    }
    tx.commit().await?;
    Ok(claimed)
}

/// Arm the delivery slot for one claimed definition inside the claim
/// transaction. Returns `None` when the slot already holds a terminal delivery
/// (a prior node finished it but crashed before advancing the parent); in that
/// case the parent is finalized directly so it stops matching the due scan.
async fn arm_delivery_slot_tx(
    tx: &mut Transaction<'_, Postgres>,
    message: &ScheduledMessageRow,
    fire_scheduled_at: DateTime<Utc>,
    resume_scheduled_at: DateTime<Utc>,
    claim_owner: &str,
    lease_secs: i64,
    now: DateTime<Utc>,
) -> Result<Option<ClaimedFire>, sqlx::Error> {
    let delivery_id = format!("smdel_{}", Uuid::new_v4());
    let claim_token = format!("smclaim_{}", Uuid::new_v4());
    let armed = sqlx::query(
        "INSERT INTO scheduled_message_deliveries
            (id, scheduled_message_id, fire_scheduled_at, resume_scheduled_at,
             delivery_kind, status, claim_owner, claim_token, lease_expires_at)
         VALUES ($1, $2, $3, $4, $5, 'running', $6, $7,
                 $8 + ($9::bigint * INTERVAL '1 second'))
         ON CONFLICT (scheduled_message_id, fire_scheduled_at) DO UPDATE
            SET status = 'running',
                claim_owner = EXCLUDED.claim_owner,
                claim_token = EXCLUDED.claim_token,
                lease_expires_at = EXCLUDED.lease_expires_at,
                retry_count = scheduled_message_deliveries.retry_count + 1,
                outbox_id = NULL,
                turn_id = NULL,
                turn_intent_at = NULL,
                launch_committed_at = NULL,
                turn_started_at = NULL,
                fallback_outbox_id = NULL,
                next_attempt_at = NULL,
                error = NULL,
                started_at = NOW(),
                finished_at = NULL,
                updated_at = NOW()
          WHERE scheduled_message_deliveries.status = 'interrupted'
         RETURNING id, retry_count, claim_token, resume_scheduled_at",
    )
    .bind(&delivery_id)
    .bind(&message.id)
    .bind(fire_scheduled_at)
    .bind(resume_scheduled_at)
    .bind(&message.delivery_kind)
    .bind(claim_owner)
    .bind(&claim_token)
    .bind(now)
    .bind(lease_secs)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(armed) = armed else {
        // Slot exists and is not interrupted: a prior attempt finished (or is
        // still running elsewhere, which cannot happen while the parent is
        // 'scheduled'). Mirror its terminal state onto the parent.
        let existing_status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM scheduled_message_deliveries
             WHERE scheduled_message_id = $1 AND fire_scheduled_at = $2",
        )
        .bind(&message.id)
        .bind(fire_scheduled_at)
        .fetch_optional(&mut **tx)
        .await?;
        let parent_status = match existing_status.as_deref() {
            Some(DELIVERY_SENT) => STATUS_SENT,
            _ => STATUS_FAILED,
        };
        sqlx::query(
            "UPDATE scheduled_messages
             SET status = $2, in_flight_delivery_id = NULL, updated_at = NOW()
             WHERE id = $1",
        )
        .bind(&message.id)
        .bind(parent_status)
        .execute(&mut **tx)
        .await?;
        return Ok(None);
    };

    let armed_id: String = armed.try_get("id")?;
    let retry_count: i32 = armed.try_get("retry_count")?;
    let claim_token: String = armed.try_get("claim_token")?;
    let resume_scheduled_at: DateTime<Utc> = armed.try_get("resume_scheduled_at")?;
    sqlx::query(
        "UPDATE scheduled_messages
         SET status = 'firing', in_flight_delivery_id = $2,
             runtime_defer_until = NULL, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&message.id)
    .bind(&armed_id)
    .execute(&mut **tx)
    .await?;

    let mut claimed_message = message.clone();
    claimed_message.scheduled_at = resume_scheduled_at;
    Ok(Some(ClaimedFire {
        message: claimed_message,
        delivery_id: armed_id,
        claim_token,
        fire_scheduled_at,
        retry_count,
    }))
}

/// Immediately arm a fire slot for `trigger-now`, bypassing the due scan.
/// Returns `None` when the definition is not currently `scheduled`.
pub async fn trigger_now_pg(
    pool: &PgPool,
    id: &str,
    claim_owner: &str,
    lease_secs: i64,
) -> Result<Option<ClaimedFire>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let message = sqlx::query_as::<_, ScheduledMessageRow>(&format!(
        "SELECT {DEFINITION_COLUMNS} FROM scheduled_messages
         WHERE id = $1 AND status = 'scheduled'
         FOR UPDATE SKIP LOCKED"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(message) = message else {
        return Ok(None);
    };
    // Manual fires get their own slot at NOW() so the original scheduled_at
    // slot stays free for the regular due scan (relevant for recurring rows).
    // The original slot is persisted separately as the recurrence anchor so
    // transient retries cannot shift the definition's cadence.
    let original_scheduled_at = message.scheduled_at;
    let now = Utc::now();
    let claimed = arm_delivery_slot_tx(
        &mut tx,
        &message,
        now,
        original_scheduled_at,
        claim_owner,
        lease_secs,
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(claimed)
}

// ── Delivery + parent state transitions (worker) ────────────────────────────

/// Terminal transition for a delivery row inside a caller-owned transaction.
/// No-op when the row already left `running` (stale lease double-completion
/// guard, message_outbox pattern). Returns whether the row transitioned.
async fn finish_delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    delivery_id: &str,
    claim_token: &str,
    status: &str,
    error: Option<&str>,
    outbox_id: Option<i64>,
    fallback_outbox_id: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET status = $3, error = $4,
             outbox_id = COALESCE($5, outbox_id),
             fallback_outbox_id = COALESCE($6, fallback_outbox_id),
             finished_at = NOW(), updated_at = NOW()
         WHERE id = $1 AND claim_token = $2 AND status = 'running'",
    )
    .bind(delivery_id)
    .bind(claim_token)
    .bind(status)
    .bind(error)
    .bind(outbox_id)
    .bind(fallback_outbox_id)
    .execute(&mut **tx)
    .await?;
    Ok(updated.rows_affected() > 0)
}

/// Acquire the parent definition before touching its active delivery. All
/// delivery/parent terminal transitions use this order, matching operator
/// cancellation and preventing parent↔delivery lock inversion deadlocks.
async fn lock_active_parent_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: &str,
    delivery_id: &str,
) -> Result<bool, sqlx::Error> {
    let locked = sqlx::query_scalar::<_, i32>(
        "SELECT 1
         FROM scheduled_messages
         WHERE id = $1 AND in_flight_delivery_id = $2 AND status = 'firing'
         FOR UPDATE",
    )
    .bind(message_id)
    .bind(delivery_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(locked.is_some())
}

/// Lock the active parent first, then fence and lock its running delivery.
///
/// Agent completion needs to hold both locks while it re-checks transcript
/// evidence and (for a confirmed no-reply outcome) stages a raw fallback.
/// This prevents a lease takeover, cancellation, or second poller from
/// committing a competing terminal outcome around that outbox handoff.
pub(crate) async fn lock_active_delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: &str,
    delivery_id: &str,
    claim_token: &str,
) -> Result<bool, sqlx::Error> {
    if !lock_active_parent_tx(tx, message_id, delivery_id).await? {
        return Ok(false);
    }
    let locked = sqlx::query_scalar::<_, i32>(
        "SELECT 1
         FROM scheduled_message_deliveries
         WHERE id = $1 AND claim_token = $2 AND status = 'running'
         FOR UPDATE",
    )
    .bind(delivery_id)
    .bind(claim_token)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(locked.is_some())
}

/// Finish a delivery whose parent and child rows were already locked by
/// [`lock_active_delivery_tx`], then advance or terminalize the parent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_locked_delivery_and_finalize_parent_tx(
    tx: &mut Transaction<'_, Postgres>,
    delivery_id: &str,
    claim_token: &str,
    delivery_status: &str,
    error: Option<&str>,
    outbox_id: Option<i64>,
    fallback_outbox_id: Option<i64>,
    message_id: &str,
    fired: bool,
    terminal_status: &str,
    next_scheduled_at: Option<DateTime<Utc>>,
) -> Result<bool, sqlx::Error> {
    if !finish_delivery_tx(
        tx,
        delivery_id,
        claim_token,
        delivery_status,
        error,
        outbox_id,
        fallback_outbox_id,
    )
    .await?
    {
        return Ok(false);
    }
    finalize_parent_tx(
        tx,
        message_id,
        delivery_id,
        fired,
        terminal_status,
        error,
        next_scheduled_at,
    )
    .await?;
    Ok(true)
}

/// Atomically finish a delivery and finalize its parent in one transaction, so
/// a crash between the two writes can never strand the parent in `firing` with
/// a terminal in-flight delivery. When the delivery already left `running`
/// (another node completed it), the parent is left untouched and `false` is
/// returned. `next_scheduled_at` present → recurring: re-arm for that slot;
/// otherwise the parent lands on `terminal_status`.
#[allow(clippy::too_many_arguments)]
pub async fn finish_delivery_and_finalize_parent_pg(
    pool: &PgPool,
    delivery_id: &str,
    claim_token: &str,
    delivery_status: &str,
    error: Option<&str>,
    outbox_id: Option<i64>,
    fallback_outbox_id: Option<i64>,
    message_id: &str,
    fired: bool,
    terminal_status: &str,
    next_scheduled_at: Option<DateTime<Utc>>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_active_delivery_tx(&mut tx, message_id, delivery_id, claim_token).await? {
        return Ok(false);
    }
    if !finish_locked_delivery_and_finalize_parent_tx(
        &mut tx,
        delivery_id,
        claim_token,
        delivery_status,
        error,
        outbox_id,
        fallback_outbox_id,
        message_id,
        fired,
        terminal_status,
        next_scheduled_at,
    )
    .await?
    {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// Atomically mark a delivery `interrupted` and rewind its parent to the fire
/// slot so the due scan re-arms it (bounded by the claim-time retry cap).
pub async fn interrupt_delivery_and_rewind_pg(
    pool: &PgPool,
    delivery_id: &str,
    claim_token: &str,
    message_id: &str,
    fire_scheduled_at: DateTime<Utc>,
    next_attempt_at: Option<DateTime<Utc>>,
    error: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_active_parent_tx(&mut tx, message_id, delivery_id).await? {
        return Ok(false);
    }
    if !finish_delivery_tx(
        &mut tx,
        delivery_id,
        claim_token,
        DELIVERY_INTERRUPTED,
        Some(error),
        None,
        None,
    )
    .await?
    {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET turn_id = NULL, turn_intent_at = NULL,
             launch_committed_at = NULL, turn_started_at = NULL,
             next_attempt_at = $3, updated_at = NOW()
         WHERE id = $1 AND claim_token = $2 AND status = 'interrupted'",
    )
    .bind(delivery_id)
    .bind(claim_token)
    .bind(next_attempt_at)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE scheduled_messages
         SET status = 'scheduled', scheduled_at = $3,
             in_flight_delivery_id = NULL, last_error = $4, updated_at = NOW()
         WHERE id = $1 AND in_flight_delivery_id = $2 AND status = 'firing'",
    )
    .bind(message_id)
    .bind(delivery_id)
    .bind(fire_scheduled_at)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Close out the parent after its in-flight delivery reached a terminal state.
/// `next_scheduled_at` present → recurring: re-arm for the next slot.
async fn finalize_parent_tx(
    tx: &mut Transaction<'_, Postgres>,
    message_id: &str,
    delivery_id: &str,
    fired: bool,
    terminal_status: &str,
    last_error: Option<&str>,
    next_scheduled_at: Option<DateTime<Utc>>,
) -> Result<(), sqlx::Error> {
    match next_scheduled_at {
        Some(next) => {
            sqlx::query(
                "UPDATE scheduled_messages
                 SET status = 'scheduled', scheduled_at = $3,
                     in_flight_delivery_id = NULL,
                     fire_count = fire_count + CASE WHEN $4 THEN 1 ELSE 0 END,
                     last_fired_at = CASE WHEN $4 THEN NOW() ELSE last_fired_at END,
                     last_error = $5, updated_at = NOW()
                 WHERE id = $1 AND in_flight_delivery_id = $2",
            )
            .bind(message_id)
            .bind(delivery_id)
            .bind(next)
            .bind(fired)
            .bind(last_error)
            .execute(&mut **tx)
            .await?;
        }
        None => {
            sqlx::query(
                "UPDATE scheduled_messages
                 SET status = $3, in_flight_delivery_id = NULL,
                     fire_count = fire_count + CASE WHEN $4 THEN 1 ELSE 0 END,
                     last_fired_at = CASE WHEN $4 THEN NOW() ELSE last_fired_at END,
                     last_error = $5, updated_at = NOW()
                 WHERE id = $1 AND in_flight_delivery_id = $2",
            )
            .bind(message_id)
            .bind(delivery_id)
            .bind(terminal_status)
            .bind(fired)
            .bind(last_error)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

pub async fn mark_expired_pg(
    pool: &PgPool,
    message_id: &str,
    delivery_id: &str,
    claim_token: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_active_parent_tx(&mut tx, message_id, delivery_id).await? {
        return Ok(false);
    }
    // Stale double-completion guard: when the delivery already left `running`
    // (a lease-recovered peer re-armed and finished this slot), that peer owns
    // the parent's final state — don't overwrite it with 'expired'.
    if !finish_delivery_tx(
        &mut tx,
        delivery_id,
        claim_token,
        DELIVERY_INTERRUPTED,
        Some("definition expired before firing"),
        None,
        None,
    )
    .await?
    {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE scheduled_messages
         SET status = 'expired', in_flight_delivery_id = NULL, updated_at = NOW()
         WHERE id = $1 AND in_flight_delivery_id = $2",
    )
    .bind(message_id)
    .bind(delivery_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
