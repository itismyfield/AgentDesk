use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use super::turn_completion::{self, ScheduledTurnOutcome};
use crate::db::scheduled_messages as db;
#[cfg(test)]
use crate::db::session_transcripts::SessionTranscriptEvent;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnEvidence {
    Delivered,
    TerminalFailure(String),
}

#[cfg(test)]
pub(super) fn transcript_delivery_evidence(
    assistant_message: &str,
    events: &[SessionTranscriptEvent],
) -> TurnEvidence {
    transcript_delivery_evidence_with_terminal_error(
        assistant_message,
        turn_completion::terminal_error_event(events),
    )
}

fn transcript_delivery_evidence_with_terminal_error(
    assistant_message: &str,
    has_terminal_error_event: bool,
) -> TurnEvidence {
    outcome_evidence(
        turn_completion::classify(assistant_message, has_terminal_error_event, false).unwrap_or(
            if has_terminal_error_event {
                ScheduledTurnOutcome::ProviderError
            } else {
                ScheduledTurnOutcome::Delivered
            },
        ),
    )
}

fn outcome_evidence(outcome: ScheduledTurnOutcome) -> TurnEvidence {
    match outcome.failure_reason() {
        None => TurnEvidence::Delivered,
        Some(reason) => TurnEvidence::TerminalFailure(reason.to_string()),
    }
}

/// Completion evidence: the turn's own record on this delivery row first (all an
/// isolated snapshot turn leaves), otherwise transcript evidence, same sources as
/// `RoutineAgentExecutor::find_turn_completion`: a non-empty assistant
/// transcript proves relay delivery unless its final non-`System` typed event
/// is `Error` or it uses a narrow provider-generated error-only envelope. An
/// `empty_response` terminal quality event proves the turn died without output.
/// The launch-commit lower bound includes evidence emitted before the later
/// runtime acknowledgement while excluding unrelated stale evidence.
pub(super) async fn find_turn_delivery_evidence(
    pool: &PgPool,
    delivery_id: &str,
    turn_id: &str,
    launch_committed_at: DateTime<Utc>,
) -> Result<Option<TurnEvidence>, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    find_turn_delivery_evidence_on_connection(
        &mut connection,
        delivery_id,
        turn_id,
        launch_committed_at,
    )
    .await
}

pub(super) async fn find_turn_delivery_evidence_on_connection(
    connection: &mut PgConnection,
    delivery_id: &str,
    turn_id: &str,
    launch_committed_at: DateTime<Utc>,
) -> Result<Option<TurnEvidence>, sqlx::Error> {
    let recorded: Option<String> = sqlx::query_scalar(
        "SELECT turn_outcome FROM scheduled_message_deliveries
         WHERE id = $1 AND turn_id = $2 AND turn_outcome_at >= $3",
    )
    .bind(delivery_id)
    .bind(turn_id)
    .bind(launch_committed_at)
    .fetch_optional(&mut *connection)
    .await?
    .flatten();
    if let Some(outcome) = recorded
        .as_deref()
        .and_then(ScheduledTurnOutcome::from_db_str)
    {
        return Ok(Some(outcome_evidence(outcome)));
    }

    let delivered: Option<(String, bool)> = sqlx::query_as(
        "SELECT assistant_message,
                COALESCE((
                    SELECT event ->> 'kind' = 'error'
                    FROM jsonb_array_elements(
                        CASE WHEN jsonb_typeof(events_json) = 'array'
                             THEN events_json ELSE '[]'::jsonb END
                    ) WITH ORDINALITY AS terminal_event(event, ordinal)
                    WHERE event ->> 'kind' IS DISTINCT FROM 'system'
                    ORDER BY ordinal DESC
                    LIMIT 1
                ), FALSE)
         FROM session_transcripts
         WHERE turn_id = $1
           AND created_at >= $2
           AND BTRIM(assistant_message) <> ''
         ORDER BY created_at ASC
         LIMIT 1",
    )
    .bind(turn_id)
    .bind(launch_committed_at)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some((assistant_message, has_terminal_error_event)) = delivered {
        return Ok(Some(transcript_delivery_evidence_with_terminal_error(
            &assistant_message,
            has_terminal_error_event,
        )));
    }

    let terminal: Option<String> = sqlx::query_scalar(
        "SELECT event_type::text
         FROM agent_quality_event
         WHERE correlation_id = $1
           AND source_event_id = $1
           AND created_at >= $2
           AND event_type = 'turn_error'::agent_quality_event_type
           AND payload #>> '{details,outcome}' = 'empty_response'
         LIMIT 1",
    )
    .bind(turn_id)
    .bind(launch_committed_at)
    .fetch_optional(&mut *connection)
    .await?;
    Ok(terminal.map(|_| {
        TurnEvidence::TerminalFailure("agent turn ended with an empty response".to_string())
    }))
}

pub(super) async fn poll_running_agent_deliveries(
    pool: &PgPool,
    claim_owner: &str,
    lease_secs: i64,
    limit: i64,
) -> bool {
    match db::list_running_agent_deliveries_pg(pool, claim_owner, lease_secs, limit).await {
        Ok(running) => {
            let mut transitioned = false;
            for delivery in running {
                if super::poll_agent_delivery(pool, delivery).await {
                    transitioned = true;
                }
            }
            transitioned
        }
        Err(error) => {
            tracing::warn!("[smsg] agent delivery poll failed: {error}");
            false
        }
    }
}
