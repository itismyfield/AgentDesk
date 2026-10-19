//! The context an operator's transition cleanup request runs under: the channel's Bound Claude row
//! on this node, checked against its launch, this server's runtime and the channel's home.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::HealthRegistry;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
    load_hosted_execution_pg,
};
use crate::services::cluster::channel_home::{self, CommandPermit, HomeOwnership};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_endpoints;
use crate::services::tui_o::writer::transition_port::Context;
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
};

/// The execution the operator names.
pub(crate) struct Target {
    pub channel: u64,
    pub session_key: String,
    pub nonce: String,
}

/// The context of `target`'s execution; a part that disagrees refuses it before a permit is taken.
/// The runtime lookup answers its one runtime for any channel, so the row is checked, not trusted.
pub(crate) async fn resolve_context(
    registry: &HealthRegistry,
    pool: &PgPool,
    target: &Target,
) -> Result<Context, String> {
    #[cfg(test)]
    test_hooks::RESOLVES.with(|count| count.set(count.get() + 1));
    let key = HostedLookupKey::SessionKey(&target.session_key);
    let record = match load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(row) => match row.record {
            HostedRecord::Known(record) => record,
            _ => return Err("the row records no known execution".into()),
        },
        other => return Err(format!("the row could not be read: {other:?}")),
    };
    owned_here(&record, target)?;
    let channel = ChannelId::new(target.channel);
    let shared = registry.shared_for_provider_on_channel(&ProviderKind::Claude, channel);
    let shared = shared.await.ok_or("no Claude runtime is registered here")?;
    if shared.token_hash != record.owner.discord_token_hash {
        return Err("the row's bot is not this runtime's".into());
    }
    launched(&record)?;
    let (home_epoch, permit) = home_admission(target.channel)?;
    Ok(Context {
        lock: shared.session_transition_lock(channel),
        tmux: record.owner.logical_key,
        nonce: record.execution_nonce,
        home_epoch,
        permit,
    })
}

/// The row is the target's Bound Claude execution, owned by this node and runtime root and placed
/// on an endpoint registered here.
fn owned_here(record: &HostedExecution, target: &Target) -> Result<(), String> {
    let owner = &record.owner;
    let root = crate::config::runtime_root().map(|root| root.display().to_string());
    let node = crate::config::session_hosts::local_node();
    let location = record.location.as_ref();
    let refused = if record.state != HostedState::Bound {
        "the execution is not Bound"
    } else if record.execution_nonce != target.nonce {
        "the row records another execution"
    } else if owner.channel_id != target.channel.to_string() {
        "the row is another channel's"
    } else if owner.provider != ProviderKind::Claude.as_str() {
        "the row is not Claude's"
    } else if node.as_deref() != Some(&owner.owner_node) {
        "another node owns the row"
    } else if location.is_none_or(|l| l.execution_node != owner.owner_node) {
        "the execution runs on another node"
    } else if root.as_deref() != Some(&owner.runtime_root) {
        "another runtime root owns the row"
    } else if herdr_endpoints().view(record).is_none() {
        "no endpoint registered here holds the execution"
    } else {
        return Ok(());
    };
    Err(refused.into())
}

/// The launch context and the pane's spawn marker still name the row's execution.
fn launched(record: &HostedExecution) -> Result<(), String> {
    let owner = &record.owner;
    let context = execution_context(&owner.provider, &record.execution_nonce)?;
    if context.tmux_session != owner.logical_key
        || context.channel_id.map(|channel| channel.to_string()) != Some(owner.channel_id.clone())
        || context.owner_runtime_root != owner.runtime_root
    {
        return Err("the launch context names another execution".into());
    }
    match observe_spawn_nonce_marker(&owner.logical_key) {
        SpawnNonceMarker::Known(nonce) if nonce == record.execution_nonce => Ok(()),
        _ => Err("the pane's spawn marker names another execution".into()),
    }
}

/// The registered home's epoch and a permit, checked and taken under one home lock; a channel
/// without a registered home has neither.
pub(crate) fn home_admission(channel: u64) -> Result<(Option<i64>, Option<CommandPermit>), String> {
    let Some(home) = channel_home::registered_channel(channel) else {
        return Ok((None, None));
    };
    let HomeOwnership::Owned { home_epoch, .. } = home.ownership() else {
        return Err("the channel's home is not held here".into());
    };
    let permit = home.admit_at_epoch(ProviderKind::Claude.as_str(), home_epoch, false);
    let permit = permit.ok_or("the channel's home admits no command now")?;
    Ok((Some(home_epoch), Some(permit)))
}

#[cfg(test)]
pub(crate) mod test_hooks {
    thread_local! {
        /// Context resolutions this thread started.
        pub(crate) static RESOLVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
}
