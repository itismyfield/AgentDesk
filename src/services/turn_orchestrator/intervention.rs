//! Queued message payloads and their per-source ownership metadata.
use std::time::Instant;

use poise::serenity_prelude::{MessageId, UserId};

use crate::services::cluster::attachment_transfer::uploads::PendingUploads;

use super::SourceMessageQueuedGeneration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InterventionMode {
    Soft,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceMessageTextSegment {
    pub(crate) message_id: MessageId,
    pub(crate) text: String,
}

impl SourceMessageTextSegment {
    pub(crate) fn new(message_id: MessageId, text: impl Into<String>) -> Self {
        Self {
            message_id,
            text: text.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Intervention {
    pub(crate) author_id: UserId,
    pub(crate) author_is_bot: bool,
    pub(crate) message_id: MessageId,
    pub(crate) queued_generation: u64,
    pub(crate) source_message_ids: Vec<MessageId>,
    pub(crate) source_message_queued_generations: Vec<SourceMessageQueuedGeneration>,
    pub(crate) source_text_segments: Vec<SourceMessageTextSegment>,
    pub(crate) text: String,
    pub(crate) mode: InterventionMode,
    pub(crate) created_at: Instant,
    pub(crate) reply_context: Option<String>,
    pub(crate) has_reply_boundary: bool,
    pub(crate) merge_consecutive: bool,
    pub(crate) pending_uploads: PendingUploads,
    /// #2266: when a voice-transcript announcement loses the
    /// `mailbox_try_start_turn` race and is enqueued for later dispatch, the
    /// per-process `voice::announce_meta` store entry is consumed by the
    /// original `handle_text_message` call before the race-loss branch runs.
    /// Embedding the full announcement here keeps the queued payload
    /// self-contained so the dispatch path (which reinserts the entry into
    /// the store before re-entering `handle_text_message`) can reconstruct
    /// the voice-transcript framing instead of falling back to plain text.
    /// `None` for non-voice paths.
    pub(crate) voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
}

impl Intervention {
    pub(crate) fn preserve_on_cancel(&self) -> bool {
        self.source_message_queued_generations
            .iter()
            .any(|source| source.preserve_on_cancel)
    }

    pub(crate) fn source_message_queued_generations(&self) -> Vec<SourceMessageQueuedGeneration> {
        let source_message_ids = if self.source_message_ids.is_empty() {
            vec![self.message_id]
        } else {
            self.source_message_ids.clone()
        };
        if self.source_message_queued_generations.is_empty() {
            return source_message_ids
                .into_iter()
                .map(|message_id| {
                    SourceMessageQueuedGeneration::new(message_id, self.queued_generation)
                })
                .collect();
        }
        let mut owners = self.source_message_queued_generations.clone();
        for message_id in source_message_ids {
            if !owners.iter().any(|owner| owner.message_id == message_id) {
                owners.push(SourceMessageQueuedGeneration::new(
                    message_id,
                    self.queued_generation,
                ));
            }
        }
        owners
    }

    pub(crate) fn source_text_segments(&self) -> Vec<SourceMessageTextSegment> {
        let source_message_ids = if self.source_message_ids.is_empty() {
            vec![self.message_id]
        } else {
            self.source_message_ids.clone()
        };
        if self.source_text_segments.is_empty() {
            return split_text_segments_for_sources(&source_message_ids, &self.text);
        }

        let mut segments = Vec::new();
        for message_id in source_message_ids {
            if let Some(segment) = self
                .source_text_segments
                .iter()
                .find(|segment| segment.message_id == message_id)
            {
                segments.push(segment.clone());
            } else {
                segments.push(SourceMessageTextSegment::new(message_id, String::new()));
            }
        }
        segments
    }
}

fn split_text_segments_for_sources(
    source_message_ids: &[MessageId],
    text: &str,
) -> Vec<SourceMessageTextSegment> {
    if source_message_ids.is_empty() {
        return Vec::new();
    }
    if source_message_ids.len() == 1 {
        return vec![SourceMessageTextSegment::new(source_message_ids[0], text)];
    }

    if text.matches('\n').count() + 1 != source_message_ids.len() {
        return source_message_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, message_id)| {
                SourceMessageTextSegment::new(message_id, if index == 0 { text } else { "" })
            })
            .collect();
    }

    let mut pieces = text.splitn(source_message_ids.len(), '\n');
    source_message_ids
        .iter()
        .copied()
        .map(|message_id| {
            SourceMessageTextSegment::new(message_id, pieces.next().unwrap_or_default())
        })
        .collect()
}
