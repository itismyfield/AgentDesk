//! Approved Decision input events and each event's single delivery workflow.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

pub(crate) mod requests;

/// The input source only the Decision service uses; a request that names it proves nothing.
pub const SOURCE: &str = "decision";
/// The tag of every ticket an event's original is admitted under.
pub(crate) const TICKET_TAG: &str = "dcsn";

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
pub enum EventKind {
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

    pub fn actor_id(&self) -> u64 {
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

    pub fn parse(state: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.as_str() == state)
    }
}

/// The node this process runs as, named once by its runtime before it moves any row.
static HOLDER: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The holder each moved row gets; until the runtime named one, a row keeps whoever held it.
fn holder() -> Option<&'static str> {
    HOLDER.get().map(String::as_str)
}

/// One run of a node: its identity and its start on the database clock.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    holder: &'static str,
    pub(crate) boot: chrono::DateTime<chrono::Utc>,
}

impl Run {
    /// This process's run as node `instance`, started at `boot`; the first name given stays.
    pub(crate) fn of(instance: &str, boot: chrono::DateTime<chrono::Utc>) -> Self {
        let holder = HOLDER.get_or_init(|| instance.to_string()).as_str();
        Self { holder, boot }
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
         SET state = 'resolving', workflow_ticket = $2,
             holder_instance_id = COALESCE($3, holder_instance_id), updated_at = NOW()
         WHERE event_id = $1 AND state = 'not_started'",
    )
    .bind(event_id)
    .bind(ticket)
    .bind(holder())
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
             holder_instance_id = COALESCE($7, holder_instance_id), updated_at = NOW()
         WHERE event_id = $1 AND state = $2 AND NOT (normalization_used AND $4)",
    )
    .bind(event_id)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(normalizing)
    .bind(turn_id)
    .bind(detail)
    .bind(holder())
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

/// The journal a write is taken under: one update from a waiting row, before any key is sent.
/// A closed or already journaled event refuses it, so its original is never written.
pub async fn begin_write(pool: &PgPool, event_id: Uuid) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts
         SET state = 'may_have_sent', holder_instance_id = COALESCE($2, holder_instance_id),
             updated_at = NOW()
         WHERE event_id = $1 AND state IN ('resolving', 'normal_wait')",
    )
    .bind(event_id)
    .bind(holder())
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

/// The closing fence: an event that journaled no write stops being runnable, with its reason.
/// The removal of its original follows, and survives a crash between the two.
pub async fn close(pool: &PgPool, event_id: Uuid, reason: &str) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts
         SET state = 'closing', detail = $2,
             holder_instance_id = COALESCE($3, holder_instance_id), updated_at = NOW()
         WHERE event_id = $1
           AND state IN ('not_started', 'resolving', 'normal_wait', 'normalizing')",
    )
    .bind(event_id)
    .bind(reason)
    .bind(holder())
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() == 1)
}

/// Moves a row only while its holder's run is over for `run`: the same node moved it before this
/// start, or its node is no longer listed online. A live holder keeps its attempt.
pub(crate) async fn end_abandoned(
    pool: &PgPool,
    event_id: Uuid,
    (from, to): (AttemptState, AttemptState),
    detail: Option<&str>,
    run: &Run,
) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE decision_delivery_attempts a
         SET state = $3, detail = COALESCE($4, a.detail), holder_instance_id = $5,
             updated_at = NOW()
         WHERE a.event_id = $1 AND a.state = $2 AND a.holder_instance_id IS NOT NULL AND (
               (a.holder_instance_id = $5 AND a.updated_at < $6)
               OR (a.holder_instance_id <> $5 AND NOT EXISTS (
                   SELECT 1 FROM worker_nodes w
                   WHERE w.instance_id = a.holder_instance_id AND w.status = 'online')))",
    )
    .bind(event_id)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(detail)
    .bind(run.holder)
    .bind(run.boot)
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
    /// The run that last moved it is over; `end_abandoned` checks that again when it ends it.
    pub stale: bool,
}

/// The unsettled workflows of `channel`, or of every channel, oldest event first. A row reads as
/// stale only to `run`, by the rule of `end_abandoned`; its age alone never makes it so.
pub async fn unsettled(
    pool: &PgPool,
    channel: Option<u64>,
    run: Option<&Run>,
) -> Result<Vec<Unsettled>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String, String, String, bool)> = sqlx::query_as(
        "SELECT e.event_id, e.origin, e.requester_agent_id, e.logical_channel_id, a.state,
                COALESCE(a.holder_instance_id IS NOT NULL AND (
                    (a.holder_instance_id = $2 AND a.updated_at < $3)
                    OR (a.holder_instance_id <> $2 AND NOT EXISTS (
                        SELECT 1 FROM worker_nodes w
                        WHERE w.instance_id = a.holder_instance_id AND w.status = 'online'))),
                    FALSE)
         FROM decision_delivery_attempts a JOIN decision_input_events e USING (event_id)
         WHERE a.state IN ('resolving', 'normal_wait', 'normalizing', 'may_have_sent', 'closing')
           AND ($1::TEXT IS NULL OR e.logical_channel_id = $1)
         ORDER BY e.admission_sequence",
    )
    .bind(channel.map(|channel| channel.to_string()))
    .bind(run.map(|run| run.holder))
    .bind(run.map(|run| run.boot))
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

/// The origin and channel of every event that reached its end among those reserved under
/// `tickets`: what an owner still holding one of these originals has left to remove.
pub(crate) async fn closed_under(
    pool: &PgPool,
    tickets: &[String],
) -> Result<Vec<(String, u64)>, sqlx::Error> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT e.origin, e.logical_channel_id
         FROM decision_delivery_attempts a JOIN decision_input_events e USING (event_id)
         WHERE a.workflow_ticket = ANY($1)
           AND a.state IN ('delivered', 'failed', 'unknown_closed')",
    )
    .bind(tickets)
    .fetch_all(pool)
    .await?;
    let read = |(origin, channel): (String, String)| Some((origin, channel.parse().ok()?));
    Ok(rows.into_iter().filter_map(read).collect())
}

/// The database clock now: a run that starts here wrote none of the rows moved before it.
pub async fn clock(pool: &PgPool) -> Result<chrono::DateTime<chrono::Utc>, sqlx::Error> {
    sqlx::query_scalar("SELECT NOW()").fetch_one(pool).await
}

#[cfg(any(test, feature = "test-support"))]
#[path = "decisions_support_tests.rs"]
pub mod test_support;
