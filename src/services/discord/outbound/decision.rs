//! Pure outbound policy planner (#1006 v3, #1164).
//!
//! This module does not send, edit, split, summarize, or attach anything.
//! It only turns a [`super::message::DiscordOutboundMessage`] and its
//! [`super::policy::DiscordOutboundPolicy`] into explicit decisions that a
//! delivery implementation can execute without re-encoding policy branches at
//! every callsite.

use std::path::PathBuf;

use poise::serenity_prelude::{ChannelId, UserId};
use serde::{Deserialize, Serialize};

use super::message::{
    DiscordOutboundMessage, OutboundAttachmentSource, OutboundDedupKey, OutboundOperation,
    OutboundTarget,
};
use super::policy::{FallbackPolicy, LengthStrategy};
use super::result::FallbackUsed;

/// Discord's hard per-message character limit.
pub(crate) const DISCORD_MESSAGE_HARD_LIMIT_CHARS: usize = 2000;
/// Conservative chunk target used by new outbound policy planning.
pub(crate) const DISCORD_MESSAGE_SAFE_CHARS: usize = 1900;
pub(crate) const DEFAULT_TEXT_ATTACHMENT_NAME: &str = "agentdesk-discord-message.txt";
pub(crate) const TEXT_ATTACHMENT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// Tunable limits used by the pure planner. Tests use smaller limits to keep
/// scenarios readable; production callers can use [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboundPolicyLimits {
    pub(crate) inline_char_limit: usize,
    pub(crate) split_chunk_char_limit: usize,
    pub(crate) compact_char_limit: usize,
}

impl Default for OutboundPolicyLimits {
    fn default() -> Self {
        Self {
            inline_char_limit: DISCORD_MESSAGE_HARD_LIMIT_CHARS,
            split_chunk_char_limit: DISCORD_MESSAGE_SAFE_CHARS,
            compact_char_limit: DISCORD_MESSAGE_SAFE_CHARS,
        }
    }
}

impl OutboundPolicyLimits {
    // #3034: #1006 v3 outbound test-limit helper — used by planner tests.
    #[allow(dead_code)]
    pub(crate) fn for_tests(limit: usize) -> Self {
        assert!(limit > 0, "test outbound limit must be non-zero");
        Self {
            inline_char_limit: limit,
            split_chunk_char_limit: limit,
            compact_char_limit: limit,
        }
    }
}

/// Length-side policy decision for a single outbound message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LengthPolicyDecision {
    Inline {
        char_count: usize,
    },
    Split {
        char_count: usize,
        chunk_char_limit: usize,
        chunk_count: usize,
        fallback_used: FallbackUsed,
    },
    Compact {
        char_count: usize,
        compact_char_limit: usize,
        summary_available: bool,
        fallback_used: FallbackUsed,
    },
    FileAttachment {
        char_count: usize,
        attachments: Vec<AttachmentPolicyDecision>,
        fallback_used: FallbackUsed,
    },
    RejectOverLimit {
        char_count: usize,
        inline_char_limit: usize,
    },
}

/// Attachment source selected by the planner for file fallback delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachmentPolicyDecision {
    pub(crate) filename: String,
    pub(crate) content_type: Option<String>,
    pub(crate) source: AttachmentSourceDecision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AttachmentSourceDecision {
    InlineBytes { byte_len: usize },
    Path { path: PathBuf },
    GeneratedTextBody { char_count: usize },
}

/// Resolved primary delivery surface before transport-specific setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PrimaryDeliveryTarget {
    Channel(ChannelId),
    DmUser(UserId),
}

/// Target fallback plan to apply if primary delivery fails because a thread
/// cannot be posted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ThreadFallbackDecision {
    None,
    RetryParent {
        parent: ChannelId,
        failed_thread: ChannelId,
    },
}

/// Complete pure policy decision for the current outbound envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiscordOutboundPolicyDecision {
    pub(crate) dedup_key: OutboundDedupKey,
    pub(crate) primary_target: PrimaryDeliveryTarget,
    pub(crate) length: LengthPolicyDecision,
    pub(crate) thread_fallback: ThreadFallbackDecision,
}

// #3034: #1006 v3 default-limits convenience wrapper (prod delivery calls
// `decide_policy_with_limits` with explicit limits). Kept as API symmetry.
#[allow(dead_code)]
pub(crate) fn decide_policy(message: &DiscordOutboundMessage) -> DiscordOutboundPolicyDecision {
    decide_policy_with_limits(message, OutboundPolicyLimits::default())
}

pub(crate) fn decide_policy_with_limits(
    message: &DiscordOutboundMessage,
    limits: OutboundPolicyLimits,
) -> DiscordOutboundPolicyDecision {
    DiscordOutboundPolicyDecision {
        dedup_key: message.dedup_key(),
        primary_target: decide_primary_target(message.target),
        length: decide_length(message, limits),
        thread_fallback: decide_thread_fallback(
            message.target,
            message.operation,
            message.policy.fallback,
        ),
    }
}

fn decide_length(
    message: &DiscordOutboundMessage,
    limits: OutboundPolicyLimits,
) -> LengthPolicyDecision {
    let char_count = message.content.chars().count();
    let inline_limit = match message.policy.length_strategy {
        LengthStrategy::Compact => limits.compact_char_limit,
        LengthStrategy::Split
        | LengthStrategy::FileAttachment
        | LengthStrategy::RejectOverLimit => limits.inline_char_limit,
    };
    if char_count <= inline_limit {
        return LengthPolicyDecision::Inline { char_count };
    }

    match message.policy.length_strategy {
        LengthStrategy::Split => {
            let chunk_limit = limits.split_chunk_char_limit.max(1);
            LengthPolicyDecision::Split {
                char_count,
                chunk_char_limit: chunk_limit,
                chunk_count: char_count.div_ceil(chunk_limit),
                fallback_used: FallbackUsed::LengthSplit,
            }
        }
        LengthStrategy::Compact => LengthPolicyDecision::Compact {
            char_count,
            compact_char_limit: limits.compact_char_limit.max(1),
            summary_available: message.summary.is_some(),
            fallback_used: FallbackUsed::LengthCompacted,
        },
        LengthStrategy::FileAttachment => {
            let mut attachments = vec![AttachmentPolicyDecision {
                filename: DEFAULT_TEXT_ATTACHMENT_NAME.to_string(),
                content_type: Some(TEXT_ATTACHMENT_CONTENT_TYPE.to_string()),
                source: AttachmentSourceDecision::GeneratedTextBody { char_count },
            }];
            attachments.extend(message.attachments.iter().map(|attachment| {
                AttachmentPolicyDecision {
                    filename: attachment.filename.clone(),
                    content_type: attachment.content_type.clone(),
                    source: match &attachment.source {
                        OutboundAttachmentSource::Bytes { data } => {
                            AttachmentSourceDecision::InlineBytes {
                                byte_len: data.len(),
                            }
                        }
                        OutboundAttachmentSource::Path { path } => {
                            AttachmentSourceDecision::Path { path: path.clone() }
                        }
                    },
                }
            }));
            LengthPolicyDecision::FileAttachment {
                char_count,
                attachments,
                fallback_used: FallbackUsed::FileAttachment,
            }
        }
        LengthStrategy::RejectOverLimit => LengthPolicyDecision::RejectOverLimit {
            char_count,
            inline_char_limit: limits.inline_char_limit,
        },
    }
}

fn decide_primary_target(target: OutboundTarget) -> PrimaryDeliveryTarget {
    match target {
        OutboundTarget::Channel(channel) => PrimaryDeliveryTarget::Channel(channel),
        OutboundTarget::Thread { thread, .. } => PrimaryDeliveryTarget::Channel(thread),
        OutboundTarget::DmUser(user) => PrimaryDeliveryTarget::DmUser(user),
    }
}

fn decide_thread_fallback(
    target: OutboundTarget,
    operation: OutboundOperation,
    fallback: FallbackPolicy,
) -> ThreadFallbackDecision {
    match (target, operation, fallback) {
        (
            OutboundTarget::Thread { parent, thread },
            OutboundOperation::Send,
            FallbackPolicy::ThreadOrChannel,
        ) => ThreadFallbackDecision::RetryParent {
            parent,
            failed_thread: thread,
        },
        _ => ThreadFallbackDecision::None,
    }
}
