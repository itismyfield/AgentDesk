use super::*;

pub(in crate::services::discord) fn synthetic_thread_channel_name(
    parent_name: &str,
    channel_id: ChannelId,
) -> String {
    format!("{parent_name}-t{}", channel_id.get())
}

pub(super) fn is_synthetic_thread_channel_name(channel_name: &str, channel_id: ChannelId) -> bool {
    channel_name.ends_with(&format!("-t{}", channel_id.get()))
}

pub(super) fn choose_restore_channel_name(
    existing_channel_name: Option<&str>,
    live_channel_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<String>)>,
    channel_id: ChannelId,
) -> Option<String> {
    if let Some(existing_name) = existing_channel_name
        && is_synthetic_thread_channel_name(existing_name, channel_id)
    {
        return Some(existing_name.to_string());
    }

    if let Some((parent_id, parent_name)) = thread_parent {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(synthetic_thread_channel_name(&parent_name, channel_id));
    }

    live_channel_name
        .or(existing_channel_name)
        .map(ToOwned::to_owned)
}

pub(in crate::services::discord) fn resolve_is_dm_channel(
    dm_hint: Option<bool>,
    live_channel_lookup_says_dm: bool,
) -> bool {
    // Prefer the gateway-provided DM hint when available so a transient
    // Discord channel lookup failure cannot disable DM default-agent fallback.
    dm_hint.unwrap_or(live_channel_lookup_says_dm)
}

/// Resolve the channel name and parent category name for a Discord channel.
///
/// `cache` is an optional optimization: when present (leader-side), category
/// names are looked up via the in-memory guild cache and avoid an extra REST
/// hop. Worker-side callers without a live shard pass `None` and pay the
/// REST fallback at line ~978 instead. Correctness is identical either way.
pub(in crate::services::discord) async fn resolve_channel_category(
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = cache.and_then(|c| {
            c.guild(gc.guild_id).and_then(|guild| {
                guild
                    .channels
                    .get(&parent_id)
                    .map(|parent_ch| parent_ch.name.clone())
            })
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
        None
    };
    (ch_name, cat_name)
}

pub(in crate::services::discord) async fn validate_live_channel_routing_with_dm_hint(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
    is_dm_hint: Option<bool>,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    let is_dm = match is_dm_hint {
        Some(is_dm) => is_dm,
        None => matches!(
            channel_id.to_channel(&ctx.http).await,
            Ok(serenity::model::channel::Channel::Private(_))
        ),
    };
    let (channel_name, _) = resolve_channel_category(&ctx.http, Some(&ctx.cache), channel_id).await;
    let (allowlist_channel_id, provider_channel_name) = if let Some((parent_id, parent_name)) =
        resolve_thread_parent(&ctx.http, channel_id).await
    {
        (parent_id, parent_name.or(channel_name.clone()))
    } else {
        (channel_id, channel_name.clone())
    };
    validate_bot_channel_routing_with_provider_channel(
        settings,
        provider,
        allowlist_channel_id,
        channel_name.as_deref(),
        provider_channel_name.as_deref(),
        is_dm,
    )
}

pub(in crate::services::discord) async fn validate_live_channel_routing(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    validate_live_channel_routing_with_dm_hint(ctx, provider, settings, channel_id, None).await
}

pub(in crate::services::discord) async fn provider_handles_channel(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> bool {
    validate_live_channel_routing(ctx, provider, settings, channel_id)
        .await
        .is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum RuntimeChannelBindingStatus {
    Owned,
    Unowned,
    Unknown,
}

fn is_actual_thread_kind(kind: poise::serenity_prelude::ChannelType) -> bool {
    use poise::serenity_prelude::ChannelType;
    matches!(
        kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    )
}

pub(in crate::services::discord) async fn resolve_runtime_channel_binding_status(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> RuntimeChannelBindingStatus {
    if settings::has_configured_channel_binding(channel_id, None) {
        return RuntimeChannelBindingStatus::Owned;
    }

    let Ok(channel) = channel_id.to_channel(http).await else {
        return RuntimeChannelBindingStatus::Unknown;
    };

    match channel {
        serenity::model::channel::Channel::Private(_) => RuntimeChannelBindingStatus::Owned,
        serenity::model::channel::Channel::Guild(gc) => match gc.kind {
            kind if is_actual_thread_kind(kind) => {
                let Some(parent_id) = gc.parent_id else {
                    return RuntimeChannelBindingStatus::Unowned;
                };
                let parent_name = match parent_id.to_channel(http).await {
                    Ok(serenity::model::channel::Channel::Guild(parent)) => {
                        Some(parent.name.clone())
                    }
                    Ok(_) => None,
                    Err(_) => None,
                };
                if settings::has_configured_channel_binding(parent_id, parent_name.as_deref()) {
                    RuntimeChannelBindingStatus::Owned
                } else {
                    RuntimeChannelBindingStatus::Unowned
                }
            }
            _ => {
                if settings::has_configured_channel_binding(channel_id, Some(&gc.name)) {
                    RuntimeChannelBindingStatus::Owned
                } else {
                    RuntimeChannelBindingStatus::Unowned
                }
            }
        },
        _ => RuntimeChannelBindingStatus::Unowned,
    }
}

/// If `channel_id` is a Discord thread, return the parent channel ID and name.
/// For non-thread channels, returns `None`.
pub(in crate::services::discord) async fn resolve_thread_parent(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> Option<(serenity::model::id::ChannelId, Option<String>)> {
    let channel = channel_id.to_channel(http).await.ok()?;
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return None;
    };
    match gc.kind {
        kind if is_actual_thread_kind(kind) => {
            let parent_id = gc.parent_id?;
            let parent_name = if let Ok(parent_ch) = parent_id.to_channel(http).await {
                match parent_ch {
                    serenity::model::channel::Channel::Guild(pg) => Some(pg.name.clone()),
                    _ => None,
                }
            } else {
                None
            };
            Some((parent_id, parent_name))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::is_actual_thread_kind;
    use poise::serenity_prelude::ChannelType;

    #[test]
    fn actual_thread_classification_includes_news_threads() {
        assert!(is_actual_thread_kind(ChannelType::PublicThread));
        assert!(is_actual_thread_kind(ChannelType::PrivateThread));
        assert!(is_actual_thread_kind(ChannelType::NewsThread));
        assert!(!is_actual_thread_kind(ChannelType::Text));
    }
}
