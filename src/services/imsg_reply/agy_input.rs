//! The server's custody of an authenticated Antigravity iMessage original.

use std::{future::Future, num::NonZeroU64, path::PathBuf};

use sqlx::PgPool;

use crate::db::imsg_reply::Obligation;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::store::agy_spool::ImessageSource;

use super::{AdmissionInput, ReplyObligation, admission};

const HASH_PREFIX: &str = "v3:agy:";
pub(crate) const QUEUED_ORIGIN: &str = "agy_imessage_http";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WriterSelection {
    pub(crate) root: PathBuf,
    pub(crate) destination: u64,
    pub(crate) logical_channel: u64,
    pub(crate) generation: NonZeroU64,
}

#[derive(Clone)]
pub(crate) struct WriterGate {
    destination: u64,
    pub(crate) ready: Option<WriterSelection>,
}

#[cfg(test)]
impl WriterGate {
    pub(crate) fn ready(writer: WriterSelection) -> Self {
        Self {
            destination: writer.destination,
            ready: Some(writer),
        }
    }

    pub(crate) fn unready(destination: u64) -> Self {
        Self {
            destination,
            ready: None,
        }
    }
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static SELECTED_WRITER: WriterGate;
}

pub(crate) fn selected(provider: &ProviderKind, destination: u64) -> Option<WriterGate> {
    if *provider != ProviderKind::Antigravity || destination == 0 {
        return None;
    }
    #[cfg(test)]
    let writer = SELECTED_WRITER.try_with(Clone::clone).ok();
    #[cfg(not(test))]
    // The cutover supplies a verified writer generation here.
    let writer: Option<WriterGate> = None;
    writer.filter(|gate| gate.destination == destination)
}

pub(crate) fn ready(provider: &ProviderKind, destination: u64) -> Option<WriterSelection> {
    selected(provider, destination)?.ready
}

pub(crate) fn request_hash(input: &AdmissionInput<'_>, logical_channel: u64) -> String {
    format!(
        "{HASH_PREFIX}{logical_channel}:{}",
        admission::request_hash(input)
    )
}

pub(super) fn split_hash(hash: &str) -> (Option<u64>, &str) {
    if let Some((parent, canonical)) = hash
        .strip_prefix(HASH_PREFIX)
        .and_then(|value| value.split_once(':'))
        && let Ok(parent) = parent.parse()
    {
        (Some(parent), canonical)
    } else {
        (None, hash)
    }
}

#[derive(Clone)]
pub(crate) struct Admitted {
    selection: WriterSelection,
    source: ImessageSource,
    guid: String,
    author: u64,
    binding: ReplyObligation,
}

impl Admitted {
    pub(crate) fn new(
        selection: WriterSelection,
        source: ImessageSource,
        guid: String,
        author: u64,
        binding: ReplyObligation,
    ) -> Self {
        Self {
            selection,
            source,
            guid,
            author,
            binding,
        }
    }

    pub(crate) fn binding(&self) -> &ReplyObligation {
        &self.binding
    }

    pub(crate) fn for_message(&self, message: u64) -> ClaimedInput {
        ClaimedInput {
            selection: self.selection.clone(),
            source: self.source.clone(),
            guid: self.guid.clone(),
            author: self.author,
            message,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClaimedInput {
    pub(crate) selection: WriterSelection,
    pub(crate) source: ImessageSource,
    pub(crate) guid: String,
    pub(crate) author: u64,
    pub(crate) message: u64,
}

tokio::task_local! {
    pub(crate) static ADMITTED_DELIVERY: Admitted;
    static CLAIMED_INPUT: Option<ClaimedInput>;
}

pub(crate) fn claimed() -> Option<ClaimedInput> {
    CLAIMED_INPUT.try_with(Clone::clone).ok().flatten()
}

pub(crate) async fn with_claim<T>(
    input: Option<ClaimedInput>,
    future: impl Future<Output = T>,
) -> T {
    CLAIMED_INPUT.scope(input, future).await
}

/// A worker payload may omit the provenance. The exact queue binding still
/// identifies it, so it cannot enter the Discord worker as a legacy input.
pub(crate) async fn bound_row(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel: u64,
    message: u64,
    include_started: bool,
) -> Result<Option<Obligation>, sqlx::Error> {
    let Some(pool) = pool.filter(|_| {
        *provider == ProviderKind::Antigravity
            && !crate::services::tui_input::input_key::is_discord_key(message)
    }) else {
        return Ok(None);
    };
    let episode = include_started.then(|| super::episode::turn(provider, channel, message));
    crate::db::imsg_reply::read_agy_binding(pool, channel, message, episode.as_deref()).await
}

pub(crate) fn restore_queued(
    row: &Obligation,
    provider: &ProviderKind,
    channel: u64,
    message: u64,
    author: u64,
    text: &str,
) -> Option<ClaimedInput> {
    let selection = ready(provider, channel)?;
    let input = AdmissionInput {
        origin_id: &row.origin_id,
        agent_id: &row.agent_id,
        provider: provider.as_str(),
        channel_id: channel,
        author_id: author,
        source: "imessage",
        text,
    };
    if row.person_id != i64::try_from(author).ok()?
        || row.channel_id != channel.to_string()
        || row.queued_message_id.as_deref() != Some(message.to_string().as_str())
        || !matches!(row.admission.as_str(), "admitting" | "admitted" | "unknown")
        || row.episode_key.is_some()
        || row.request_hash != request_hash(&input, selection.logical_channel)
    {
        return None;
    }
    Some(ClaimedInput {
        selection,
        source: ImessageSource::from_guid(input.origin_id)?,
        guid: row.origin_id.clone(),
        author,
        message,
    })
}
