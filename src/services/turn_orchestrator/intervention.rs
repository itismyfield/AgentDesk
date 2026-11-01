//! Queued message payloads and their per-source ownership metadata.
use std::time::Instant;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

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
}

/// Where a turn resolved to run and the routing its claim publishes, kept with a queued item so
/// a replay uses this decision instead of resolving again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct QueuedRoute {
    pub(crate) target: ChannelId,
    pub(crate) parent: Option<ChannelId>,
    pub(crate) role: Option<ChannelId>,
}

#[cfg(test)]
thread_local! {
    static SOURCE_CLOCK_SAMPLE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

impl Intervention {
    #[cfg(test)]
    pub(crate) fn source_clock_sample_hook_for_tests(hook: impl FnOnce() + 'static) {
        SOURCE_CLOCK_SAMPLE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    /// The first route any source resolved before queueing; `None` resolves again on replay.
    pub(crate) fn queued_route(&self) -> Option<QueuedRoute> {
        self.source_message_queued_generations
            .iter()
            .find_map(|source| source.route)
    }

    /// Stamps every source with the route, so merges and per-source exits keep it.
    pub(crate) fn with_route(mut self, route: QueuedRoute) -> Self {
        if self.source_message_queued_generations.is_empty() {
            self.source_message_queued_generations = self.source_message_queued_generations();
        }
        for source in &mut self.source_message_queued_generations {
            source.route = Some(route);
        }
        self
    }

    /// Consecutive soft inputs from one author merge unless a reply boundary or two routes split them.
    pub(crate) fn merges_with(&self, incoming: &Intervention) -> bool {
        self.mode == InterventionMode::Soft
            && incoming.mode == InterventionMode::Soft
            && self.merge_consecutive
            && incoming.merge_consecutive
            && self.author_id == incoming.author_id
            && !self.has_reply_boundary
            && !incoming.has_reply_boundary
            && match (self.queued_route(), incoming.queued_route()) {
                (Some(held), Some(arrived)) => held == arrived,
                // A route-less input resolves again in this channel, so it takes the other's route.
                _ => true,
            }
    }

    pub(crate) fn preserve_on_cancel(&self) -> bool {
        self.source_message_queued_generations
            .iter()
            .any(|source| source.preserve_on_cancel)
    }

    pub(crate) fn source_message_queued_generations(&self) -> Vec<SourceMessageQueuedGeneration> {
        #[cfg(test)]
        if let Some(hook) = SOURCE_CLOCK_SAMPLE_HOOK.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let source_message_ids = if self.source_message_ids.is_empty() {
            vec![self.message_id]
        } else {
            self.source_message_ids.clone()
        };
        let mut owners = if self.source_message_queued_generations.is_empty() {
            source_message_ids
                .iter()
                .copied()
                .map(|message_id| {
                    SourceMessageQueuedGeneration::new(message_id, self.queued_generation)
                })
                .collect()
        } else {
            self.source_message_queued_generations.clone()
        };
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
