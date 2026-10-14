use super::super::intake_dispatch::{IntakeAdmission, ledger as custody};
use crate::services::discord::{Error, SharedData, input_runtime as runtime};
use crate::services::discord::{
    bot_role::UtilityBotRole as Role, health::UtilityBotUserIdResolution as Resolution,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::rows::receipt_identity::Responsibility;
use custody::{DeferReason as Reason, Recovery};
use poise::serenity_prelude::{Channel, ChannelId, ChannelType, Http, Message, MessageType};
use runtime::intake::admit::{self, SourceKind};
use runtime::intake::client::{self, Access};
use runtime::intake::{Classification, Origin, PreparedSource, SourceDraft, StagedAttachment};
use runtime::supervisor::{ordering::ScanCapability, receipt::Deferred};
use sha2::{Digest, Sha256};
use std::sync::Arc;

fn derived_targets_enabled() -> bool {
    #[cfg(test)]
    if runtime::supervisor::mutant("c2b_mixed_reads_use_any_protected") {
        return runtime::fence::any_protected();
    }
    runtime::fence::boot_skip(runtime::fence::BootTarget::Unknown)
}

/// Ledger guards refuse destinations whose Legacy fence is closed.
/// Keep HTTP and voice state out of the enclosing intake futures.
pub(in crate::services::discord) fn legacy_allowed<'a>(
    shared: &'a Arc<SharedData>,
    http: &'a Arc<Http>,
    provider: &'a ProviderKind,
    channel: ChannelId,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<(), Error>> + Send + 'a>> {
    Box::pin(async move {
        #[cfg(test)]
        if runtime::supervisor::mutant("c2b_skip_final_legacy_checks") {
            return Ok(());
        }
        let unprotected = |channel: ChannelId| {
            if runtime::fence::channel_gate(channel.get()).is_some_and(|gate| {
                let held = gate.mode() != runtime::fence::Mode::LegacyOpen;
                #[cfg(test)]
                let held = if runtime::supervisor::mutant("c2b_final_guard_ignores_mode") {
                    true
                } else if runtime::supervisor::mutant("c2b_final_guard_only_legacy_open") {
                    !held
                } else {
                    held
                };
                held
            }) {
                Err::<(), Error>(
                    "input source remains pending; ledger admission is required".into(),
                )
            } else {
                Ok(())
            }
        };
        unprotected(channel)?;
        if !derived_targets_enabled() {
            return Ok(());
        }
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
    })
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

struct ScanCursor {
    client: Arc<runtime::intake::Client>,
    channel: u64,
    capability: Option<ScanCapability>,
    last_settled: Option<u64>,
    complete_after: bool,
    completion: Option<Result<(), Deferred>>,
}

type MessageAdmission = (Option<IntakeAdmission>, Option<ScanCursor>);

fn rescan(original: PreparedSource, reason: Reason) -> IntakeAdmission {
    custody::deferred(original, reason, Recovery::Rescan)
}

/// Raw delivery keeps its pending obligation; the same body consumes caller-held scans.
pub(in crate::services::discord) async fn hold_live(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    provider: &ProviderKind,
    message: &Message,
    kind: SourceKind,
    prompt: Option<String>,
) -> Result<bool, Error> {
    let result = prepare_message(shared, http, provider, message, kind, prompt, None).await?;
    debug_assert!(result.1.is_none());
    match result.0 {
        Some(admission) => Err(custody::retained(admission)),
        None => Ok(false),
    }
}

async fn preserves_message(shared: &Arc<SharedData>, message: &Message) -> bool {
    let roles = if let Some(registry) = shared.health_registry() {
        (
            registry
                .utility_bot_user_id_resolution(Role::Announce)
                .await,
            registry.utility_bot_user_id_resolution(Role::Notify).await,
        )
    } else {
        (Resolution::Unconfigured, Resolution::Unconfigured)
    };
    let allowed = shared.settings.read().await.allowed_bot_ids.clone();
    let preserves = !message.author.bot
        && !super::gate::live_sender_excluded_from_human_preservation(
            &allowed,
            message.author.id.get(),
            roles.0,
            roles.1,
        );
    #[cfg(all(test, unix))]
    let preserves = io_tests::preservation_for_test(preserves);
    preserves
}

async fn message_classification(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    message: &Message,
    source: &PreparedSource,
) -> Classification {
    #[cfg(all(test, unix))]
    let earlier = io_tests::policy_before_terminal(shared, http, message, source).await;
    let terminal = !preserves_message(shared, message).await
        && crate::services::discord::stale_dispatch_turn_for_text(
            shared.pg_pool.as_ref(),
            &message.content,
        )
        .await
        .is_some();
    let preserves = terminal && preserves_message(shared, message).await;
    let facts = runtime::source_policy_facts(shared, http, source).await;
    #[cfg(all(test, unix))]
    let (facts, preserves) = io_tests::restore_policy_snapshot(earlier, facts, preserves);
    let mut facts = facts;
    if terminal && !preserves {
        facts.disposition = Some(runtime::intake::InputDisposition::Terminal);
    }
    runtime::intake::classify(&facts, source)
}

async fn prepare_message(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    provider: &ProviderKind,
    message: &Message,
    mut kind: SourceKind,
    prompt: Option<String>,
    mut scan: Option<ScanCursor>,
) -> Result<MessageAdmission, Error> {
    #[cfg(test)]
    if runtime::supervisor::mutant("c2b_bypass_live_gate") {
        return Ok((None, scan));
    }
    if scan.is_none()
        && runtime::fence::channel_gate(message.channel_id.get()).is_none()
        && !derived_targets_enabled()
    {
        return Ok((None, scan));
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
    let initial = if scan.is_none() {
        admit::begin_live(
            runtime::runtime::registry(),
            provider,
            original.key(),
            original.identity(),
        )
        .await
        .map_err(|reason| custody::retained(rescan(original.clone(), Reason::Receipt(reason))))?
    } else {
        None
    };
    let facts = runtime::source_policy_facts(shared, http, &original).await;
    let execution = facts.execution.channel_id;
    let prepared = admit::prepare(
        kind,
        draft(message, execution, prompt.clone()),
        Some(message_kind),
    )?;
    let facts = runtime::source_policy_facts(shared, http, &prepared).await;
    if runtime::intake::classify(&facts, &prepared) == Classification::Legacy {
        return Ok((None, scan));
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
    let target = match &facts.voice_target {
        runtime::intake::VoiceTarget::Resolved(target)
            if !matches!(target.protection, runtime::intake::Protection::Unprotected) =>
        {
            target.channel_id
        }
        _ => execution,
    };
    let mut routed = prepared.identity().clone();
    routed.execution_channel_id = target;
    let _pending = if scan.is_none() && initial.is_none() {
        admit::begin_live(
            runtime::runtime::registry(),
            provider,
            prepared.key(),
            &routed,
        )
        .await
        .map_err(|reason| custody::retained(rescan(prepared.clone(), Reason::Receipt(reason))))?
    } else {
        None
    };
    let mut material = draft(message, execution, prompt);
    let mut actual = admit::prepare(
        kind,
        draft(message, execution, material.prompt.clone()),
        Some(message_kind),
    )?;
    let mut classification = message_classification(shared, http, message, &actual).await;
    if classification == Classification::NonTarget && scan.is_none() {
        return Ok((None, scan));
    }
    let client = match client::lookup(runtime::runtime::registry(), provider, execution) {
        Access::Ready(client) => client,
        Access::Deferred(reason) => {
            return Err(custody::retained(rescan(actual, Reason::Receipt(reason))));
        }
        Access::Legacy => {
            return Err(custody::retained(rescan(
                actual,
                Reason::Receipt(Deferred::Conflict),
            )));
        }
    };
    if scan
        .as_ref()
        .is_some_and(|cursor| cursor.channel != execution || !Arc::ptr_eq(&cursor.client, &client))
    {
        return Err(custody::retained(rescan(
            actual,
            Reason::Receipt(Deferred::Conflict),
        )));
    }
    if classification == Classification::Target {
        let lookup = client.lookup_responsibility(actual.identity().clone());
        #[cfg(all(test, unix))]
        let lookup = io_tests::lookup_or_absent(lookup);
        let lookup = lookup.await;
        let absent = matches!(&lookup, Ok(value) if value.responsibility == Responsibility::Absent && value.receipt.is_none());
        if !absent {
            let admission = custody::lookup_for_rescan(actual.clone(), lookup);
            if scan.is_none() || !matches!(admission, IntakeAdmission::LedgerReceived(_)) {
                return Ok((Some(admission), scan));
            }
        } else {
            material = enrich(material, message, http).await?;
            actual = admit::prepare(kind, material, Some(message_kind))?;
        }
        classification = message_classification(shared, http, message, &actual).await;
    }
    let Some(mut cursor) = scan.take() else {
        return Ok((
            Some(rescan(actual, Reason::Classification(classification))),
            scan,
        ));
    };
    let requested_last = actual.identity().source_ids.last().copied();
    let outcome = custody::consume_scan(
        runtime::runtime::registry(),
        provider,
        actual,
        classification,
        cursor
            .capability
            .take()
            .ok_or("scan capability unavailable; rescan required")?,
    )
    .await;
    let (admission, capability, settled) = match outcome {
        custody::ScanAdmission::Admission {
            admission,
            capability,
        } => {
            let settled = custody::settled(&admission);
            (Some(admission), capability, settled)
        }
        custody::ScanAdmission::NonTarget(capability) => (None, Some(capability), true),
        custody::ScanAdmission::Legacy {
            original,
            capability,
        } => (
            Some(rescan(
                original,
                Reason::Classification(Classification::Legacy),
            )),
            Some(capability),
            false,
        ),
    };
    cursor = ScanCursor {
        capability,
        ..cursor
    };
    if settled {
        cursor.last_settled = requested_last;
    }
    if settled
        && cursor.complete_after
        && let Some(capability) = cursor.capability.take()
    {
        cursor.completion = Some(cursor.client.complete_scan(capability).await.map(|_| ()));
        if cursor.completion == Some(Ok(()))
            && let Some(last) = cursor.last_settled
        {
            crate::services::discord::advance_last_message_checkpoint(
                shared,
                provider,
                ChannelId::new(cursor.channel),
                poise::serenity_prelude::MessageId::new(last),
            );
        }
    }
    Ok((admission, Some(cursor)))
}

pub(super) async fn terminal_feedback(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    provider: &ProviderKind,
    message: &Message,
    stale: crate::services::discord::dispatch_policy::StaleDispatchTurn,
) {
    tracing::info!(message_id = %message.id, dispatch = %stale.dispatch_id, status = %stale.status,
        "skipped terminal dispatch message");
    let checkpoint_allowed = legacy_allowed(shared, http, provider, message.channel_id)
        .await
        .is_ok();
    #[cfg(all(test, unix))]
    let checkpoint_allowed = io_tests::checkpoint_for_test(checkpoint_allowed);
    if checkpoint_allowed {
        crate::services::discord::advance_last_message_checkpoint(
            shared,
            provider,
            message.channel_id,
            message.id,
        );
    }
    let emoji = crate::services::discord::queue_exit_feedback_emoji(stale.queue_exit_kind);
    crate::services::discord::queue_marker::note_exit_feedback_added(
        shared,
        http,
        message.channel_id,
        message.id,
        emoji,
    )
    .await;
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
    #[cfg(test)]
    if runtime::supervisor::mutant("c2b_final_policy_before_raw_fence") {
        legacy_allowed(deps.shared, deps.http, &deps.shared.provider, channel).await?;
    }
    let permit = runtime::fence::effect::admit(&deps.shared.provider, channel.get())
        .map_err(|failure| -> Error { format!("input admission refused: {failure:?}").into() })?;
    legacy_allowed(deps.shared, deps.http, &deps.shared.provider, channel).await?;
    Ok(permit)
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

#[cfg(all(test, unix))]
#[path = "ledger_io_tests.rs"]
mod io_tests;

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod ledger_tests;
