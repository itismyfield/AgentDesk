use super::*;
use crate::services::agent_recovery::{RecoveryIntake, channel_recovery_intake, fallback_provider};
use crate::services::discord::commands::autocomplete::{
    already_routed, mark_routed, query_deadline,
};

struct ChannelPolicy {
    binding: ChannelId,
    name: Option<String>,
    provider_name: Option<String>,
    is_dm: bool,
}

struct ChannelInfo {
    name: Option<String>,
    kind: serenity::ChannelType,
    parent: Option<ChannelId>,
}

enum Route {
    Allowed,
    Rejected {
        channel: Option<ChannelPolicy>,
        target: Option<ProviderKind>,
    },
    Unverified,
}

pub(super) async fn check(ctx: Context<'_>) -> Result<bool, Error> {
    let poise::Context::Application(app) = ctx else {
        let settings = ctx.data().shared.settings.read().await.clone();
        return Ok(provider_handles_channel(
            ctx.serenity_context(),
            &ctx.data().provider,
            &settings,
            ctx.channel_id(),
        )
        .await);
    };
    let deadline = query_deadline(ctx).await;
    if already_routed(ctx).await {
        return Ok(true);
    }
    let route = tokio::time::timeout_at(deadline.into(), route(ctx))
        .await
        .unwrap_or(Route::Unverified);
    if matches!(route, Route::Allowed) {
        mark_routed(ctx).await;
        return Ok(true);
    }
    tracing::info!(channel_id = %ctx.channel_id(), provider = ctx.data().provider.as_str(), command = %ctx.command().name, "CMD-GUARD: rejecting slash command");
    match app.interaction_type {
        poise::CommandInteractionType::Autocomplete => {
            app.interaction
                .create_response(
                    ctx.serenity_context(),
                    serenity::CreateInteractionResponse::Autocomplete(
                        serenity::CreateAutocompleteResponse::new(),
                    ),
                )
                .await?;
        }
        poise::CommandInteractionType::Command => {
            let content = match route {
                Route::Rejected { channel, target } => {
                    let target = match target {
                        Some(target) => Some(target),
                        None => tokio::time::timeout_at(
                            deadline.into(),
                            target_provider(ctx, channel.as_ref()),
                        )
                        .await
                        .ok()
                        .flatten(),
                    };
                    guidance(ctx, target)
                }
                _ => "이 채널 담당 여부를 확인하지 못했어요. 잠시 후 다시 시도해 주세요.".into(),
            };
            ctx.send(
                poise::CreateReply::default()
                    .content(content)
                    .ephemeral(true),
            )
            .await?;
        }
    }
    Ok(false)
}

async fn route(ctx: Context<'_>) -> Route {
    let channel_id = ctx.channel_id().get().to_string();
    match channel_recovery_intake(&ctx.data().provider, &channel_id).await {
        Some(RecoveryIntake::Allow) => return Route::Allowed,
        Some(RecoveryIntake::Skip) => {
            // Skip also represents a failed durable refresh. Only a verified fallback proves rejection.
            if let Some(fallback) = fallback_provider(&channel_id)
                && matches!(
                    channel_recovery_intake(&fallback, &channel_id).await,
                    Some(RecoveryIntake::Allow)
                )
            {
                return Route::Rejected {
                    channel: None,
                    target: Some(fallback),
                };
            }
            return Route::Unverified;
        }
        None => {}
    }
    let Ok(channel) = channel_policy(ctx).await else {
        return Route::Unverified;
    };
    let settings = ctx.data().shared.settings.read().await.clone();
    if validates(&settings, &ctx.data().provider, &channel) {
        Route::Allowed
    } else {
        Route::Rejected {
            channel: Some(channel),
            target: None,
        }
    }
}

fn validates(
    settings: &DiscordBotSettings,
    provider: &ProviderKind,
    channel: &ChannelPolicy,
) -> bool {
    settings::validate_bot_channel_routing_with_provider_channel(
        settings,
        provider,
        channel.binding,
        channel.name.as_deref(),
        channel.provider_name.as_deref(),
        channel.is_dm,
    )
    .is_ok()
}

fn is_thread(kind: serenity::ChannelType) -> bool {
    matches!(
        kind,
        serenity::ChannelType::PublicThread
            | serenity::ChannelType::PrivateThread
            | serenity::ChannelType::NewsThread
    )
}

fn is_dm(kind: serenity::ChannelType) -> bool {
    matches!(
        kind,
        serenity::ChannelType::Private | serenity::ChannelType::GroupDm
    )
}

impl ChannelInfo {
    fn complete(&self) -> bool {
        is_dm(self.kind)
            || (self.name.is_some() && (!is_thread(self.kind) || self.parent.is_some()))
    }
}

async fn channel_info(
    ctx: Context<'_>,
    id: ChannelId,
    payload: Option<&serenity::PartialChannel>,
) -> Result<ChannelInfo, ()> {
    if let Some(channel) = payload.filter(|channel| channel.id == id) {
        let info = ChannelInfo {
            name: channel.name.clone(),
            kind: channel.kind,
            parent: channel.parent_id,
        };
        if info.complete() {
            return Ok(info);
        }
    }
    let cached = ctx.guild_id().and_then(|guild| {
        let guild = ctx.serenity_context().cache.guild(guild)?;
        guild
            .channels
            .get(&id)
            .or_else(|| guild.threads.iter().find(|thread| thread.id == id))
            .map(|channel| ChannelInfo {
                name: Some(channel.name.clone()),
                kind: channel.kind,
                parent: channel.parent_id,
            })
    });
    if let Some(channel) = cached.filter(ChannelInfo::complete) {
        return Ok(channel);
    }
    let info = match id
        .to_channel(&ctx.serenity_context().http)
        .await
        .map_err(|_| ())?
    {
        serenity::Channel::Guild(channel) => ChannelInfo {
            name: Some(channel.name),
            kind: channel.kind,
            parent: channel.parent_id,
        },
        serenity::Channel::Private(_) => ChannelInfo {
            name: None,
            kind: serenity::ChannelType::Private,
            parent: None,
        },
        _ => return Err(()),
    };
    info.complete().then_some(info).ok_or(())
}

async fn channel_policy(ctx: Context<'_>) -> Result<ChannelPolicy, ()> {
    let payload = match ctx {
        poise::Context::Application(app) => app.interaction.channel.as_ref(),
        _ => None,
    };
    let channel = channel_info(ctx, ctx.channel_id(), payload).await?;
    let is_dm = is_dm(channel.kind);
    let parent = channel.parent.filter(|_| is_thread(channel.kind));
    let provider_name = match parent {
        Some(parent) => Some(channel_info(ctx, parent, None).await?.name.ok_or(())?),
        None => channel.name.clone(),
    };
    Ok(ChannelPolicy {
        binding: parent.unwrap_or(ctx.channel_id()),
        name: channel.name,
        provider_name,
        is_dm,
    })
}

async fn target_provider(
    ctx: Context<'_>,
    channel: Option<&ChannelPolicy>,
) -> Option<ProviderKind> {
    let id = ctx.channel_id().get().to_string();
    if let Some(fallback) = fallback_provider(&id)
        && matches!(
            channel_recovery_intake(&fallback, &id).await,
            Some(RecoveryIntake::Allow)
        )
    {
        return Some(fallback);
    }
    let channel = channel?;
    for candidate in crate::services::provider::provider_registry()
        .iter()
        .filter_map(|entry| entry.kind())
    {
        if candidate != ctx.data().provider
            && validates(&DiscordBotSettings::default(), &candidate, channel)
            && !matches!(
                channel_recovery_intake(&candidate, &id).await,
                Some(RecoveryIntake::Skip)
            )
        {
            return Some(candidate);
        }
    }
    None
}

fn guidance(ctx: Context<'_>, target: Option<ProviderKind>) -> String {
    let command = ctx
        .parent_commands()
        .iter()
        .map(|command| command.name.as_ref())
        .chain(std::iter::once(ctx.command().name.as_ref()))
        .collect::<Vec<_>>()
        .join(" ");
    match target {
        Some(provider) => format!(
            "이 채널에서는 `{}` 봇의 `/{command}` 명령을 선택해 주세요.",
            provider.as_str()
        ),
        None => format!(
            "이 봇은 이 채널의 명령을 처리할 수 없어요. 이 채널 담당 봇의 `/{command}` 명령을 선택해 주세요."
        ),
    }
}
