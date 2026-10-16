//! A Herdr channel's own admitted Bound execution, found before any watcher claims it.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{HostedLookupKey, load_hosted_execution_pg};
use crate::services::discord::SharedData;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

/// The logical session of this node's Bound Herdr execution on `channel` once a reconcile admitted
/// it, judged as a claimed watcher would be; `None` leaves the channel to the Unbound read.
pub(in crate::services::discord::turn_presence::activity) async fn admitted(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<String> {
    let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get())?;
    let node = crate::config::session_hosts::local_node();
    let name = start_name(shared, provider, channel).await?;
    let runtime_root = crate::config::runtime_root().map(|p| p.display().to_string());
    let owner = HostedOwner {
        provider: provider.as_str().into(),
        discord_token_hash: shared.token_hash.clone(),
        channel_id: channel.to_string(),
        logical_key: name.clone(),
        owner_node: node.clone().unwrap_or_default(),
        runtime_root: runtime_root.unwrap_or_default(),
    };
    let token_hash = &shared.token_hash;
    if !local_owner(
        &owner,
        provider,
        channel.get(),
        token_hash,
        &name,
        node.as_deref(),
    ) || crate::services::tui_prompt_dedupe::herdr_claim_admission(&name, true).is_err()
    {
        return None;
    }
    let channel_text = channel.to_string();
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id: &channel_text,
    };
    let key = HostedLookupKey::Canonical {
        provider: provider.as_str(),
        identity,
    };
    let HostedLookup::Found(found) = load_hosted_execution_pg(shared.pg_pool.as_ref()?, key).await
    else {
        return None;
    };
    herdr_record_at(&found, HostedState::Bound, &owner, &endpoint).then_some(name)
}

/// The name a start would give the session: the core basis and every configured name agree on it.
async fn start_name(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Option<String> {
    let core = (shared.core.lock().await.sessions.get(&channel))
        .and_then(|session| session.channel_name.clone());
    let config = crate::config_live_reload::current()?;
    let configured = (config.agents.iter().flat_map(|agent| agent.channels.iter()))
        .filter(|(_, b)| {
            b.channel_id().and_then(|id| id.parse::<u64>().ok()) == Some(channel.get())
        })
        .filter_map(|(_, b)| b.channel_name().or_else(|| b.aliases().into_iter().next()));
    let mut names =
        (core.into_iter().chain(configured)).map(|basis| provider.build_tmux_session_name(&basis));
    let name = names.next()?;
    names.all(|other| other == name).then_some(name)
}

#[cfg(test)]
#[path = "herdr_bound_tests.rs"]
mod tests;
