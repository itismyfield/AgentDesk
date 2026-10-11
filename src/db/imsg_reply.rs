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
