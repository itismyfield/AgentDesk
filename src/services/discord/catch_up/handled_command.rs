//! Text-command evidence for catch-up: live intake's consumed record, or this
//! bot's reply to the command in the scanned history.

use poise::serenity_prelude as serenity;
use serenity::{MessageId, MessageReferenceKind};

use super::settled_frontier::SettledFrontier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextCommandEvidence {
    /// Not a `!` text command; recovery is unchanged.
    NotCommand,
    /// Live intake recorded the command or this bot replied to it: it was consumed.
    Consumed,
    /// No reply from this bot is in the scanned history; recovery is unchanged.
    NoReply,
    /// This bot's identity is unknown, so its replies cannot be recognized.
    ReplierUnknown,
}

pub(super) fn text_command_evidence(
    text: &str,
    message_id: MessageId,
    scanned: &[serenity::Message],
    bot_user_id: Option<u64>,
) -> TextCommandEvidence {
    if !is_text_command(text) {
        return TextCommandEvidence::NotCommand;
    }
    let Some(bot_user_id) = bot_user_id else {
        return TextCommandEvidence::ReplierUnknown;
    };
    let replied = scanned.iter().any(|reply| {
        reply.author.id.get() == bot_user_id
            && reply.message_reference.as_ref().is_some_and(|reference| {
                reference.kind == MessageReferenceKind::Default
                    && reference.message_id == Some(message_id)
            })
    });
    if replied {
        TextCommandEvidence::Consumed
    } else {
        TextCommandEvidence::NoReply
    }
}

/// The live intake's command test: `!` after an optional leading `<@id>` / `<@!id>`.
pub(super) fn is_text_command(text: &str) -> bool {
    without_leading_mention(text).starts_with('!')
}

/// Seals the frontier before a command whose handling cannot be read and
/// returns the retry cursor, so it is neither dropped nor replayed.
pub(super) fn defer_unrecognized_command(
    frontier: &mut SettledFrontier,
    scan_checkpoint: Option<u64>,
    message_id: u64,
) -> u64 {
    frontier.seal(message_id);
    tracing::warn!(
        message_id,
        "catch-up: own bot identity unavailable; text command deferred"
    );
    frontier.retry_after(scan_checkpoint, message_id)
}

/// Same shape as the live intake's leading `<@id>` / `<@!id>` strip.
fn without_leading_mention(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("<@") else {
        return text;
    };
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    match rest[digits..].strip_prefix('>') {
        Some(body) if digits > 0 => body.trim_start(),
        _ => text,
    }
}
