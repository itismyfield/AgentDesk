//! A clear commits its next-launch intent independently of transcript-boundary persistence.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::super::ClearReport;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::provider_resume_fence;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) async fn set_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    explicit_key: Option<&str>,
    report: &mut ClearReport,
) {
    let host = crate::services::discord::admin_host_guard::host_decision_of;
    if *provider != ProviderKind::Claude || !host(shared, provider, channel.get()).await.is_herdr() {
        return;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        report.fail("provider_resume_fence", "postgres pool unavailable");
        return;
    };
    let resolved = if explicit_key.is_some() {
        None
    } else {
        super::super::resolve_session_key_for_clear(http, shared, channel, provider).await
    };
    let Some(key) = super::super::choose_clear_session_key(explicit_key, resolved) else {
        report.fail(
            "provider_resume_fence",
            "no session key resolved for the channel",
        );
        return;
    };
    let channel_key = channel.get().to_string();
    let write = || async {
        provider_resume_fence::set_for_channel_pg(
            pool,
            provider.as_str(),
            CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &channel_key,
            },
            &key,
        )
        .await
        .map_err(anyhow::Error::msg)
    };
    if report
        .attempt("provider_resume_fence", write)
        .await
        .is_some()
    {
        report.conversation_reset = true;
    }
}
