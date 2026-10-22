//! Only a new human prompt's adoption needs a live execution; admitted readers keep their tail.
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
};
use crate::services::discord::SharedData;
use crate::services::session_host::herdr_endpoints;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

pub(super) async fn current(
    shared: &Arc<SharedData>,
    provider: &str,
    channel: ChannelId,
    logical: &str,
) -> bool {
    match crate::services::tui_prompt_dedupe::hosted_source::herdr_evidence(
        channel.get(),
        logical,
        None,
    ) {
        Ok(false) => return true,
        Err(_) => return false,
        Ok(true) => {}
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return false;
    };
    let channel_text = channel.to_string();
    let key = HostedLookupKey::Canonical {
        provider,
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &shared.token_hash,
            channel_id: &channel_text,
        },
    };
    let HostedLookup::Found(observed) = load_hosted_execution_pg(pool, key).await else {
        return false;
    };
    let HostedRecord::Known(record) = &observed.record else {
        return false;
    };
    if record.state != HostedState::Bound || record.owner.logical_key != logical {
        return false;
    }
    execution_current(record).await
}

pub(super) async fn execution_current(
    record: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
) -> bool {
    let Some(target) = herdr_endpoints().target(record) else {
        return false;
    };
    tokio::task::spawn_blocking(move || target.execution_alive())
        .await
        .unwrap_or(false)
}
