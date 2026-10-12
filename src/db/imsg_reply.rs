//! Dormant iMessage reply storage; production senders are not connected to this module.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool, Row};
use uuid::Uuid;

#[derive(Debug, FromRow)]
pub struct Obligation {
    pub origin_id: String,
    pub source: String,
    pub agent_id: String,
    pub channel_id: String,
    pub person_id: i64,
    pub request_hash: String,
    pub opened_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub admission: String,
    pub admission_kind: Option<String>,
    pub admission_result: Option<Value>,
    pub delivery_confirmed: Option<bool>,
    pub resolution: String,
    pub admitted_turn_id: Option<String>,
    pub queued_message_id: Option<String>,
    pub episode_key: Option<String>,
    pub inject_nonce: Option<String>,
    pub last_reply_episode: Option<String>,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub terminal_seen_at: Option<DateTime<Utc>>,
    pub terminal_first_line: Option<String>,
    pub active_send_action: Option<Uuid>,
    pub discord_mirror_id: Option<String>,
    pub closed_at: Option<DateTime<Utc>>,
    pub closed_reason: Option<String>,
}

#[derive(Debug, FromRow)]
pub struct Action {
    pub id: Uuid,
    pub origin_id: String,
    pub purpose: String,
    pub action_key: String,
    pub status: String,
    pub reply_episode_key: Option<String>,
    pub claimed_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
    pub receipt: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Progress,
    Final,
    Fallback,
    Expiry,
    Mirror,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Final => "final",
            Self::Fallback => "fallback",
            Self::Expiry => "expiry",
            Self::Mirror => "mirror",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckOutcome {
    Sent,
    Unknown,
    NotSent,
}

impl AckOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Unknown => "unknown",
            Self::NotSent => "not_sent",
        }
    }
}

pub struct ClaimAction<'a> {
    pub id: Uuid,
    pub origin_id: &'a str,
    pub purpose: Purpose,
    pub action_key: &'a str,
    pub reply_episode_key: Option<&'a str>,
}

/// The caller must verify sender authorization and transport evidence before supplying an ACK.
pub struct ActionAck<'a> {
    pub origin_id: &'a str,
    pub id: Uuid,
    pub outcome: AckOutcome,
    pub receipt: Option<Value>,
}

pub(crate) struct AdmissionIdentity<'a> {
    pub origin_id: &'a str,
    pub agent_id: &'a str,
    pub channel_id: &'a str,
    pub person_id: i64,
    pub request_hash: &'a str,
}

pub(crate) struct AdmissionSettlement<'a> {
    pub admission: &'a str,
    pub kind: Option<&'a str>,
    pub result: &'a Value,
    pub delivery_confirmed: Option<bool>,
    pub turn_id: Option<&'a str>,
}

pub async fn read_obligation(
    pool: &PgPool,
    origin_id: &str,
) -> Result<Option<Obligation>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM imsg_reply_obligations WHERE origin_id = $1")
        .bind(origin_id)
        .fetch_optional(pool)
        .await
}

pub async fn read_action(pool: &PgPool, id: Uuid) -> Result<Option<Action>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM imsg_reply_actions WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Only the inserted GUID owner may perform delivery; duplicates require a fresh read.
pub(crate) async fn reserve_admission(
    pool: &PgPool,
    identity: &AdmissionIdentity<'_>,
) -> Result<bool, sqlx::Error> {
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO imsg_reply_obligations (
             origin_id, source, agent_id, channel_id,
             person_id, request_hash, opened_at, expires_at
         ) VALUES ($1, 'imessage', $2, $3, $4, $5, NOW(), NOW() + INTERVAL '24 hours')
         ON CONFLICT (origin_id) DO NOTHING RETURNING origin_id",
    )
    .bind(identity.origin_id)
    .bind(identity.agent_id)
    .bind(identity.channel_id)
    .bind(identity.person_id)
    .bind(identity.request_hash)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

/// Settlement changes admission metadata without reopening a resolution recorded by an early ACK.
pub(crate) async fn settle_admission(
    pool: &PgPool,
    origin_id: &str,
    settlement: &AdmissionSettlement<'_>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE imsg_reply_obligations SET
             admission = $2, admission_kind = $3, admission_result = $4,
             delivery_confirmed = $5,
             admitted_turn_id = COALESCE($6, admitted_turn_id),
             resolution = CASE
                 WHEN $2 = 'refused' AND resolution = 'open' THEN 'closed'
                 ELSE resolution END,
             closed_at = CASE
                 WHEN $2 = 'refused' AND resolution = 'open' THEN NOW()
                 ELSE closed_at END
         WHERE origin_id = $1 AND admission = 'admitting'",
    )
    .bind(origin_id)
    .bind(settlement.admission)
    .bind(settlement.kind)
    .bind(settlement.result)
    .bind(settlement.delivery_confirmed)
    .bind(settlement.turn_id)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// Before a start or busy paste: the delivering admission names its episode candidate, once.
/// Each guarded UPDATE autocommits, so `true` means exactly one committed row.
pub(crate) async fn bind_episode(
    pool: &PgPool,
    origin_id: &str,
    episode: &str,
    nonce: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE imsg_reply_obligations SET episode_key = $2,
             inject_nonce = COALESCE($3, inject_nonce)
         WHERE origin_id = $1 AND admission = 'admitting'
           AND episode_key IS NULL AND queued_message_id IS NULL",
    )
    .bind(origin_id)
    .bind(episode)
    .bind(nonce)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// Before a queue write: the delivering admission names the queue entry it becomes.
pub(crate) async fn bind_queued(
    pool: &PgPool,
    origin_id: &str,
    queued_message_id: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE imsg_reply_obligations SET queued_message_id = $2
         WHERE origin_id = $1 AND admission = 'admitting'
           AND episode_key IS NULL AND queued_message_id IS NULL",
    )
    .bind(origin_id)
    .bind(queued_message_id)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// A start refused before any effect gives its candidate up, so no turn ever claims it.
pub(crate) async fn release_episode(
    pool: &PgPool,
    origin_id: &str,
    episode: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE imsg_reply_obligations SET episode_key = NULL
         WHERE origin_id = $1 AND admission = 'admitting' AND episode_key = $2
           AND inject_nonce IS NULL AND terminal_seen_at IS NULL",
    )
    .bind(origin_id)
    .bind(episode)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// A paste the pane refused leaves its busy episode: the GUID waits as queue entry `queued`, and
/// no end of that episode can count for it. The nonce stays for diagnosis.
pub(crate) async fn hand_back_busy(
    pool: &PgPool,
    origin_id: &str,
    (episode, nonce): (&str, &str),
    queued_message_id: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE imsg_reply_obligations SET episode_key = NULL,
             queued_message_id = $4, terminal_seen_at = NULL
         WHERE origin_id = $1 AND admission = 'admitting'
           AND episode_key = $2 AND inject_nonce = $3 AND queued_message_id IS NULL",
    )
    .bind(origin_id)
    .bind(episode)
    .bind(nonce)
    .bind(queued_message_id)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// When a local turn actually claims queue entry `queued`, its GUIDs join that turn's episode.
pub async fn bind_consumed(
    pool: &PgPool,
    channel_id: &str,
    queued_message_id: &str,
    episode: &str,
) -> Result<u64, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE imsg_reply_obligations SET episode_key = $3, terminal_seen_at = NULL
         WHERE channel_id = $1 AND queued_message_id = $2 AND episode_key IS NULL
           AND admission <> 'refused'",
    )
    .bind(channel_id)
    .bind(queued_message_id)
    .bind(episode)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected())
}

const CLAIM_SQL: &str = r#"
WITH slot AS (
    UPDATE imsg_reply_obligations AS o SET active_send_action = $2
    WHERE o.origin_id = $1 AND o.resolution = 'open'
      AND o.active_send_action IS NULL
      AND (
        ($3 = 'final' AND o.admission IN ('admitting', 'admitted'))
        OR ($3 = 'progress' AND o.admission = 'admitted')
        OR ($3 = 'fallback' AND o.admission = 'admitted'
            AND o.episode_key IS NOT NULL AND o.terminal_seen_at IS NOT NULL
            AND (o.admission_kind <> 'unconfirmed' OR o.delivery_confirmed IS TRUE)
            AND NOT EXISTS (
                SELECT 1 FROM imsg_reply_actions p
                WHERE p.origin_id = o.origin_id AND p.purpose = 'progress'
                  AND p.status = 'sent' AND p.reply_episode_key = o.episode_key))
        OR ($3 = 'expiry' AND o.admission = 'admitted' AND o.expires_at <= NOW())
      )
      AND NOT EXISTS (
        SELECT 1 FROM imsg_reply_actions a WHERE a.origin_id = o.origin_id
          AND a.purpose IN ('progress', 'final', 'fallback', 'expiry')
          AND a.status IN ('claimed', 'unknown'))
    RETURNING o.origin_id
)
INSERT INTO imsg_reply_actions (id, origin_id, purpose, action_key, reply_episode_key)
SELECT $2, slot.origin_id, $3, $4, $5 FROM slot
ON CONFLICT DO NOTHING RETURNING id
"#;

const MIRROR_CLAIM_SQL: &str = r#"
INSERT INTO imsg_reply_actions (id, origin_id, purpose, action_key, reply_episode_key)
SELECT $2, origin_id, $3, $4, $5 FROM imsg_reply_obligations
WHERE origin_id = $1 AND admission = 'admitted'
ON CONFLICT DO NOTHING RETURNING id
"#;

/// Only a committed `Some(id)` grants a new send; `None` never authorizes a retry.
pub async fn claim_action(
    pool: &PgPool,
    claim: &ClaimAction<'_>,
) -> Result<Option<Uuid>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let sql = if claim.purpose == Purpose::Mirror {
        MIRROR_CLAIM_SQL
    } else {
        CLAIM_SQL
    };
    let id = sqlx::query_scalar(sql)
        .bind(claim.origin_id)
        .bind(claim.id)
        .bind(claim.purpose.as_str())
        .bind(claim.action_key)
        .bind(claim.reply_episode_key)
        .fetch_optional(&mut *tx)
        .await?;
    if id.is_some() {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(id)
}

/// Updates the action and its obligation atomically; unknown keeps the external send slot.
pub async fn ack_action(pool: &PgPool, ack: &ActionAck<'_>) -> Result<bool, sqlx::Error> {
    if ack.outcome == AckOutcome::Sent && ack.receipt.as_ref().is_none_or(Value::is_null) {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT o.resolution, o.active_send_action, a.purpose, a.status,
                a.reply_episode_key, a.receipt
         FROM imsg_reply_obligations o JOIN imsg_reply_actions a
           ON a.origin_id = o.origin_id AND a.id = $2
         WHERE o.origin_id = $1 FOR UPDATE OF o",
    )
    .bind(ack.origin_id)
    .bind(ack.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(false);
    };
    let status: String = row.try_get("status")?;
    if status == "sent" || status == "not_sent" {
        let identical = status == ack.outcome.as_str()
            && row.try_get::<Option<Value>, _>("receipt")? == ack.receipt;
        tx.rollback().await?;
        return Ok(identical);
    }
    let purpose: String = row.try_get("purpose")?;
    let external = purpose != "mirror";
    if external
        && (row.try_get::<String, _>("resolution")? != "open"
            || row.try_get::<Option<Uuid>, _>("active_send_action")? != Some(ack.id))
    {
        tx.rollback().await?;
        return Ok(false);
    }
    let changed = sqlx::query(
        "UPDATE imsg_reply_actions SET status = $3, receipt = $4,
             sent_at = CASE WHEN $3 = 'sent' THEN NOW() ELSE NULL END
         WHERE origin_id = $1 AND id = $2 AND status IN ('claimed', 'unknown')",
    )
    .bind(ack.origin_id)
    .bind(ack.id)
    .bind(ack.outcome.as_str())
    .bind(&ack.receipt)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    if external {
        let episode: Option<String> = row.try_get("reply_episode_key")?;
        let updated = sqlx::query(
            "UPDATE imsg_reply_obligations SET
                 resolution = CASE
                     WHEN $3 = 'sent' AND $5 = 'final' THEN 'replied'
                     WHEN $3 = 'sent' AND $5 = 'expiry' THEN 'expired'
                     ELSE resolution END,
                 active_send_action = CASE WHEN $3 = 'unknown' THEN active_send_action ELSE NULL END,
                 last_reply_episode = CASE WHEN $3 = 'sent' AND $5 = 'progress' THEN $4 ELSE last_reply_episode END,
                 last_reply_at = CASE WHEN $3 = 'sent' AND $5 = 'progress' THEN NOW() ELSE last_reply_at END,
                 closed_at = CASE WHEN $3 = 'sent' AND $5 IN ('final', 'expiry') THEN NOW() ELSE closed_at END
             WHERE origin_id = $1 AND active_send_action = $2 AND resolution = 'open'",
        )
        .bind(ack.origin_id)
        .bind(ack.id)
        .bind(ack.outcome.as_str())
        .bind(episode)
        .bind(purpose)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
#[path = "imsg_reply_tests.rs"]
mod tests;
