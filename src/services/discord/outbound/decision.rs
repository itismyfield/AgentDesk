//! Pure outbound policy planner (#1006 v3, #1164).
//!
//! This module does not send, edit, split, summarize, or attach anything.
//! It only turns a [`super::message::DiscordOutboundMessage`] and its
//! [`super::policy::DiscordOutboundPolicy`] into explicit decisions that a
//! future transport implementation can execute without re-encoding policy
//! branches at every callsite.

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
    if char_count <= limits.inline_char_limit {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::outbound::message::{
        DiscordOutboundMessage, OutboundOperationKey, OutboundTargetKey,
    };
    use crate::services::discord::outbound::policy::{
        DiscordOutboundPolicy, FallbackPolicy, LengthStrategy,
    };
    use std::time::Duration;

    fn message_with_policy(policy: DiscordOutboundPolicy) -> DiscordOutboundMessage {
        DiscordOutboundMessage::new(
            "dispatch:1164",
            "dispatch:1164:posted",
            "x".repeat(11),
            OutboundTarget::Channel(ChannelId::new(10)),
            policy,
        )
    }

    fn policy(length_strategy: LengthStrategy, fallback: FallbackPolicy) -> DiscordOutboundPolicy {
        DiscordOutboundPolicy {
            length_strategy,
            fallback,
            idempotency_window: Duration::from_secs(60),
        }
    }

    #[test]
    fn split_policy_decision_records_chunk_count_and_fallback_tag() {
        let msg = message_with_policy(policy(LengthStrategy::Split, FallbackPolicy::None));

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(5));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::Split {
                char_count: 11,
                chunk_char_limit: 5,
                chunk_count: 3,
                fallback_used: FallbackUsed::LengthSplit,
            }
        );
        assert_eq!(
            decision.dedup_key,
            OutboundDedupKey {
                correlation_id: "dispatch:1164".into(),
                semantic_event_id: "dispatch:1164:posted".into(),
                target: OutboundTargetKey::Channel {
                    channel: ChannelId::new(10),
                },
                operation: OutboundOperationKey::Send,
            }
        );
    }

    #[test]
    fn compact_policy_decision_keeps_single_message_target() {
        let msg = message_with_policy(policy(LengthStrategy::Compact, FallbackPolicy::None));

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(5));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::Compact {
                char_count: 11,
                compact_char_limit: 5,
                summary_available: false,
                fallback_used: FallbackUsed::LengthCompacted,
            }
        );
        assert_eq!(
            decision.primary_target,
            PrimaryDeliveryTarget::Channel(ChannelId::new(10))
        );
    }

    #[test]
    fn compact_policy_decision_records_summary_availability() {
        let msg = message_with_policy(policy(LengthStrategy::Compact, FallbackPolicy::None))
            .with_summary("summary");

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(5));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::Compact {
                char_count: 11,
                compact_char_limit: 5,
                summary_available: true,
                fallback_used: FallbackUsed::LengthCompacted,
            }
        );
    }

    #[test]
    fn file_attachment_policy_decision_selects_text_file_fallback() {
        let msg = message_with_policy(policy(LengthStrategy::FileAttachment, FallbackPolicy::None));

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(5));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::FileAttachment {
                char_count: 11,
                attachments: vec![AttachmentPolicyDecision {
                    filename: DEFAULT_TEXT_ATTACHMENT_NAME.to_string(),
                    content_type: Some(TEXT_ATTACHMENT_CONTENT_TYPE.to_string()),
                    source: AttachmentSourceDecision::GeneratedTextBody { char_count: 11 },
                }],
                fallback_used: FallbackUsed::FileAttachment,
            }
        );
    }

    #[test]
    fn file_attachment_policy_decision_preserves_body_and_supplied_attachment_sources() {
        let msg = message_with_policy(policy(LengthStrategy::FileAttachment, FallbackPolicy::None))
            .with_bytes_attachment("report.md", Some("text/markdown"), b"payload".to_vec())
            .with_path_attachment("trace.log", Some("text/plain"), "/tmp/trace.log");

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(5));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::FileAttachment {
                char_count: 11,
                attachments: vec![
                    AttachmentPolicyDecision {
                        filename: DEFAULT_TEXT_ATTACHMENT_NAME.to_string(),
                        content_type: Some(TEXT_ATTACHMENT_CONTENT_TYPE.to_string()),
                        source: AttachmentSourceDecision::GeneratedTextBody { char_count: 11 },
                    },
                    AttachmentPolicyDecision {
                        filename: "report.md".into(),
                        content_type: Some("text/markdown".into()),
                        source: AttachmentSourceDecision::InlineBytes { byte_len: 7 },
                    },
                    AttachmentPolicyDecision {
                        filename: "trace.log".into(),
                        content_type: Some("text/plain".into()),
                        source: AttachmentSourceDecision::Path {
                            path: PathBuf::from("/tmp/trace.log"),
                        },
                    },
                ],
                fallback_used: FallbackUsed::FileAttachment,
            }
        );
    }

    #[test]
    fn thread_fallback_policy_decision_does_not_reroute_edits() {
        let msg = DiscordOutboundMessage::new(
            "dispatch:1164",
            "dispatch:1164:thread-edit",
            "short",
            OutboundTarget::Thread {
                parent: ChannelId::new(100),
                thread: ChannelId::new(101),
            },
            policy(LengthStrategy::Split, FallbackPolicy::ThreadOrChannel),
        )
        .with_operation(OutboundOperation::Edit {
            message_id: poise::serenity_prelude::MessageId::new(777),
        });

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(20));

        assert_eq!(decision.thread_fallback, ThreadFallbackDecision::None);
    }

    #[test]
    fn inline_content_does_not_trigger_length_fallback() {
        let msg = message_with_policy(policy(LengthStrategy::FileAttachment, FallbackPolicy::None));

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(20));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::Inline { char_count: 11 }
        );
    }

    #[test]
    fn thread_fallback_policy_decision_reroutes_to_parent_after_thread_failure() {
        let msg = DiscordOutboundMessage::new(
            "dispatch:1164",
            "dispatch:1164:thread",
            "short",
            OutboundTarget::Thread {
                parent: ChannelId::new(100),
                thread: ChannelId::new(101),
            },
            policy(LengthStrategy::Split, FallbackPolicy::ThreadOrChannel),
        );

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(20));

        assert_eq!(
            decision.primary_target,
            PrimaryDeliveryTarget::Channel(ChannelId::new(101))
        );
        assert_eq!(
            decision.thread_fallback,
            ThreadFallbackDecision::RetryParent {
                parent: ChannelId::new(100),
                failed_thread: ChannelId::new(101),
            }
        );
    }

    #[test]
    fn thread_fallback_policy_decision_stays_disabled_for_plain_channels() {
        let msg = message_with_policy(policy(
            LengthStrategy::Split,
            FallbackPolicy::ThreadOrChannel,
        ));

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(20));

        assert_eq!(decision.thread_fallback, ThreadFallbackDecision::None);
    }

    #[test]
    fn dm_target_policy_decision_keeps_user_target() {
        let msg = DiscordOutboundMessage::new(
            "dm:7",
            "dm:7:notice",
            "short",
            OutboundTarget::DmUser(UserId::new(7)),
            policy(LengthStrategy::Split, FallbackPolicy::None),
        );

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(20));

        assert_eq!(
            decision.primary_target,
            PrimaryDeliveryTarget::DmUser(UserId::new(7))
        );
        assert_eq!(decision.thread_fallback, ThreadFallbackDecision::None);
    }
}
