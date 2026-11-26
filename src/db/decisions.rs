//! Approved Decision input events and each event's single delivery workflow.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// The input source only the Decision service uses; a request that names it proves nothing.
pub const SOURCE: &str = "decision";

/// The event an origin of the form `decision:<uuid>[:opinion:<message id>]` names.
pub fn event_of_origin(origin: &str) -> Option<Uuid> {
    let rest = origin.strip_prefix("decision:")?;
    let (event, tail) = rest.split_at_checked(36)?;
    let opinion = tail.strip_prefix(":opinion:").is_some_and(|message| {
        !message.is_empty() && message.bytes().all(|byte| byte.is_ascii_digit())
    });
    (tail.is_empty() || opinion)
        .then(|| Uuid::try_parse(event).ok())
        .flatten()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    Selection,
    Opinion,
}

/// An approved event as PG stores it. Only `load_event` builds one, so holding it is the proof
/// that the service approved this input; a source string or request metadata never is.
#[derive(Clone, Debug)]
pub struct DecisionProvenance {
    kind: EventKind,
    origin: String,
    channel_id: u64,
    actor_id: u64,
    text_sha256: String,
}

impl DecisionProvenance {
    pub(crate) fn kind(&self) -> EventKind {
        self.kind
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// The requester's logical channel; the session on it is resolved when the input is delivered.
    pub fn channel_id(&self) -> u64 {
        self.channel_id
    }

    pub(crate) fn actor_id(&self) -> u64 {
        self.actor_id
    }

    /// Whether `text` is byte for byte the text this event approved.
    pub fn approves(&self, text: &str) -> bool {
        hex::encode(Sha256::digest(text.as_bytes())) == self.text_sha256
    }
}

#[cfg(test)]
impl DecisionProvenance {
    /// An approved event for tests of the code that only accepts one; no PG row backs it.
    pub(crate) fn for_tests(kind: EventKind, actor_id: u64, text: &str) -> Self {
        let event_id = Uuid::new_v4();
        Self {
            kind,
            origin: format!("decision:{event_id}"),
            channel_id: 7,
            actor_id,
            text_sha256: hex::encode(Sha256::digest(text.as_bytes())),
        }
    }
}

pub async fn load_event(
    pool: &PgPool,
    event_id: Uuid,
) -> Result<Option<DecisionProvenance>, sqlx::Error> {
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT kind, origin, logical_channel_id, actor_user_id, text_sha256
         FROM decision_input_events WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(kind, origin, channel, actor, text_sha256)| {
        // An origin that does not name its own event is not an approval this build understands.
        (event_of_origin(&origin) == Some(event_id)).then_some(())?;
        Some(DecisionProvenance {
            kind: match kind.as_str() {
                "selection" => EventKind::Selection,
                _ => EventKind::Opinion,
            },
            origin,
            channel_id: channel.parse().ok()?,
            actor_id: actor.parse().ok()?,
            text_sha256,
        })
    }))
}

/// The workflow states. `Closing` is the fence a failure writes before its original is removed;
/// it and the three ends after it never become runnable again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptState {
    NotStarted,
    Resolving,
    NormalWait,
    Normalizing,
    MayHaveSent,
    Closing,
    Delivered,
    Failed,
    UnknownClosed,
}

impl AttemptState {
    const ALL: [Self; 9] = [
        Self::NotStarted,
        Self::Resolving,
        Self::NormalWait,
        Self::Normalizing,
        Self::MayHaveSent,
        Self::Closing,
        Self::Delivered,
        Self::Failed,
        Self::UnknownClosed,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Resolving => "resolving",
            Self::NormalWait => "normal_wait",
            Self::Normalizing => "normalizing",
            Self::MayHaveSent => "may_have_sent",
            Self::Closing => "closing",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
            Self::UnknownClosed => "unknown_closed",
        }
    }

    pub(crate) fn parse(state: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.as_str() == state)
    }
}

/// Starts the event's workflow. Only the caller whose update moved `not_started` may deliver.
pub async fn reserve_first_attempt(
    pool: &PgPool,
    event_id: Uuid,
    ticket: &str,
) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts
         SET state = 'resolving', workflow_ticket = $2, updated_at = NOW()
         WHERE event_id = $1 AND state = 'not_started'",
    )
    .bind(event_id)
    .bind(ticket)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

/// Moves the workflow from `from` to `to`. Normalizing is entered at most once per event.
pub async fn advance(
    pool: &PgPool,
    event_id: Uuid,
    (from, to): (AttemptState, AttemptState),
    turn_id: Option<&str>,
    detail: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let normalizing = to == AttemptState::Normalizing;
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts
         SET state = $3, normalization_used = normalization_used OR $4,
             attempt_turn_id = COALESCE($5, attempt_turn_id), detail = COALESCE($6, detail),
             updated_at = NOW()
         WHERE event_id = $1 AND state = $2 AND NOT (normalization_used AND $4)",
    )
    .bind(event_id)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(normalizing)
    .bind(turn_id)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

/// The journal a write is taken under: one update from a waiting row, before any key is sent.
/// A closed or already journaled event refuses it, so its original is never written.
pub async fn begin_write(pool: &PgPool, event_id: Uuid) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts SET state = 'may_have_sent', updated_at = NOW()
         WHERE event_id = $1 AND state IN ('resolving', 'normal_wait')",
    )
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

/// The closing fence: an event that journaled no write stops being runnable, with its reason.
/// The removal of its original follows, and survives a crash between the two.
pub async fn close(pool: &PgPool, event_id: Uuid, reason: &str) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts SET state = 'closing', detail = $2, updated_at = NOW()
         WHERE event_id = $1
           AND state IN ('not_started', 'resolving', 'normal_wait', 'normalizing')",
    )
    .bind(event_id)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

pub async fn state_of(pool: &PgPool, event_id: Uuid) -> Result<Option<AttemptState>, sqlx::Error> {
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM decision_delivery_attempts WHERE event_id = $1")
            .bind(event_id)
            .fetch_optional(pool)
            .await?;
    Ok(state.as_deref().and_then(AttemptState::parse))
}

/// A started workflow that has not reached its end.
#[derive(Clone, Debug)]
pub struct Unsettled {
    pub event_id: Uuid,
    pub origin: String,
    pub agent_id: String,
    pub channel_id: u64,
    pub state: AttemptState,
    /// Last written before `before`: the process that wrote it is gone.
    pub stale: bool,
}

/// The unsettled workflows of `channel`, or of every channel, oldest event first. `before` is
/// this process's start on the database clock; without one no row reads as stale.
pub async fn unsettled(
    pool: &PgPool,
    channel: Option<u64>,
    before: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<Unsettled>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String, String, String, bool)> = sqlx::query_as(
        "SELECT e.event_id, e.origin, e.requester_agent_id, e.logical_channel_id, a.state,
                COALESCE(a.updated_at < $2, FALSE)
         FROM decision_delivery_attempts a JOIN decision_input_events e USING (event_id)
         WHERE a.state IN ('resolving', 'normal_wait', 'normalizing', 'may_have_sent', 'closing')
           AND ($1::TEXT IS NULL OR e.logical_channel_id = $1)
         ORDER BY e.admission_sequence",
    )
    .bind(channel.map(|channel| channel.to_string()))
    .bind(before)
    .fetch_all(pool)
    .await?;
    let read =
        |(event_id, origin, agent_id, channel, state, stale): (_, _, _, String, String, _)| {
            Some(Unsettled {
                event_id,
                origin,
                agent_id,
                channel_id: channel.parse().ok()?,
                state: AttemptState::parse(&state)?,
                stale,
            })
        };
    Ok(rows.into_iter().filter_map(read).collect())
}

/// The database clock now: rows last written before it belong to an earlier process.
pub async fn clock(pool: &PgPool) -> Result<chrono::DateTime<chrono::Utc>, sqlx::Error> {
    sqlx::query_scalar("SELECT NOW()").fetch_one(pool).await
}

#[cfg(any(test, feature = "test-support"))]
#[path = "decisions_support_tests.rs"]
pub(crate) mod test_support;
