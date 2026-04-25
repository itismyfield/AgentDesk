//! Pure outbound policy planner (#1006 v3, #1164).
//!
//! This module does not send, edit, split, summarize, or attach anything.
//! It only turns a [`super::message::DiscordOutboundMessage`] and its
//! [`super::policy::DiscordOutboundPolicy`] into explicit decisions that a
//! future transport implementation can execute without re-encoding policy
//! branches at every callsite.

use poise::serenity_prelude::ChannelId;
use serde::{Deserialize, Serialize};

use super::message::{DiscordOutboundMessage, OutboundTarget};
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
        fallback_used: FallbackUsed,
    },
    FileAttachment {
        char_count: usize,
        filename: String,
        content_type: String,
        fallback_used: FallbackUsed,
    },
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
    pub(crate) dedup_key: String,
    pub(crate) primary_channel: ChannelId,
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
        primary_channel: message.target.delivery_channel(),
        length: decide_length(&message.content, message.policy.length_strategy, limits),
        thread_fallback: decide_thread_fallback(message.target, message.policy.fallback),
    }
}

fn decide_length(
    content: &str,
    strategy: LengthStrategy,
    limits: OutboundPolicyLimits,
) -> LengthPolicyDecision {
    let char_count = content.chars().count();
    if char_count <= limits.inline_char_limit {
        return LengthPolicyDecision::Inline { char_count };
    }

    match strategy {
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
            fallback_used: FallbackUsed::LengthCompacted,
        },
        LengthStrategy::FileAttachment => LengthPolicyDecision::FileAttachment {
            char_count,
            filename: DEFAULT_TEXT_ATTACHMENT_NAME.to_string(),
            content_type: TEXT_ATTACHMENT_CONTENT_TYPE.to_string(),
            fallback_used: FallbackUsed::FileAttachment,
        },
    }
}

fn decide_thread_fallback(
    target: OutboundTarget,
    fallback: FallbackPolicy,
) -> ThreadFallbackDecision {
    match (target, fallback) {
        (OutboundTarget::Thread { parent, thread }, FallbackPolicy::ThreadOrChannel) => {
            ThreadFallbackDecision::RetryParent {
                parent,
                failed_thread: thread,
            }
        }
        _ => ThreadFallbackDecision::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::outbound::message::DiscordOutboundMessage;
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
        assert_eq!(decision.dedup_key, "dispatch:1164::dispatch:1164:posted");
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
                fallback_used: FallbackUsed::LengthCompacted,
            }
        );
        assert_eq!(decision.primary_channel, ChannelId::new(10));
    }

    #[test]
    fn file_attachment_policy_decision_selects_text_file_fallback() {
        let msg = message_with_policy(policy(LengthStrategy::FileAttachment, FallbackPolicy::None));

        let decision = decide_policy_with_limits(&msg, OutboundPolicyLimits::for_tests(5));

        assert_eq!(
            decision.length,
            LengthPolicyDecision::FileAttachment {
                char_count: 11,
                filename: DEFAULT_TEXT_ATTACHMENT_NAME.to_string(),
                content_type: TEXT_ATTACHMENT_CONTENT_TYPE.to_string(),
                fallback_used: FallbackUsed::FileAttachment,
            }
        );
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

        assert_eq!(decision.primary_channel, ChannelId::new(101));
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
}
