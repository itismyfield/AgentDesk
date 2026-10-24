//! Channel-owned credentials for automatic inline session notices.

use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, Http};

use super::{ProviderKind, health::HealthRegistry, session_notice};

pub(crate) async fn channel_provider(
    registry: &HealthRegistry,
    channel: ChannelId,
    hint: Option<&str>,
) -> Result<ProviderKind, String> {
    if let Some(hint) = hint {
        let provider = ProviderKind::from_str(hint)
            .filter(ProviderKind::is_supported)
            .ok_or_else(|| format!("unsupported session notice provider: {hint}"))?;
        session_notice::provider_http(registry, &provider, channel).await?;
        return Ok(provider);
    }

    let mut owners = Vec::new();
    for entry in registry.provider_entries_snapshot().await {
        let Some(provider) = ProviderKind::from_str(&entry.name).filter(ProviderKind::is_supported)
        else {
            continue;
        };
        let settings = entry.shared.settings.read().await.clone();
        let explicit = settings.allowed_channel_ids.contains(&channel.get());
        let live = if explicit {
            false
        } else if let Some(ctx) = entry.shared.http.cached_serenity_ctx.get() {
            super::provider_handles_channel(ctx, &provider, &settings, channel).await
        } else {
            false
        };
        if (explicit || live) && !owners.contains(&provider) {
            owners.push(provider);
        }
    }
    if owners.len() != 1 {
        return Err(format!(
            "session notice requires one channel provider on {channel}; found {}",
            owners.len()
        ));
    }
    let provider = owners.remove(0);
    // The shared formatter's owner contract also rejects ambiguous runtimes within a provider.
    session_notice::provider_http(registry, &provider, channel).await?;
    Ok(provider)
}

pub(crate) async fn channel_http(
    registry: &HealthRegistry,
    channel: ChannelId,
) -> Result<Arc<Http>, String> {
    let provider = channel_provider(registry, channel, None).await?;
    session_notice::provider_http(registry, &provider, channel).await
}

#[cfg(test)]
pub(crate) struct TestProviderHttp {
    _rest: super::shared_state::test_rest::Guard,
}

#[cfg(test)]
pub(crate) async fn register_test_provider(
    registry: &super::health::HealthRegistry,
    provider: &str,
    channel: u64,
    http: std::sync::Arc<poise::serenity_prelude::Http>,
    pg: sqlx::PgPool,
) -> TestProviderHttp {
    let shared = super::make_shared_data_for_tests_with_storage(Some(pg));
    shared.settings.write().await.allowed_channel_ids = vec![channel];
    registry.register(provider.to_string(), shared).await;
    TestProviderHttp {
        _rest: super::shared_state::test_rest::install(http),
    }
}
