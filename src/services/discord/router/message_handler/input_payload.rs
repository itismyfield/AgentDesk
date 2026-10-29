//! Shared preparation leaves raw provenance and file lifetimes with the caller.
#[cfg(test)]
use super::super::response_format::retry_prompt_context::RetryPromptContext;
#[cfg(test)]
pub(in crate::services::discord) use super::intake_turn::session_preparation::selected_reply;
pub(in crate::services::discord) use super::intake_turn::session_preparation::{
    Restore as SessionRestore, restore as restore_session,
};
pub(in crate::services::discord) use super::turn_context::TurnContext;
use super::*;
use crate::services::cluster::attachment_transfer::{materialize, uploads::Upload};

pub(in crate::services::discord) fn render_turn(input: TurnContext<'_>) -> String {
    let mut chunks = Vec::new();
    if let Some(trigger) = input.trigger {
        chunks.push(trigger);
    }
    if let Some(recent) = input.recent {
        recent.append_rendered_context_to(&mut chunks);
    }
    if let Some(knowledge) = input.knowledge {
        chunks.push(knowledge.to_string());
    }
    if let Some(reply) = input.reply {
        chunks.push(reply.to_string());
    }
    let user_input =
        wrap_user_prompt_with_author(input.author_name, input.author_id, input.user_input);
    crate::services::provider::compact_resumed_provider_turn_prompt(
        input.provider,
        input.session_id,
        render_input_chunks(input.uploads, chunks, user_input),
    )
}

/// Input chunks have one ordering regardless of the transport's outer author frame.
pub(in crate::services::discord) fn render_input_chunks(
    uploads: &[String],
    context: impl IntoIterator<Item = String>,
    user_input: String,
) -> String {
    let mut chunks = Vec::new();
    if !uploads.is_empty() {
        chunks.push(uploads.join("\n"));
    }
    chunks.extend(context);
    if !user_input.is_empty() {
        chunks.push(user_input);
    }
    chunks.join("\n\n")
}

pub(in crate::services::discord) async fn prepare_uploads(
    uploads: &[Upload],
    pool: Option<&sqlx::PgPool>,
) -> Result<materialize::MaterializedUploads, String> {
    materialize::prepare(uploads, pool).await
}

pub(in crate::services::discord) async fn prepare_busy_uploads(
    channel: ChannelId,
    uploads: &[Upload],
    pool: Option<&sqlx::PgPool>,
) -> Result<Vec<String>, String> {
    super::prepare_busy_inject_uploads(channel, uploads, pool).await
}

#[cfg(test)]
pub(in crate::services::discord) fn fresh_source(
    original: &crate::services::turn_orchestrator::PendingQueueItem,
    context: &RetryPromptContext,
) -> Result<crate::services::turn_orchestrator::PendingQueueItem, String> {
    if original
        .source_message_ids
        .iter()
        .any(|id| *id != original.message_id)
    {
        return Err("fresh source requires one explicit original".into());
    }
    let encoded = context.encode().ok_or("raw context serialization failed")?;
    if original
        .reply_context
        .as_ref()
        .is_some_and(|old| old != &encoded)
    {
        return Err("fresh source cannot replace existing reply context".into());
    }
    let mut original = original.clone();
    original.reply_context = Some(encoded);
    Ok(original)
}

#[cfg(test)]
pub(in crate::services::discord) fn owned_original(
    request: &crate::services::discord::IntakeRequest,
    author_is_bot: bool,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    voice: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    durable_context: bool,
) -> crate::services::turn_orchestrator::Intervention {
    let reply = if durable_context {
        RetryPromptContext::restore(request.reply_context.clone())
    } else {
        RetryPromptContext::raw(request.reply_context.clone())
    };
    let mut original = build_race_requeued_intervention(
        request.request_owner, request.user_msg_id, &request.user_text,
        request.preserve_on_cancel, reply.encode(), request.has_reply_boundary,
        request.merge_consecutive, uploads, voice,
    );
    original.author_is_bot = author_is_bot;
    for source in request.source_message_ids.iter().filter(|id| **id != request.user_msg_id) {
        original.source_message_ids.push(*source);
        let generation = original.queued_generation;
        let source = if request.preserve_on_cancel {
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(*source, generation)
        } else {
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::new(*source, generation)
        };
        original.source_message_queued_generations.push(source);
    }
    original
}

#[cfg(test)]
mod tests;
