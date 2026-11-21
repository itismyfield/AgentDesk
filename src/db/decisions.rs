//! Approved Decision input events and each event's single delivery workflow.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

/// The input source only the Decision service uses; a request that names it proves nothing.
pub(crate) const SOURCE: &str = "decision";

/// The event an origin of the form `decision:<uuid>[:opinion:<message id>]` names.
pub(crate) fn event_of_origin(origin: &str) -> Option<Uuid> {
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

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    /// The requester's logical channel; the session on it is resolved when the input is delivered.
    pub(crate) fn channel_id(&self) -> u64 {
        self.channel_id
    }

    pub(crate) fn actor_id(&self) -> u64 {
        self.actor_id
    }

    /// Whether `text` is byte for byte the text this event approved.
    pub(crate) fn approves(&self, text: &str) -> bool {
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

pub(crate) async fn load_event(
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

/// The workflow states this slice writes; settlement states arrive with their writers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptState {
    Resolving,
    NormalWait,
    Normalizing,
    MayHaveSent,
    Delivered,
    Failed,
}

impl AttemptState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Resolving => "resolving",
            Self::NormalWait => "normal_wait",
            Self::Normalizing => "normalizing",
            Self::MayHaveSent => "may_have_sent",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }
}

/// Starts the event's workflow. Only the caller whose update moved `not_started` may deliver.
pub(crate) async fn reserve_first_attempt(
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
pub(crate) async fn advance(
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
             attempt_turn_id = COALESCE($5, attempt_turn_id), detail = $6, updated_at = NOW()
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
