//! Reconcile bot-owned input reactions; the caller supplies the original message's channel.

use std::future::Future;

use poise::serenity_prelude as serenity;

const REACTIONS: [char; 3] = ['\u{23f3}', '\u{2705}', '\u{26a0}'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputReaction {
    Pending,
    Done,
    Warning,
}

pub trait ReactionPort: Send + Sync {
    fn set(
        &self,
        channel: u64,
        message: u64,
        emoji: char,
        present: bool,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

pub struct DiscordReactions<'a>(pub &'a serenity::Http);

impl ReactionPort for DiscordReactions<'_> {
    async fn set(
        &self,
        channel: u64,
        message: u64,
        emoji: char,
        present: bool,
    ) -> Result<(), String> {
        let channel = serenity::ChannelId::new(channel);
        let message = serenity::MessageId::new(message);
        let emoji = serenity::ReactionType::Unicode(emoji.to_string());
        if present {
            channel.create_reaction(self.0, message, emoji).await
        } else {
            channel.delete_reaction(self.0, message, None, emoji).await
        }
        .map_err(|error| error.to_string())
    }
}

/// Repeated calls converge through Discord's own reaction identity, with no local dedup state.
pub async fn reconcile(
    port: &impl ReactionPort,
    channel: u64,
    message: u64,
    state: InputReaction,
) -> Result<(), String> {
    if channel == 0 || message == 0 {
        return Err("input reactions require a Discord channel and message".into());
    }
    let wanted = match state {
        InputReaction::Pending => REACTIONS[0],
        InputReaction::Done => REACTIONS[1],
        InputReaction::Warning => REACTIONS[2],
    };
    // Preserve the previous visible status if adding its replacement fails.
    port.set(channel, message, wanted, true).await?;
    for emoji in REACTIONS {
        if emoji != wanted {
            port.set(channel, message, emoji, false).await?;
        }
    }
    Ok(())
}
