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
    let token_hash = &shared.token_hash;
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
    let HostedRecord::Known(record) = &found.record else {
        return None;
    };
    // Only the logical key comes from the row; every other owner field is this node's own.
    let logical = record.owner.logical_key.clone();
    let node = crate::config::session_hosts::local_node();
    let runtime_root = crate::config::runtime_root().map(|p| p.display().to_string());
    let owner = HostedOwner {
        provider: provider.as_str().into(),
        discord_token_hash: token_hash.clone(),
        channel_id: channel_text.clone(),
        logical_key: logical.clone(),
        owner_node: node.clone().unwrap_or_default(),
        runtime_root: runtime_root.unwrap_or_default(),
    };
    let local = local_owner(
        &owner,
        provider,
        channel.get(),
        token_hash,
        &logical,
        node.as_deref(),
    );
    let ok = local
        && herdr_record_at(&found, HostedState::Bound, &owner, &endpoint)
        && crate::services::tui_prompt_dedupe::herdr_claim_admission(&logical, true).is_ok();
    ok.then_some(logical)
}

#[cfg(test)]
#[path = "herdr_bound_tests.rs"]
mod tests;
