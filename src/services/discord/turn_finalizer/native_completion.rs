//! A native-owned turn's completion records, read from its own proven run instead of the bridge
//! body: the transcript row, the channel session selector, the scheduled outcome and work dispatch.
use super::*;
use crate::db::session_transcripts::{PersistSessionTranscript, SessionTranscriptEvent};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::scheduled_messages::turn_completion::{
    ScheduledTurnOutcome, classify, terminal_error_event,
};
use crate::services::tui_input::human_frame::history::v2::Source;
use crate::services::tui_input::human_frame::history::v2::native::{Disposition, Settlement};
use crate::services::tui_o::writer::input_facts::native::Evidence;
use poise::serenity_prelude::MessageId;
use std::io::{Read, Seek, SeekFrom};

#[cfg(any(test, feature = "test-support"))]
#[path = "native_completion_hook_tests.rs"]
pub(in crate::services::discord) mod hook_tests;

/// Records an executed primary's completion once its result is durable and before its row CAS.
/// A missing or replaced row, or an unreadable run, ends the step without records.
pub(in crate::services::discord) async fn record(
    shared: &Arc<SharedData>,
    settlement: &Settlement,
    evidence: &Evidence,
    key: TurnKey,
    captured: Option<&InflightTurnState>,
    actor: Option<&CancelToken>,
) {
    let (Some(source), Some(captured)) = (evidence.ticket.source.as_ref(), captured) else {
        return;
    };
    if evidence.ticket.owning_episode.is_none() || evidence.disposition != Disposition::Executed {
        return;
    }
    #[cfg(any(test, feature = "test-support"))]
    let _registry_lock = hook_tests::lock_registry();
    if settlement.ticket != evidence.ticket
        || settlement.cleanup_complete
        || !settlement
            .outcome
            .as_ref()
            .is_some_and(|saved| saved.disposition == Disposition::Executed)
    {
        return;
    }
    let row = match crate::services::discord::inflight::load_inflight_state_read_only_result(
        &ProviderKind::Claude,
        key.channel_id.get(),
    ) {
        Ok(Some(row)) => row,
        _ => return,
    };
    // Only the live row of this exact turn; a stale capture never revives a cleared one.
    if !super::super::NativeCleanup::matches_row(&row, key, source)
        || row.started_at != captured.started_at
        || row.turn_start_offset != captured.turn_start_offset
        || row.user_msg_id == 0
    {
        return;
    }
    match read_run(evidence, source) {
        Ok((body, events)) => write(shared, &row, &source.session_id, &body, &events, actor).await,
        Err(error) => tracing::warn!(
            channel_id = row.channel_id,
            user_msg_id = row.user_msg_id,
            %error,
            "native completion run unreadable; its records are skipped"
        ),
    }
}

/// [`record`] for a live result before its actor drive.
pub(in crate::services::discord::turn_finalizer) async fn record_live(
    shared: &Arc<SharedData>,
    settlement: &Settlement,
    work: &super::super::NativeCleanup,
) {
    if let Some((key, actor)) = &work.admission.cleanup {
        record(
            shared,
            settlement,
            &work.admission.evidence,
            *key,
            work.row.as_ref(),
            Some(actor),
        )
        .await;
    }
}

/// The body and events of the run alone: from its opener (never an earlier enqueue) to its closer.
fn read_run(
    evidence: &Evidence,
    source: &Source,
) -> Result<(String, Vec<SessionTranscriptEvent>), String> {
    let (start, end) = evidence.range;
    let start = evidence
        .run
        .as_ref()
        .map_or(start, |run| run.start.max(start));
    let mut file = std::fs::File::open(&source.transcript).map_err(|e| e.to_string())?;
    if super::native_terminal::stamp(source, &file)?.len < end || start > end {
        return Err("native completion range is not in the source".into());
    }
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    (&file)
        .take(end - start)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 != end - start {
        return Err("native completion range read short".into());
    }
    let mut buffer = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    // A raw closer is the file's unterminated last record.
    if !buffer.ends_with('\n') {
        buffer.push('\n');
    }
    use crate::services::discord::tmux::tmux_output_stream::{
        WatcherToolState, process_watcher_lines,
    };
    let mut state = crate::services::session_backend::StreamLineState::new();
    let mut tools = WatcherToolState::new();
    let mut body = String::new();
    while !buffer.is_empty() {
        let before = buffer.len();
        process_watcher_lines(&mut buffer, &mut state, &mut body, &mut tools);
        // A follower's prompt joined this same run; the run's answer continues past it.
        if buffer.len() == before {
            match buffer.find('\n') {
                Some(end) => drop(buffer.drain(..=end)),
                None => break,
            }
        }
    }
    Ok((body, tools.transcript_events))
}

/// The run's own session becomes the channel's stored and cached provider selector, as a bridge
/// turn end leaves it; a channel cleared since the turn began stays empty.
async fn record_selector(shared: &Arc<SharedData>, row: &InflightTurnState, session: &str) {
    // A pane that already left this session keeps the one it runs now.
    let rotated = row
        .tmux_session_name
        .as_deref()
        .and_then(crate::services::tui_prompt_dedupe::claude_session_rotation_for_tmux)
        .filter(|rotation| rotation.left_session_ids.iter().any(|left| left == session))
        .map(|rotation| rotation.new_session_id);
    let session = rotated.as_deref().unwrap_or(session);
    let channel = ChannelId::new(row.channel_id);
    let live = shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .map(|live| live.cleared);
    if live == Some(true) {
        return;
    }
    if let (Some(pool), Some(key)) = (shared.pg_pool.as_ref(), row.session_key.as_deref()) {
        let started = *MessageId::new(row.user_msg_id).created_at();
        let store = crate::db::dispatched_sessions::rotation_selector::store_provider_selector_pg;
        match store(pool, key, session, &row.channel_id.to_string(), started).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(channel_id = row.channel_id, %error, "native selector not stored")
            }
        }
    }
    let mut data = shared.core.lock().await;
    if let Some(live) = data.sessions.get_mut(&channel).filter(|live| !live.cleared) {
        live.restore_provider_session(Some(session.to_string()));
    }
}

async fn write(
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
    session: &str,
    body: &str,
    events: &[SessionTranscriptEvent],
    actor: Option<&CancelToken>,
) {
    let pool = shared.pg_pool.as_ref();
    let channel = ChannelId::new(row.channel_id);
    let turn_id = format!("discord:{}:{}", row.channel_id, row.user_msg_id);
    let answered = !body.trim().is_empty();
    let (isolated, routine) = crate::services::discord::turn_bridge::native_completion_scope(
        shared, channel, &turn_id, row,
    )
    .await;
    let isolated_run = row.routine_presentation.isolated_run();
    if answered && (routine || !isolated) {
        let binding = crate::services::discord::settings::resolve_role_binding(
            channel,
            row.channel_name.as_deref(),
        );
        let channel_text = row.channel_id.to_string();
        let persisted = crate::db::session_transcripts::persist_turn_db_in_context(
            pool,
            PersistSessionTranscript {
                turn_id: &turn_id,
                session_key: row.session_key.as_deref(),
                channel_id: Some(channel_text.as_str()),
                agent_id: binding.as_ref().map(|binding| binding.role_id.as_str()),
                provider: Some(ProviderKind::Claude.as_str()),
                dispatch_id: row.dispatch_id.as_deref(),
                user_message: &row.user_text,
                assistant_message: body,
                events,
                duration_ms: crate::services::discord::tmux::inflight_duration_ms(Some(
                    row.started_at.as_str(),
                )),
                turn_started_at_millis:
                    crate::db::session_transcripts::discord_message_started_at_millis(Some(
                        MessageId::new(row.user_msg_id),
                    )),
            },
            None,
            !isolated_run,
        )
        .await;
        if let Err(error) = persisted {
            tracing::warn!(%turn_id, %error, "native completion transcript not persisted");
        }
    }
    let dispatch = row.dispatch_id.as_deref().filter(|_| answered);
    let owns_channel = !isolated && !isolated_run;
    let unprotected = row.replay_receipt_id.is_none() && !row.replay_rerun_blocked();
    let ordinary = if !row.scheduled.may_settle() && dispatch.is_none() {
        // No outcome is classified here, so only an unprotected turn's session is recorded.
        owns_channel && unprotected && actor.and_then(|actor| actor.replay_attempt()).is_none()
    } else {
        // Only a live token can classify a protected outcome as ordinary.
        match actor {
            Some(actor) => {
                !crate::services::discord::turn_bridge::should_suppress_headless_delivery_for_cancel(
                    Some(actor),
                ) && crate::services::discord::replay_policy::ordinary_outcome(pool, actor, row)
                    .await
            }
            None => unprotected,
        }
    };
    if !ordinary {
        return;
    }
    if owns_channel {
        record_selector(shared, row, session).await;
    }
    if row.scheduled.may_settle() {
        let outcome = if answered {
            // Native terminal evidence has no provider failure flag.
            classify(body, terminal_error_event(events), false)
        } else {
            Some(ScheduledTurnOutcome::EmptyResponse)
        };
        crate::services::discord::scheduled_turn::settle(pool, row, outcome).await;
    }
    if dispatch.is_some() {
        crate::services::discord::turn_bridge::complete_work_dispatch_on_turn_end(
            shared,
            dispatch,
            row.worktree_path.as_deref(),
            Some(body),
        )
        .await;
    }
}
