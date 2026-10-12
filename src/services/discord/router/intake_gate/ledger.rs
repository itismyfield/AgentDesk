use crate::services::discord::{Error, SharedData, input_runtime as runtime};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{Channel, ChannelId, ChannelType, Http, Message, MessageType};
use runtime::intake::admit::{self, SourceKind};
use runtime::intake::{Classification, Origin, SourceDraft, StagedAttachment};
use sha2::{Digest, Sha256};
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

fn draft(message: &Message, execution: u64, prompt: String) -> SourceDraft {
    SourceDraft {
        origin: Origin::Message {
            message_id: message.id.get(),
            source_ids: vec![message.id.get()],
            author_id: message.author.id.get(),
            original_channel_id: message.channel_id.get(),
            execution_channel_id: execution,
        },
        author_is_bot: message.author.bot,
        original_text: message.content.clone(),
        prompt,
        reply_context: None,
        attachments: message
            .attachments
            .iter()
            .map(|attachment| StagedAttachment {
                filename: attachment.filename.clone(),
                url: attachment.url.clone(),
                staged: None,
            })
            .collect(),
        voice_announcement: crate::voice::announce_meta::global_store().peek_clone(message.id),
    }
}

async fn fetch_attachment(url: &str) -> Result<Vec<u8>, Error> {
    let parsed = url::Url::parse(url)?;
    if parsed.scheme() != "https"
        || !matches!(
            parsed.host_str(),
            Some("cdn.discordapp.com" | "media.discordapp.net")
        )
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return Err("attachment URL is not an allowed Discord CDN target".into());
    }
    #[cfg(test)]
    let url = std::env::var("ADK_C2B_TEST_ATTACHMENT_URL").unwrap_or_else(|_| url.into());
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let limit = crate::services::cluster::attachment_transfer::store::MAX_FILE_BYTES;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|len| len > limit as u64)
    {
        return Err("attachment response is incomplete or oversized".into());
    }
    let mut bytes = vec![];
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err("attachment exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Register the original before reply/download IO; only a later valid scan can settle it.
pub(in crate::services::discord) async fn hold_live(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    provider: &ProviderKind,
    message: &Message,
    mut kind: SourceKind,
    prompt: Option<String>,
) -> Result<bool, Error> {
    #[cfg(test)]
    if runtime::supervisor::mutant("c2b_bypass_live_gate") {
        return Ok(false);
    }
    if !runtime::fence::any_protected() {
        return Ok(false);
    }
    let message_kind = match message.kind {
        MessageType::Regular => "regular",
        MessageType::InlineReply => "inline_reply",
        _ => return Err("unsupported original Discord message kind".into()),
    };
    let mut prompt = prompt.unwrap_or_else(|| message.content.clone());
    let original = admit::prepare(
        kind,
        draft(message, message.channel_id.get(), prompt.clone()),
        Some(message_kind),
    )?;
    let initial = admit::begin_live(
        runtime::runtime::registry(),
        provider,
        original.key(),
        original.identity(),
    )
    .await
    .map_err(|reason| std::io::Error::other(format!("original pending unavailable: {reason:?}")))?;
    let facts = runtime::source_policy_facts(shared, http, &original).await;
    let execution = facts.execution.channel_id;
    let mut material = draft(message, execution, prompt.clone());
    let prepared = admit::prepare(
        kind,
        draft(message, execution, material.prompt.clone()),
        Some(message_kind),
    )?;
    let facts = runtime::source_policy_facts(shared, http, &prepared).await;
    let classification = runtime::intake::classify(&facts, &prepared);
    if classification == Classification::Legacy {
        return Ok(false);
    }
    if kind == SourceKind::LiveMessage {
        if let Some(skill) =
            runtime::intake::skills::protected_text_skill_source(shared, &message.content).await
        {
            kind = SourceKind::TextSkill;
            prompt = skill?;
        } else if message.content.trim().is_empty() && !message.attachments.is_empty() {
            kind = SourceKind::RawAttachment;
        }
    }
    material = draft(message, execution, prompt);
    let identity = prepared.identity();
    let target = match &facts.voice_target {
        runtime::intake::VoiceTarget::Resolved(target)
            if !matches!(target.protection, runtime::intake::Protection::Unprotected) =>
        {
            target.channel_id
        }
        _ => execution,
    };
    let mut routed = identity.clone();
    routed.execution_channel_id = target;
    let _pending = if initial.is_none() {
        admit::begin_live(
            runtime::runtime::registry(),
            provider,
            prepared.key(),
            &routed,
        )
        .await
        .map_err(|reason| {
            std::io::Error::other(format!("routed pending unavailable: {reason:?}"))
        })?
    } else {
        None
    };
    let actual = admit::prepare(
        kind,
        draft(message, execution, material.prompt.clone()),
        Some(message_kind),
    )?;
    let facts = runtime::source_policy_facts(shared, http, &actual).await;
    let classification = runtime::intake::classify(&facts, &actual);
    if classification != Classification::Target {
        return Err(format!("original input held: {classification:?}").into());
    }
    material = enrich(material, message, http).await?;
    let staged = admit::prepare(kind, material, Some(message_kind))?;
    let final_facts = runtime::source_policy_facts(shared, http, &staged).await;
    Err(format!(
        "original input remains pending for a valid scan: {:?}",
        runtime::intake::classify(&final_facts, &staged)
    )
    .into())
}

async fn enrich(
    mut material: SourceDraft,
    message: &Message,
    http: &Arc<Http>,
) -> Result<SourceDraft, Error> {
    let reply_needed = message.message_reference.is_some();
    #[cfg(test)]
    let reply_needed = reply_needed && !runtime::supervisor::mutant("c2b_live_reply_omitted");
    if reply_needed && let Some(reference) = message.message_reference.as_ref() {
        let reply = match message.referenced_message.as_deref() {
            Some(reply) => reply.clone(),
            None => {
                reference
                    .channel_id
                    .message(
                        http,
                        reference.message_id.ok_or("reply identity unavailable")?,
                    )
                    .await?
            }
        };
        material.reply_context = Some(serde_json::to_string(&reply)?);
    }
    let stage = true;
    #[cfg(test)]
    let stage = stage && !runtime::supervisor::mutant("c2b_live_staging_omitted");
    for attachment in &mut material.attachments {
        if !stage {
            continue;
        }
        let bytes = fetch_attachment(&attachment.url).await?;
        attachment.staged = Some(
            crate::services::cluster::attachment_transfer::AttachmentEntryV1 {
                filename: attachment.filename.clone(),
                sha256: hex::encode(Sha256::digest(&bytes)),
                bytes,
            },
        );
    }
    Ok(material)
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

#[cfg(test)]
#[path = "ledger_io_tests.rs"]
mod io_tests;

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod ledger_tests;
