//! State-changing commands on a delegated channel run only where its home is held with intake
//! open. Nothing sends a command on to the holder yet, so elsewhere it is refused before any effect.

use poise::serenity_prelude::ChannelId;

use super::super::super::{Context, Error};
use crate::services::cluster::channel_home::{self, CommandPermit, HomeRefusal};

/// A delegated channel's command refused on this node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct CommandRefused(
    pub(in crate::services::discord) HomeRefusal,
);

impl std::fmt::Display for CommandRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self.0 {
            HomeRefusal::NotHeld => "이 채널은 다른 노드가 맡고 있어 여기서 명령을 처리하지 않아요",
            HomeRefusal::Draining => "이 채널을 다른 노드로 넘기는 중이라 명령을 받지 않아요",
        };
        write!(f, "{reason} ({})", self.0)
    }
}

impl std::error::Error for CommandRefused {}

pub(in crate::services::discord) fn admit(
    channel_id: ChannelId,
    provider: &str,
) -> Result<Option<CommandPermit>, CommandRefused> {
    channel_home::admit_command(&channel_id.get().to_string(), provider).map_err(CommandRefused)
}

/// Refuses the command when the channel's home is registered here but not held with intake open;
/// a channel with no registered home passes unread.
pub(super) fn check(channel_id: ChannelId) -> Result<(), CommandRefused> {
    admit(channel_id, "preflight").map(drop)
}

/// [`check`] for a slash command: a refusal is answered and the command ends there.
pub(super) async fn refused(ctx: &Context<'_>) -> Result<bool, Error> {
    let Err(refused) = check(ctx.channel_id()) else {
        return Ok(false);
    };
    let channel_id = ctx.channel_id().get();
    tracing::warn!(channel_id, %refused, "delegated channel command refused");
    ctx.say(refused.to_string()).await?;
    Ok(true)
}

#[cfg(test)]
#[path = "home_fence_tests.rs"]
mod tests;

#[cfg(test)]
pub(in crate::services::discord) fn mutant(name: &str) -> bool {
    crate::services::cluster::channel_home::command_mutant(name)
}

#[cfg(test)]
thread_local! {
    pub(in crate::services::discord) static PAUSE: std::cell::RefCell<Option<(&'static str, ArcPause)>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(in crate::services::discord) type ArcPause =
    std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>;

#[cfg(test)]
pub(in crate::services::discord) async fn pause(label: &str) {
    let barrier = PAUSE.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|(name, _)| *name == label)
            .map(|(_, barrier)| barrier.clone())
    });
    if let Some(barrier) = barrier {
        barrier.0.notify_one();
        barrier.1.notified().await;
    }
}
