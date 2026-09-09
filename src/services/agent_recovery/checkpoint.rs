use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

use crate::utils::redact::{contains_registered_secret, redact_known_secrets};

pub const DEFAULT_READ_EVENT_LIMIT: usize = 8;
pub const READ_BYTE_CAP: usize = 24 * 1024;
pub const LAST_USER_MESSAGE_MAX: usize = 4000;
pub const DEFAULT_MAX_CHECKPOINT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointEventKind {
    OwnerProgress,
    Stall,
    FallbackProgress,
    Complete,
    Restore,
}

impl CheckpointEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerProgress => "owner_progress",
            Self::Stall => "stall",
            Self::FallbackProgress => "fallback_progress",
            Self::Complete => "complete",
            Self::Restore => "restore",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner_progress" => Some(Self::OwnerProgress),
            "stall" => Some(Self::Stall),
            "fallback_progress" => Some(Self::FallbackProgress),
            "complete" => Some(Self::Complete),
            "restore" => Some(Self::Restore),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRecoveryStatus {
    Owner,
    TakeoverPending,
    FallbackRunning,
    FallbackDone,
    RestorePending,
    Restored,
    Aborted,
}

impl ChannelRecoveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::TakeoverPending => "takeover_pending",
            Self::FallbackRunning => "fallback_running",
            Self::FallbackDone => "fallback_done",
            Self::RestorePending => "restore_pending",
            Self::Restored => "restored",
            Self::Aborted => "aborted",
        }
    }

    pub fn lock_held(self) -> bool {
        matches!(
            self,
            Self::TakeoverPending
                | Self::FallbackRunning
                | Self::FallbackDone
                | Self::RestorePending
        )
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "takeover_pending" => Some(Self::TakeoverPending),
            "fallback_running" => Some(Self::FallbackRunning),
            "fallback_done" => Some(Self::FallbackDone),
            "restore_pending" => Some(Self::RestorePending),
            "restored" => Some(Self::Restored),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPayload {
    pub schema_version: u32,
    pub identity_label: String,
    pub goal: String,
    pub progress: String,
    pub decisions: String,
    pub files: Vec<String>,
    pub next: String,
    pub last_user_message: String,
}

impl CheckpointPayload {
    pub fn compact(
        identity_label: impl Into<String>,
        goal: impl Into<String>,
        progress: impl Into<String>,
        decisions: impl Into<String>,
        files: Vec<String>,
        next: impl Into<String>,
        last_user_message: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            identity_label: identity_label.into(),
            goal: goal.into(),
            progress: progress.into(),
            decisions: decisions.into(),
            files,
            next: next.into(),
            last_user_message: truncate_chars(&last_user_message.into(), LAST_USER_MESSAGE_MAX),
        }
    }

    fn redact(&self) -> Self {
        Self {
            schema_version: self.schema_version,
            identity_label: redact_known_secrets(&self.identity_label),
            goal: redact_known_secrets(&self.goal),
            progress: redact_known_secrets(&self.progress),
            decisions: redact_known_secrets(&self.decisions),
            files: self
                .files
                .iter()
                .map(|path| redact_known_secrets(path))
                .collect(),
            next: redact_known_secrets(&self.next),
            last_user_message: redact_known_secrets(&self.last_user_message),
        }
    }

    pub fn five_section_text(&self) -> String {
        let files = if self.files.is_empty() {
            String::new()
        } else {
            self.files.join(", ")
        };
        format!(
            "Goal: {}\nProgress: {}\nDecisions: {}\nFiles: {}\nNext: {}",
            self.goal, self.progress, self.decisions, files, self.next
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointEvent {
    pub id: String,
    pub channel_id: String,
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub writer_agent_id: String,
    pub kind: CheckpointEventKind,
    pub payload: CheckpointPayload,
    pub payload_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelState {
    pub channel_id: String,
    pub status: ChannelRecoveryStatus,
    pub owner_agent_id: String,
    pub fallback_agent_id: String,
    pub active_writer_agent_id: String,
    pub workspace: String,
    pub primary_turn_id: Option<String>,
    pub next_seq: i64,
    /// Monotonic fencing token. A writer from an older takeover must never
    /// mutate a later owner/fallback lease.
    pub generation: i64,
    /// Immutable launch context for a live lease, independent of config reloads.
    pub context: Option<super::context::RecoveryContext>,
}

impl ChannelState {
    pub fn lock_held(&self) -> bool {
        self.status.lock_held()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryLease {
    pub channel_id: String,
    pub generation: i64,
    pub active_writer_agent_id: String,
}

impl RecoveryLease {
    pub fn from_state(state: &ChannelState) -> Self {
        Self {
            channel_id: state.channel_id.clone(),
            generation: state.generation,
            active_writer_agent_id: state.active_writer_agent_id.clone(),
        }
    }
}

/// Preconditions evaluated while the state row is locked. Keeping them with
/// the WAL append is what prevents a stale process from reopening a channel.
#[derive(Clone, Debug)]
pub struct RecoveryTransition<'a> {
    pub expected_generation: i64,
    pub expected_next_seq: i64,
    pub expected_writer_agent_id: Option<&'a str>,
    pub allowed_statuses: &'a [ChannelRecoveryStatus],
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryStoreError {
    #[error("recovery lease conflict: {0}")]
    Conflict(String),
    #[error("recovery store database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Atomically appends a recovery WAL batch and advances the fenced channel
/// state. Callers must not start a process or reopen intake until this returns
/// its lease successfully.
pub async fn commit_recovery_transition(
    pool: &PgPool,
    state: &ChannelState,
    events: &[CheckpointEvent],
    transition: RecoveryTransition<'_>,
) -> Result<RecoveryLease, RecoveryStoreError> {
    if events.is_empty() && state.status != ChannelRecoveryStatus::Aborted {
        return Err(RecoveryStoreError::Conflict(
            "a recovery transition requires at least one WAL event".to_string(),
        ));
    }
    if transition.expected_next_seq < 0
        || state.next_seq != transition.expected_next_seq + events.len() as i64
        || events.iter().enumerate().any(|(index, event)| {
            event.channel_id != state.channel_id
                || event.seq != transition.expected_next_seq + index as i64 + 1
        })
    {
        return Err(RecoveryStoreError::Conflict(
            "recovery transition contains an invalid WAL event".to_string(),
        ));
    }
    if state.generation < transition.expected_generation
        || state.generation > transition.expected_generation + 1
    {
        return Err(RecoveryStoreError::Conflict(format!(
            "transition regresses generation from {} to {}",
            transition.expected_generation, state.generation
        )));
    }

    let mut tx = pool.begin().await?;
    lock_admission(&mut tx, &state.channel_id).await?;
    let current = load_locked_state_for_transition(&mut tx, &state.channel_id).await?;
    match current {
        Some((status, generation, writer, next_seq)) => {
            if generation != transition.expected_generation {
                return Err(RecoveryStoreError::Conflict(format!(
                    "expected generation {}, found {generation}",
                    transition.expected_generation
                )));
            }
            if !transition.allowed_statuses.contains(&status) {
                return Err(RecoveryStoreError::Conflict(format!(
                    "status '{}' is not eligible for this transition",
                    status.as_str()
                )));
            }
            if next_seq != transition.expected_next_seq {
                return Err(RecoveryStoreError::Conflict(
                    "recovery WAL frontier changed while staging the transition".to_string(),
                ));
            }
            if writer != state.active_writer_agent_id && state.generation == generation {
                return Err(RecoveryStoreError::Conflict(
                    "changing the active writer requires a new generation".to_string(),
                ));
            }
            if transition
                .expected_writer_agent_id
                .is_some_and(|expected| expected != writer)
            {
                return Err(RecoveryStoreError::Conflict(
                    "active writer does not own the recovery lease".to_string(),
                ));
            }
            write_state_in_transaction(&mut tx, state).await?;
        }
        None => {
            if transition.expected_generation != 0 || transition.expected_next_seq != 0 {
                return Err(RecoveryStoreError::Conflict(
                    "cannot advance a missing recovery lease".to_string(),
                ));
            }
            insert_state_in_transaction(&mut tx, state).await?;
        }
    }
    for event in events {
        insert_event_in_transaction(&mut tx, event).await?;
    }
    tx.commit().await?;
    Ok(RecoveryLease::from_state(state))
}

async fn load_locked_state_for_transition(
    tx: &mut Transaction<'_, Postgres>,
    channel_id: &str,
) -> Result<Option<(ChannelRecoveryStatus, i64, String, i64)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64, String, i64)>(
        "SELECT status, generation, active_writer_agent_id, next_seq
           FROM agent_recovery_channel_state
          WHERE channel_id = $1
          FOR UPDATE",
    )
    .bind(channel_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.and_then(|(status, generation, writer, next_seq)| {
        ChannelRecoveryStatus::parse(&status).map(|status| (status, generation, writer, next_seq))
    }))
}

async fn insert_state_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    state: &ChannelState,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_recovery_channel_state (
             channel_id, status, owner_agent_id, fallback_agent_id,
             active_writer_agent_id, workspace, primary_turn_id, next_seq, generation, recovery_context, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())",
    )
    .bind(&state.channel_id)
    .bind(state.status.as_str())
    .bind(&state.owner_agent_id)
    .bind(&state.fallback_agent_id)
    .bind(&state.active_writer_agent_id)
    .bind(&state.workspace)
    .bind(state.primary_turn_id.as_deref())
    .bind(state.next_seq)
    .bind(state.generation)
    .bind(state.context.as_ref().map(sqlx::types::Json))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn write_state_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    state: &ChannelState,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE agent_recovery_channel_state
            SET status = $2,
                owner_agent_id = $3,
                fallback_agent_id = $4,
                active_writer_agent_id = $5,
                workspace = $6,
                primary_turn_id = $7,
                next_seq = $8,
                generation = $9,
                recovery_context = $10,
                updated_at = NOW()
          WHERE channel_id = $1",
    )
    .bind(&state.channel_id)
    .bind(state.status.as_str())
    .bind(&state.owner_agent_id)
    .bind(&state.fallback_agent_id)
    .bind(&state.active_writer_agent_id)
    .bind(&state.workspace)
    .bind(state.primary_turn_id.as_deref())
    .bind(state.next_seq)
    .bind(state.generation)
    .bind(state.context.as_ref().map(sqlx::types::Json))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_event_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    event: &CheckpointEvent,
) -> Result<(), sqlx::Error> {
    let payload = serde_json::to_value(&event.payload).unwrap_or(Value::Null);
    sqlx::query(
        "INSERT INTO agent_recovery_checkpoint_events (
             id, channel_id, seq, at, writer_agent_id, kind, payload, payload_bytes
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&event.id)
    .bind(&event.channel_id)
    .bind(event.seq)
    .bind(event.at)
    .bind(&event.writer_agent_id)
    .bind(event.kind.as_str())
    .bind(payload)
    .bind(event.payload_bytes as i32)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    RedactFailed,
    TooLarge { bytes: usize, max_bytes: usize },
    Serialize(String),
}

impl CheckpointError {
    pub fn message(&self) -> String {
        match self {
            Self::RedactFailed => "recovery checkpoint redact failed; write refused".to_string(),
            Self::TooLarge { bytes, max_bytes } => {
                format!(
                    "recovery checkpoint is too large: {bytes} bytes exceeds max_checkpoint_bytes {max_bytes}"
                )
            }
            Self::Serialize(error) => format!("recovery checkpoint serialize failed: {error}"),
        }
    }
}

pub fn prepare_event(
    channel_id: &str,
    seq: i64,
    writer_agent_id: &str,
    kind: CheckpointEventKind,
    payload: CheckpointPayload,
    max_checkpoint_bytes: usize,
) -> Result<CheckpointEvent, CheckpointError> {
    let redacted = payload.redact();
    let serialized = serde_json::to_vec(&redacted)
        .map_err(|error| CheckpointError::Serialize(error.to_string()))?;
    if serialized.len() > max_checkpoint_bytes.max(1) {
        return Err(CheckpointError::TooLarge {
            bytes: serialized.len(),
            max_bytes: max_checkpoint_bytes.max(1),
        });
    }
    let joined = String::from_utf8_lossy(&serialized);
    // Field-level redact already ran ASSIGNMENT_RE. Re-running it on JSON
    // treats `token=***"` as a new assignment and mutates quotes, so fail
    // closed only when a registered secret or PEM still survives.
    if contains_registered_secret(&joined) || contains_unredacted_secret_pattern(&joined) {
        return Err(CheckpointError::RedactFailed);
    }
    Ok(CheckpointEvent {
        id: format!("arc_{}", uuid::Uuid::new_v4().simple()),
        channel_id: channel_id.to_string(),
        seq,
        at: Utc::now(),
        writer_agent_id: writer_agent_id.to_string(),
        kind,
        payload: redacted,
        payload_bytes: serialized.len(),
    })
}

pub fn last_n_events(
    events: &[CheckpointEvent],
    limit: usize,
    byte_cap: usize,
) -> Vec<CheckpointEvent> {
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    for event in events.iter().rev() {
        if selected.len() >= limit.max(1) {
            break;
        }
        let next_bytes = bytes.saturating_add(event.payload_bytes);
        if !selected.is_empty() && next_bytes > byte_cap {
            break;
        }
        bytes = next_bytes;
        selected.push(event.clone());
    }
    selected.reverse();
    selected
}

pub fn payload_from_json(value: &Value) -> Result<CheckpointPayload, CheckpointError> {
    serde_json::from_value(value.clone())
        .map_err(|error| CheckpointError::Serialize(error.to_string()))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

fn contains_unredacted_secret_pattern(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    upper.contains("BEGIN PRIVATE KEY") || upper.contains("BEGIN RSA PRIVATE KEY")
}

#[derive(sqlx::FromRow)]
struct RecoveryStateRow {
    channel_id: String,
    status: String,
    owner_agent_id: String,
    fallback_agent_id: String,
    active_writer_agent_id: String,
    workspace: String,
    primary_turn_id: Option<String>,
    next_seq: i64,
    generation: i64,
    recovery_context: Option<sqlx::types::Json<super::context::RecoveryContext>>,
}

impl RecoveryStateRow {
    fn decode(self) -> Result<ChannelState, sqlx::Error> {
        let status = ChannelRecoveryStatus::parse(&self.status).ok_or_else(|| {
            sqlx::Error::Decode(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown recovery status",
            )))
        })?;
        Ok(ChannelState {
            channel_id: self.channel_id,
            status,
            owner_agent_id: self.owner_agent_id,
            fallback_agent_id: self.fallback_agent_id,
            active_writer_agent_id: self.active_writer_agent_id,
            workspace: self.workspace,
            primary_turn_id: self.primary_turn_id,
            next_seq: self.next_seq,
            generation: self.generation,
            context: self.recovery_context.map(|context| context.0),
        })
    }
}

pub async fn load_channel_state(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<ChannelState>, sqlx::Error> {
    sqlx::query_as::<_, RecoveryStateRow>(
        "SELECT channel_id, status, owner_agent_id, fallback_agent_id,
                active_writer_agent_id, workspace, primary_turn_id, next_seq, generation, recovery_context
           FROM agent_recovery_channel_state WHERE channel_id = $1",
    ).bind(channel_id).fetch_optional(pool).await?.map(RecoveryStateRow::decode).transpose()
}

/// Serialize durable ownership changes with mailbox reservation, including a
/// channel whose state row has not yet been inserted.
pub(super) async fn lock_admission(
    tx: &mut Transaction<'_, Postgres>,
    channel: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(1894, hashtext($1))")
        .bind(channel)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(super) async fn load_channel_state_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    channel: &str,
) -> Result<Option<ChannelState>, sqlx::Error> {
    sqlx::query_as::<_, RecoveryStateRow>(
        "SELECT channel_id, status, owner_agent_id, fallback_agent_id,
                active_writer_agent_id, workspace, primary_turn_id, next_seq, generation, recovery_context
           FROM agent_recovery_channel_state WHERE channel_id = $1",
    ).bind(channel).fetch_optional(&mut **tx).await?.map(RecoveryStateRow::decode).transpose()
}

pub async fn load_channel_states(pool: &PgPool) -> Result<Vec<ChannelState>, sqlx::Error> {
    sqlx::query_as::<_, RecoveryStateRow>(
        "SELECT channel_id, status, owner_agent_id, fallback_agent_id,
                active_writer_agent_id, workspace, primary_turn_id, next_seq, generation, recovery_context
           FROM agent_recovery_channel_state",
    ).fetch_all(pool).await?.into_iter().map(RecoveryStateRow::decode).collect()
}

pub async fn load_checkpoint_events(
    pool: &sqlx::PgPool,
    channel_id: &str,
    limit: i64,
) -> Result<Vec<CheckpointEvent>, sqlx::Error> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            i64,
            DateTime<Utc>,
            String,
            String,
            Value,
            i32,
        ),
    >(
        "SELECT id, channel_id, seq, at, writer_agent_id, kind, payload, payload_bytes
           FROM agent_recovery_checkpoint_events
          WHERE channel_id = $1
          ORDER BY seq DESC
          LIMIT $2",
    )
    .bind(channel_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut events = Vec::new();
    for (id, channel_id, seq, at, writer_agent_id, kind, payload, payload_bytes) in rows {
        let Some(kind) = CheckpointEventKind::parse(&kind) else {
            continue;
        };
        let Ok(payload) = payload_from_json(&payload) else {
            continue;
        };
        events.push(CheckpointEvent {
            id,
            channel_id,
            seq,
            at,
            writer_agent_id,
            kind,
            payload,
            payload_bytes: payload_bytes.max(0) as usize,
        });
    }
    events.reverse();
    Ok(events)
}
