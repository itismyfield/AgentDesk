use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

use crate::db::scheduled_messages as db;
use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::services::provider_error_transcript::is_strong_provider_error_transcript;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnEvidence {
    Delivered,
    TerminalFailure(String),
}

pub(super) fn transcript_delivery_evidence(
    assistant_message: &str,
    events: &[SessionTranscriptEvent],
) -> TurnEvidence {
    let has_terminal_error_event = events
        .iter()
        .rev()
        .find(|event| event.kind != SessionTranscriptEventKind::System)
        .is_some_and(|event| event.kind == SessionTranscriptEventKind::Error);
    transcript_delivery_evidence_with_terminal_error(assistant_message, has_terminal_error_event)
}

fn transcript_delivery_evidence_with_terminal_error(
    assistant_message: &str,
    has_terminal_error_event: bool,
) -> TurnEvidence {
    if assistant_message.trim().eq_ignore_ascii_case("NO_REPLY") {
        TurnEvidence::TerminalFailure("agent turn returned NO_REPLY".to_string())
    } else if has_terminal_error_event || is_strong_provider_error_transcript(assistant_message) {
        TurnEvidence::TerminalFailure(
            "agent turn returned terminal provider error transcript".to_string(),
        )
    } else {
        TurnEvidence::Delivered
    }
}

/// Transcript-based completion evidence, same sources as
/// `RoutineAgentExecutor::find_turn_completion`: a non-empty assistant
/// transcript proves relay delivery unless its final non-`System` typed event
/// is `Error` or it uses a narrow provider-generated error-only envelope. An
/// `empty_response` terminal quality event proves the turn died without output.
/// The launch-commit lower bound includes evidence emitted before the later
/// runtime acknowledgement while excluding unrelated stale evidence.
pub(super) async fn find_turn_delivery_evidence(
    pool: &PgPool,
    turn_id: &str,
    launch_committed_at: DateTime<Utc>,
) -> Result<Option<TurnEvidence>, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    find_turn_delivery_evidence_on_connection(&mut connection, turn_id, launch_committed_at).await
}

pub(super) async fn find_turn_delivery_evidence_on_connection(
    connection: &mut PgConnection,
    turn_id: &str,
    launch_committed_at: DateTime<Utc>,
) -> Result<Option<TurnEvidence>, sqlx::Error> {
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
