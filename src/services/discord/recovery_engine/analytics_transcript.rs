//! #3479 recovered-turn analytics + transcript-persistence helpers, moved
//! verbatim out of recovery_engine.rs (behavior-preserving — only visibility
//! prefixes and `super::` depth adjusted).

use super::*;

pub(super) fn extract_turn_analytics_from_output(
    output_path: &str,
    start_offset: u64,
) -> (Option<String>, Option<TurnTokenUsage>) {
    crate::services::session_backend::extract_turn_analytics_from_output(output_path, start_offset)
}

pub(super) fn recovered_turn_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

pub(super) async fn lookup_turn_finished_dispatch_kind(
    dispatch_id: Option<&str>,
) -> Option<String> {
    let dispatch_id = dispatch_id?;
    let body = super::super::internal_api::lookup_dispatch_info(dispatch_id)
        .await
        .ok()?;
    super::super::turn_bridge::classify_turn_finished_dispatch_kind(
        body.get("dispatch_context")
            .and_then(|value| value.as_str()),
        body.get("dispatch_type").and_then(|value| value.as_str()),
    )
    .map(str::to_string)
}

/// Build the transcript turn id for a recovered turn. A message-less recovery
/// turn (`user_msg_id == 0`, e.g. TUI-direct) must NOT key on `discord:<ch>:0`
/// (collides across every such turn → overwrite, Codex P2 r2) NOR purely on the
/// session (repeated message-less turns upsert-overwrite, Codex P2 r3): instead
/// append a PER-TURN discriminator — the JSONL start offset, falling back to the
/// `started_at` timestamp when no offset is recorded.
pub(super) fn recovered_transcript_turn_id(
    channel_id: u64,
    user_msg_id: u64,
    session_key: Option<&str>,
    turn_start_offset: Option<u64>,
    started_at: &str,
) -> String {
    if user_msg_id != 0 {
        return format!("discord:{channel_id}:{user_msg_id}");
    }
    // Per-turn discriminator: stable for THIS turn, distinct across turns.
    let turn_discriminator = match turn_start_offset {
        Some(offset) => format!("off{offset}"),
        None => format!("at{}", started_at.replace([' ', ':'], "-")),
    };
    match session_key {
        Some(key) => format!("discord:{channel_id}:session:{key}:{turn_discriminator}"),
        None => format!("discord:{channel_id}:recovery:{turn_discriminator}"),
    }
}

pub(super) async fn persist_recovered_transcript(
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    dispatch_id: Option<&str>,
    assistant_message: &str,
) -> bool {
    let assistant_message = assistant_message.trim();
    if assistant_message.is_empty() {
        return false;
    }

    let turn_id = recovered_transcript_turn_id(
        state.channel_id,
        state.user_msg_id,
        state.session_key.as_deref(),
        state.turn_start_offset,
        &state.started_at,
    );
    let channel_id_text = state.channel_id.to_string();
    match crate::db::session_transcripts::persist_turn_db(
        pg_pool,
        crate::db::session_transcripts::PersistSessionTranscript {
            turn_id: &turn_id,
            session_key: state.session_key.as_deref(),
            channel_id: Some(channel_id_text.as_str()),
            agent_id: None,
            provider: Some(provider.as_str()),
            dispatch_id,
            user_message: &state.user_text,
            assistant_message,
            events: &[],
            duration_ms: None,
            turn_started_at_millis: None,
        },
    )
    .await
    {
        Ok(_) => true,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ recovery: failed to persist session transcript: {e}");
            false
        }
    }
}
