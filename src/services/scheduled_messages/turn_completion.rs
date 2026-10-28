//! The outcome record a completer writes onto the delivery that launched its exact
//! turn, classified by the same rules as the poller's transcript evidence.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::services::provider_error_transcript::is_strong_provider_error_transcript;

/// The normalized result of one scheduled agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScheduledTurnOutcome {
    Delivered,
    NoReply,
    ProviderError,
    EmptyResponse,
}

impl ScheduledTurnOutcome {
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::NoReply => "no_reply",
            Self::ProviderError => "provider_error",
            Self::EmptyResponse => "empty_response",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "delivered" => Some(Self::Delivered),
            "no_reply" => Some(Self::NoReply),
            "provider_error" => Some(Self::ProviderError),
            "empty_response" => Some(Self::EmptyResponse),
            _ => None,
        }
    }

    /// The poller's terminal-failure reason; `None` for a delivered answer.
    pub(crate) fn failure_reason(self) -> Option<&'static str> {
        match self {
            Self::Delivered => None,
            Self::NoReply => Some("agent turn returned NO_REPLY"),
            Self::ProviderError => Some("agent turn returned terminal provider error transcript"),
            Self::EmptyResponse => Some("agent turn ended with an empty response"),
        }
    }
}

/// Whether the final non-`System` typed event is an `Error`.
pub(crate) fn terminal_error_event(events: &[SessionTranscriptEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| event.kind != SessionTranscriptEventKind::System)
        .is_some_and(|event| event.kind == SessionTranscriptEventKind::Error)
}

/// Classify a non-empty answer. `None` means there is no answer to classify;
/// an empty response is reported explicitly by the completer.
pub(crate) fn classify(
    assistant_message: &str,
    terminal_error_event: bool,
    provider_failure: bool,
) -> Option<ScheduledTurnOutcome> {
    let trimmed = assistant_message.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(if trimmed.eq_ignore_ascii_case("NO_REPLY") {
        ScheduledTurnOutcome::NoReply
    } else if terminal_error_event
        || provider_failure
        || is_strong_provider_error_transcript(assistant_message)
    {
        ScheduledTurnOutcome::ProviderError
    } else {
        ScheduledTurnOutcome::Delivered
    })
}

/// What a record attempt proved; `NotOwned` means no running launch-committed
/// delivery owns this turn (a human turn, a canceled or settled delivery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    Recorded,
    AlreadyRecorded(ScheduledTurnOutcome),
    NotOwned,
}

impl RecordOutcome {
    pub(crate) fn is_evidence(self) -> bool {
        !matches!(self, Self::NotOwned)
    }
}

/// Write `outcome` onto the delivery that launched `turn_id`. First writer wins.
pub(crate) async fn record(
    pool: &PgPool,
    turn_id: &str,
    outcome: ScheduledTurnOutcome,
) -> Result<RecordOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET turn_outcome = $2, turn_outcome_at = NOW(), updated_at = NOW()
         WHERE turn_id = $1 AND delivery_kind = 'agent'
           AND launch_committed_at IS NOT NULL
           AND status = 'running' AND turn_outcome IS NULL",
    )
    .bind(turn_id)
    .bind(outcome.as_db_str())
    .execute(&mut *tx)
    .await?;
    let result = if updated.rows_affected() > 0 {
        RecordOutcome::Recorded
    } else {
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT turn_outcome FROM scheduled_message_deliveries
             WHERE turn_id = $1 AND delivery_kind = 'agent'
               AND launch_committed_at IS NOT NULL AND turn_outcome IS NOT NULL
             LIMIT 1",
        )
        .bind(turn_id)
        .fetch_optional(&mut *tx)
        .await?;
        existing
            .as_deref()
            .and_then(ScheduledTurnOutcome::from_db_str)
            .map_or(RecordOutcome::NotOwned, RecordOutcome::AlreadyRecorded)
    };
    tx.commit().await?;
    Ok(result)
}

/// For an unmarked (older) inflight row: whether `turn_id` was launched by
/// a snapshot reservation. `None` when no agent delivery owns the turn.
pub(crate) async fn legacy_snapshot_turn(
    pool: &PgPool,
    turn_id: &str,
) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT m.context_strategy = 'snapshot'
         FROM scheduled_message_deliveries d
         JOIN scheduled_messages m ON m.id = d.scheduled_message_id
         WHERE d.turn_id = $1 AND d.delivery_kind = 'agent'
           AND d.launch_committed_at IS NOT NULL
         LIMIT 1",
    )
    .bind(turn_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
#[path = "turn_completion_tests.rs"]
pub(crate) mod tests;
