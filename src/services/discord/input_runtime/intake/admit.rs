//! Original source factories and live pending obligations on the existing supervisor.

use std::io;
use std::sync::Arc;

use super::client::{Access, Client, lookup};
use super::{Origin, PreparedSource, SourceDraft, prepare_source};
use crate::services::discord::input_runtime::supervisor::{
    Registry, external, ordering::PendingOverflow, receipt::Deferred,
};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::input_key::{external_key_v1, is_discord_key};
use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum SourceKind {
    LiveMessage,
    RawAttachment,
    TextSkill,
    SlashSkill,
}

impl SourceKind {
    fn label(self) -> &'static str {
        match self {
            Self::LiveMessage => "live_message",
            Self::RawAttachment => "raw_attachment",
            Self::TextSkill => "text_skill",
            Self::SlashSkill => "slash_skill",
        }
    }
}

pub(in crate::services::discord) fn prepare(
    kind: SourceKind,
    draft: SourceDraft,
    message_kind: Option<&str>,
) -> io::Result<PreparedSource> {
    let valid = match (&draft.origin, kind, message_kind) {
        (
            Origin::Message {
                message_id,
                source_ids,
                ..
            },
            SourceKind::LiveMessage | SourceKind::RawAttachment | SourceKind::TextSkill,
            Some("regular" | "inline_reply"),
        ) => is_discord_key(*message_id) && source_ids.iter().copied().all(is_discord_key),
        (Origin::SlashInteraction { interaction_id, .. }, SourceKind::SlashSkill, None) => {
            is_discord_key(*interaction_id)
        }
        _ => false,
    };
    #[cfg(test)]
    let valid = valid
        || crate::services::discord::input_runtime::supervisor::mutant(
            "c2b_kind_accepts_wrong_origin",
        );
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source kind does not match original provenance",
        ));
    }
    Ok(prepare_source(draft)?.label_kind(kind.label(), message_kind))
}

pub(in crate::services::discord) struct LivePending<'a> {
    client: Arc<Client>,
    original: &'a ReceiptIdentity,
    overflow: PendingOverflow,
}

impl LivePending<'_> {
    pub(in crate::services::discord) fn client(&self) -> &Client {
        &self.client
    }
    pub(in crate::services::discord) fn identity(&self) -> &ReceiptIdentity {
        self.original
    }
}

impl Drop for LivePending<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        if crate::services::discord::input_runtime::supervisor::mutant(
            "c2b_drop_live_pending_obligation",
        ) {
            return;
        }
        self.overflow.mark_dirty();
    }
}

/// Call before reply or attachment IO; live material remains a pending scan obligation.
pub(in crate::services::discord) async fn begin_live<'a>(
    registry: &'static Registry,
    provider: &ProviderKind,
    key: u64,
    original: &'a ReceiptIdentity,
) -> Result<Option<LivePending<'a>>, Deferred> {
    let client = match lookup(registry, provider, original.execution_channel_id) {
        Access::Legacy => return Ok(None),
        Access::Deferred(reason) => return Err(reason),
        Access::Ready(client) => client,
    };
    if !original.validates(key)
        || !is_discord_key(key)
        || !original.source_ids.iter().copied().all(is_discord_key)
    {
        return Err(Deferred::Conflict);
    }
    let sources = original.source_ids.clone();
    #[cfg(test)]
    let sources = if crate::services::discord::input_runtime::supervisor::mutant(
        "c2b_skip_live_pending_registration",
    ) {
        vec![]
    } else {
        sources
    };
    let pending = client.pending(sources).await?;
    Ok(Some(LivePending {
        client,
        original,
        overflow: pending.overflow,
    }))
}

pub(in crate::services::discord) fn begin_external(
    registry: &'static Registry,
    provider: &ProviderKind,
    channel: u64,
) -> Result<Option<Arc<Client>>, Deferred> {
    match lookup(registry, provider, channel) {
        Access::Legacy => Ok(None),
        Access::Ready(client) => Ok(Some(client)),
        Access::Deferred(reason) => Err(reason),
    }
}

/// The caller borrows its entire source; only the supervisor's durable receipt is a success.
pub(in crate::services::discord) async fn submit_slash(
    client: &Client,
    prepared: &PreparedSource,
) -> Result<external::ExternalReceipt, Deferred> {
    let input = prepared.input();
    let origin = input["http_origin"]["origin_id"].as_str().unwrap_or("");
    let valid = input["source_kind"] == "slash_skill"
        && input["message_kind"].is_null()
        && input["http_origin"]["version"].as_u64() == Some(1)
        && input["http_origin"]["source"] == "discord-slash-interaction-v1"
        && origin.parse::<u64>().ok().is_some_and(is_discord_key)
        && prepared.identity().source_ids == [prepared.key()]
        && prepared.key()
            == external_key_v1(
                prepared.identity().execution_channel_id,
                "discord-slash-interaction-v1",
                origin,
            );
    #[cfg(test)]
    let valid = valid
        || crate::services::discord::input_runtime::supervisor::mutant(
            "c2b_slash_origin_validation_omitted",
        );
    if !valid {
        return Err(Deferred::Conflict);
    }
    client.submit_external(prepared).await
}

#[cfg(test)]
#[path = "admit_tests.rs"]
mod tests;
