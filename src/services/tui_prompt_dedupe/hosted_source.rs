//! The observed native source's execution, carried unchanged while its claim waits.

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
use crate::services::discord::SharedData;
use crate::services::session_host::herdr_endpoints;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::binding_context::{
    self, BindingContext, CapturedContext, HookBindingEnvelope, SpawnNonceMarker,
};
use super::binding_events::{self, BindingTarget, SourceId};
use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::session_host::HostKind;
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};
use crate::services::tui_o::shadow::binding_reader::source_id_for;

mod observation;
pub(crate) use observation::observe_hook_at;
#[cfg(unix)]
pub(crate) use observation::{observe_claude_record_at, observe_codex_record_at};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HostedSourcePin {
    pub(crate) provider: String,
    pub(crate) channel_id: u64,
    pub(crate) logical_key: String,
    pub(crate) execution_nonce: String,
    #[serde(default = "default_hosted_admission_required")]
    pub(crate) hosted_admission_required: bool,
    pub(crate) source: SourceId,
}

fn default_hosted_admission_required() -> bool {
    true
}

/// Only affirmative host evidence selects hosted admission; unreadable evidence stays unresolved.
pub(crate) fn herdr_evidence(
    channel: u64,
    logical: &str,
    record: Option<&HostedRecord>,
) -> Result<bool, &'static str> {
    let marker = read_host_kind_marker(logical);
    if matches!(
        marker,
        HostKindMarker::ReadFailed(_) | HostKindMarker::Unrecognized(_)
    ) {
        return Err("synthetic host marker is unreadable or unrecognized");
    }
    if matches!(record, Some(HostedRecord::Unknown(_))) {
        return Err("synthetic hosted execution is unreadable");
    }
    Ok(
        crate::config::session_hosts::herdr_endpoint(channel).is_some()
            || super::herdr_execution_listed(logical)
            || marker == HostKindMarker::Known(HostKind::Herdr)
            || matches!(record, Some(HostedRecord::Known(row)) if row.state != HostedState::Retired),
    )
}

/// Reads the original file's binding history; a current binding cannot supply a missing origin.
pub(crate) fn capture(
    provider: &str,
    logical: &str,
    channel: u64,
    path: &Path,
    native_session: Option<&str>,
) -> Option<HostedSourcePin> {
    capture_native(provider, logical, channel, Some(path), native_session, None)
}

/// The provider's captured launch context disambiguates a source reused by a resume.
pub(crate) fn capture_in_context(
    provider: &str,
    logical: &str,
    channel: u64,
    path: &Path,
    native_session: Option<&str>,
    context: &BindingContext,
) -> Option<HostedSourcePin> {
    context_matches(provider, logical, channel, context).then_some(())?;
    capture_native(
        provider,
        logical,
        channel,
        Some(path),
        native_session,
        Some(&context.execution_nonce),
    )
}

fn context_matches(provider: &str, logical: &str, channel: u64, context: &BindingContext) -> bool {
    context.provider == provider
        && context.tmux_session == logical
        && context.channel_id == Some(channel)
        && context.owner_runtime_root == crate::services::tmux_common::current_tmux_owner_marker()
        && binding_context::execution_context(provider, &context.execution_nonce).as_ref()
            == Ok(context)
}

/// Capture before a scanner opens the file; its immutable selector names the execution.
pub(crate) fn scan_context_from_path(
    provider: &str,
    logical: &str,
    channel: u64,
    path: Option<&Path>,
) -> Option<BindingContext> {
    let path = path?;
    let CapturedContext::Captured(context) =
        HookBindingEnvelope::capture_from_env(provider, |key| {
            (key == "AGENTDESK_BINDING_CONTEXT").then(|| path.as_os_str().to_owned())
        })
        .context
    else {
        return None;
    };
    context_current(provider, logical, channel, &context).then_some(context)
}

fn context_current(provider: &str, logical: &str, channel: u64, context: &BindingContext) -> bool {
    crate::services::tmux_common::with_tmux_source_authority(logical, |_| {
        context_matches(provider, logical, channel, context)
            && binding_context::observe_spawn_nonce_marker(logical)
                == SpawnNonceMarker::Known(context.execution_nonce.clone())
    })
}

/// A scanner's record must belong to its captured execution, even if older history is missing.
pub(crate) fn capture_scanned(
    provider: &str,
    logical: &str,
    channel: u64,
    path: &Path,
    context: Option<&BindingContext>,
    record_created_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<HostedSourcePin> {
    let Some(context) = context else {
        return capture(provider, logical, channel, path, None);
    };
    context_current(provider, logical, channel, context).then_some(())?;
    let pin = capture_in_context(provider, logical, channel, path, None, context)?;
    let Some(created) = record_created_at else {
        static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                event = "hosted_source_scanned_record_untimed",
                provider,
                channel_id = channel,
                path = %path.display(),
                execution_nonce = %context.execution_nonce,
                "a scanned source record has no native creation time"
            );
        }
        return None;
    };
    if created < context.created_at {
        return None;
    }
    Some(pin)
}

pub(crate) fn claude_record_created_at(
    path: &Path,
    start: u64,
) -> Option<chrono::DateTime<chrono::Utc>> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut line = String::new();
    file.read_line(&mut line).ok()?;
    let row: serde_json::Value = serde_json::from_str(&line).ok()?;
    chrono::DateTime::parse_from_rfc3339(row.get("timestamp")?.as_str()?)
        .ok()
        .map(|at| at.with_timezone(&chrono::Utc))
}

/// Rechecks the execution already carried by a source; it never chooses a newer execution.
pub(crate) fn validate(pin: &HostedSourcePin) -> bool {
    capture_native(
        &pin.provider,
        &pin.logical_key,
        pin.channel_id,
        Some(&pin.source.path),
        Some(&pin.source.session_id),
        Some(&pin.execution_nonce),
    )
    .is_some_and(|mut observed| {
        // Host evidence belongs to the original capture, not a later source validation.
        observed.hosted_admission_required = pin.hosted_admission_required;
        observed == *pin
    })
}

/// An exact saved episode supplies the original native session and execution nonce.
pub(crate) fn restore(
    provider: &str,
    logical: &str,
    channel: u64,
    captured_source: &(String, u64),
    (native_session, execution_nonce): (&str, &str),
) -> Option<HostedSourcePin> {
    let path = Path::new(&captured_source.0);
    (std::fs::metadata(path).ok()?.len() >= captured_source.1).then_some(())?;
    capture_native(
        provider,
        logical,
        channel,
        Some(path),
        Some(native_session),
        Some(execution_nonce),
    )
}

fn capture_native(
    provider: &str,
    logical: &str,
    channel: u64,
    path: Option<&Path>,
    native_session: Option<&str>,
    expected_nonce: Option<&str>,
) -> Option<HostedSourcePin> {
    let provider = provider.trim().to_ascii_lowercase();
    let native_session = native_session.map(str::trim).filter(|id| !id.is_empty());
    if channel == 0 || logical.trim().is_empty() || (path.is_none() && native_session.is_none()) {
        return None;
    }
    let events = binding_events::records_strict(channel).ok()?.ok()?;
    let hosted_admission_required = herdr_evidence(channel, logical, None).ok()?;
    let mut pinned = None;
    for event in events {
        if event.provider != provider
            || event.channel_id != channel
            || event.tmux_session != logical
            || expected_nonce.is_some_and(|nonce| event.execution_nonce.as_deref() != Some(nonce))
        {
            continue;
        }
        let source = match event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
            _ => continue,
        };
        if path.is_some_and(|path| source.path != path)
            || native_session.is_some_and(|session| source.session_id != session)
        {
            continue;
        }
        let execution_nonce = event.execution_nonce.filter(|nonce| !nonce.is_empty())?;
        if source_id_for(&source.session_id, &source.path).ok()? != source {
            return None;
        }
        let candidate = HostedSourcePin {
            provider: provider.clone(),
            channel_id: channel,
            logical_key: logical.to_owned(),
            execution_nonce,
            hosted_admission_required,
            source,
        };
        if pinned.as_ref().is_some_and(|old| old != &candidate) {
            return None;
        }
        pinned = Some(candidate);
    }
    pinned
}

/// A hook's routing id can outlive a native clear; only its payload names the observed source.
pub(crate) fn capture_hook(
    provider: &str,
    logical: &str,
    channel: u64,
    payload: &serde_json::Value,
    context: Option<&BindingContext>,
) -> Option<HostedSourcePin> {
    let text = |key| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let session = text("session_id").or_else(|| text("sessionId"));
    let path = text("transcript_path")
        .filter(|path| !path.trim().is_empty())
        .map(Path::new);
    if let Some(context) = context {
        context_matches(provider, logical, channel, context).then_some(())?;
        capture_native(
            provider,
            logical,
            channel,
            path,
            session,
            Some(&context.execution_nonce),
        )
    } else {
        capture_native(provider, logical, channel, path, session, None)
    }
}

#[cfg(all(test, unix))]
#[path = "hosted_source/source_tests.rs"]
pub(crate) mod tests;

pub(crate) async fn adoption_channel(
    shared: &Arc<SharedData>,
    prompt: &super::ObservedTuiPrompt,
    candidate: Option<ChannelId>,
) -> Option<ChannelId> {
    let Some(channel_id) = candidate else {
        tracing::debug!(
            provider = %prompt.provider,
            tmux_session_name = %prompt.tmux_session_name,
            "skipping SSH-direct TUI prompt notify; no Discord channel mapping"
        );
        return None;
    };
    if !current_execution_for_adoption(
        shared,
        &prompt.provider,
        channel_id,
        &prompt.tmux_session_name,
    )
    .await
    {
        return None;
    }
    Some(channel_id)
}

// New prompt adoption needs a live execution; admitted readers keep their tail.
pub(crate) async fn current_execution_for_adoption(
    shared: &Arc<SharedData>,
    provider: &str,
    channel: ChannelId,
    logical: &str,
) -> bool {
    match herdr_evidence(channel.get(), logical, None) {
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

pub(crate) async fn execution_current(
    record: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
) -> bool {
    let Some(target) = herdr_endpoints().target(record) else {
        return false;
    };
    tokio::task::spawn_blocking(move || target.execution_alive())
        .await
        .unwrap_or(false)
}
