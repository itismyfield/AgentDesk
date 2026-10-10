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

    /// The typed result a holder answers a forwarded stop with.
    fn wire(&self) -> serde_json::Value {
        let outcome =
            |outcome: &str| serde_json::json!({"outcome": outcome, "effect_started": false});
        if let Some(reason) = self.refusal {
            let mut refused = outcome("refused");
            refused["reason"] = reason.to_string().into();
            return refused;
        }
        match &self.stop {
            CommandStop::Session(_) | CommandStop::Stop(_) => {
                serde_json::json!({"outcome": "stopping", "effect_started": true})
            }
            #[cfg(unix)]
            CommandStop::Herdr(stop) => {
                let seen = stop.observation();
                serde_json::json!({"outcome": "herdr", "intent": seen.intent,
                    "delivery": seen.delivery, "reason": seen.reason,
                    "settlement": seen.settlement, "effect_started": seen.delivery != "not_sent"})
            }
            CommandStop::AlreadyStopping => outcome("already_stopping"),
            CommandStop::HostRefused => outcome("host_refused"),
            CommandStop::NoActiveTurn => outcome("no_active_turn"),
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
        drop(permit);
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
                stop: {
                    eprintln!("D2B_UNNAMED_BEGIN_CALLED");
                    turn_bridge::begin_command_stop(shared, provider, channel, false).await
                },
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

/// A forwarded stop runs here as this holder's own `/stop`, inside the permit its receiver
/// admitted; `None` when no runtime of `provider` serves the channel.
pub(crate) async fn run_holder_stop(
    registry: Option<&crate::services::discord::health::HealthRegistry>,
    provider: &ProviderKind,
    channel: u64,
) -> Option<serde_json::Value> {
    let channel = ChannelId::new(channel);
    let shared = registry?
        .shared_for_provider_on_channel(provider, channel)
        .await?;
    Some(run_holder_stop_on(&shared, provider, channel).await)
}

pub(in crate::services::discord) async fn run_holder_stop_on(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> serde_json::Value {
    let reply = run_slash_stop(shared, provider, channel).await;
    let wire = reply.wire();
    reply.finish(shared, provider, channel).await;
    wire
}

/// What a gateway's `/stop` answers for a channel delegated away; `None` keeps the existing path.
pub(in crate::services::discord) async fn gateway_stop_reply(
    context: &crate::services::session_forwarding::ForwardCallerContext,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<String> {
    use crate::services::session_forwarding::home_stop::{self, GatewayStop};
    Some(
        match home_stop::gateway_stop(context, channel.get(), provider.as_str()).await {
            GatewayStop::Legacy => return None,
            GatewayStop::Refused(reason) => {
                format!("이 채널의 home 상태로는 여기서 중지하지 않아요 ({reason}).")
            }
            GatewayStop::Unconfirmed(reason) => format!(
                "holder 노드의 중지 결과를 확인하지 못했어요. 다시 보내지 않아요 ({reason})."
            ),
            GatewayStop::Confirmed(answer) => holder_reply(&answer),
        },
    )
}

fn holder_reply(answer: &serde_json::Value) -> String {
    let field = |key: &str| {
        answer
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("")
    };
    let fixed = match (field("outcome"), field("delivery")) {
        ("herdr", "sent") => "holder 노드가 중지 키를 보냈어요. 작업 종료 기록을 기다려요.",
        ("herdr", "indeterminate") => {
            "holder 노드가 중지 키 전달을 확인하지 못했어요. 재전송하지 않고 종료 기록을 기다려요."
        }
        ("herdr", "not_sent") => {
            return format!(
                "holder 노드가 중지 키를 보내지 않았어요 ({}).",
                field("reason")
            );
        }
        ("herdr", _) => "holder 노드의 중지 키 전달 결과를 확인하지 못했어요. 다시 보내지 않아요.",
        ("stopping", _) => super::STOPPING_RESPONSE,
        ("already_stopping", _) => super::ALREADY_STOPPING_RESPONSE,
        ("host_refused", _) => super::HOST_REFUSED_STOP_RESPONSE,
        ("no_active_turn", _) => super::NO_ACTIVE_TURN_RESPONSE,
        _ => return format!("holder 노드가 중지를 거절했어요 ({}).", field("reason")),
    };
    fixed.to_owned()
}
