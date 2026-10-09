//! Slash stop execution keeps provider-owned settlement apart from legacy cleanup.
use super::super::{SharedData, turn_bridge};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;
use turn_bridge::CommandStop;

pub(in crate::services::discord) struct StopReply {
    stop: CommandStop,
    refusal: Option<crate::services::cluster::channel_home::HomeRefusal>,
    permit: Option<crate::services::cluster::channel_home::CommandPermit>,
}

impl StopReply {
    pub(in crate::services::discord) fn text(&self) -> &'static str {
        if let Some(reason) = self.refusal {
            return match reason {
                crate::services::cluster::channel_home::HomeRefusal::Draining => "home_draining",
                crate::services::cluster::channel_home::HomeRefusal::NotHeld => "home_not_held",
            };
        }
        match &self.stop {
            CommandStop::Session(_) | CommandStop::Stop(_) => super::STOPPING_RESPONSE,
            #[cfg(unix)]
            CommandStop::Herdr(stop) => stop.reply(),
            CommandStop::AlreadyStopping => super::ALREADY_STOPPING_RESPONSE,
            CommandStop::HostRefused => super::HOST_REFUSED_STOP_RESPONSE,
            CommandStop::NoActiveTurn => super::NO_ACTIVE_TURN_RESPONSE,
        }
    }

    /// Called after the Discord acknowledgement, just as for legacy slash stop.
    pub(in crate::services::discord) async fn finish(
        self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
    ) {
        crate::services::cluster::channel_home::command_scope(self.permit, async {
            match self.stop {
            CommandStop::Session(stop) => {
                stop.interrupt("/stop").await;
            }
            CommandStop::Stop(stop) => {
                stop.stop(turn_bridge::TmuxCleanupPolicy::PreserveSession, "/stop")
                    .await;
                let release =
                    super::super::zombie_foreground_release::release_zombie_foreground_turn(
                        shared, provider, channel, "/stop",
                    )
                    .await;
                log_info_event!(
                    "discord_cancel_signal_sent",
                    channel_id = channel.get(),
                    provider = provider.as_str(),
                    status = if release.released { "released" } else { "sent" },
                    mailbox_foreground_released = release.released,
                    zombie_verdict = release.verdict_str(),
                    queue_depth_after = release.queue_depth_after,
                    queue_kickoff_scheduled = release.queue_kickoff_scheduled,
                );
            }
            #[cfg(unix)]
            CommandStop::Herdr(_) =>
            {
                #[cfg(test)]
                if crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(
                    "sent_as_terminal",
                ) {
                    crate::services::discord::mailbox_finish_turn(shared, provider, channel).await;
                }
            }
            CommandStop::AlreadyStopping | CommandStop::HostRefused | CommandStop::NoActiveTurn => {
            }
        }
        })
        .await;
    }
}

pub(in crate::services::discord) async fn run_slash_stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> StopReply {
    let permit = match super::control::home_fence::admit(channel, provider.as_str()) {
        Ok(permit) => permit,
        Err(reason) => {
            return StopReply {
                stop: CommandStop::HostRefused,
                refusal: Some(reason.0),
                permit: None,
            };
        }
    };
    #[cfg(test)]
    let permit = if super::control::home_fence::mutant("stop_permit_removed") {
        None
    } else {
        permit
    };
    crate::services::cluster::channel_home::command_scope(permit.clone(), async {
        #[cfg(test)]
        if crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(
            "slash_stop_uses_unnamed_begin",
        ) {
            return StopReply {
                refusal: None,
                stop: CommandStop::HostRefused,
                permit,
            };
        }
        StopReply {
            refusal: None,
            permit,
            stop: turn_bridge::begin_user_stop(shared, provider, channel, false, "/stop").await,
        }
    })
    .await
}
