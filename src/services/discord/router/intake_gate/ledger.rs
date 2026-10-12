use crate::services::discord::{Error, SharedData, input_runtime as runtime};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{Channel, ChannelId, ChannelType, Http};
use std::sync::Arc;

/// Legacy effects require confirmed absence of protection on every real destination.
pub(in crate::services::discord) async fn legacy_allowed(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Result<(), Error> {
    #[cfg(test)]
    if runtime::supervisor::mutant("c2b_skip_final_legacy_checks") {
        return Ok(());
    }
    if !runtime::fence::any_protected() {
        return Ok(());
    }
    let unprotected = |channel: ChannelId| {
        if runtime::fence::channel_gate(channel.get()).is_some() {
            Err::<(), Error>("input source remains pending; ledger admission is required".into())
        } else {
            Ok(())
        }
    };
    unprotected(channel)?;
    let actual = channel.to_channel(http).await?;
    if let Channel::Guild(actual) = actual {
        if matches!(
            actual.kind,
            ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
        ) {
            unprotected(actual.parent_id.ok_or("thread target unavailable")?)?;
        }
    }
    for target in [
        shared
            .voice_barge_in
            .text_reply_target(channel, provider)
            .await,
        shared
            .voice_barge_in
            .announcement_target(channel, provider)
            .await,
    ] {
        if let Some(target) = target.map_err(std::io::Error::other)? {
            unprotected(target)?;
        }
    }
    Ok(())
}

pub(in crate::services::discord) async fn legacy_effect(
    deps: &crate::services::discord::router::message_handler::IntakeDeps<'_>,
    channel: ChannelId,
) -> Result<Option<runtime::fence::Permit>, Error> {
    legacy_allowed(deps.shared, deps.http, &deps.shared.provider, channel).await?;
    runtime::fence::effect::admit(&deps.shared.provider, channel.get())
        .map_err(|failure| format!("input admission refused: {failure:?}").into())
}

pub(in crate::services::discord) async fn try_text_reply(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    channel: ChannelId,
    text: &str,
) -> Result<bool, Error> {
    legacy_allowed(shared, http, &shared.provider, channel).await?;
    Ok(shared
        .voice_barge_in
        .try_handle_voice_channel_text_reply(http, channel, text, &shared.provider)
        .await)
}
