//! A committed clear's writes and teardown: each failure is reported, none undoes the clear.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use tokio::sync::OwnedMutexGuard;

use super::super::super::SharedData;
use super::{ClearReport, ManagedSessionClearBehavior, managed_session_clear_behavior};
use crate::db::session_transcripts;
use crate::services::provider::ProviderKind;

/// Advances the channel's transcript clear boundary, retried once; it also supersedes any
/// unresolved native clear boundary, so no older clear holds the next input.
pub(super) async fn write_boundary(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
    report: &mut ClearReport,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        report.fail("pg_boundary", "postgres pool unavailable");
        return;
    };
    let key = channel_id.get().to_string();
    let key = key.as_str();
    let write = move || async move {
        let tx = session_transcripts::begin_channel_clear_boundary_tx(pool).await?;
        session_transcripts::finish_channel_clear_boundary_tx(tx, key).await
    };
    report.attempt("pg_boundary", write).await;
}

/// Forgets the stored provider session so the next launch starts fresh instead of resuming it.
pub(super) async fn forget_selector(shared: &SharedData, key: &str, report: &mut ClearReport) {
    use super::super::super::adk_session;
    let Some(pool) = shared.pg_pool.as_ref() else {
        // Without a pool the API clear stays best-effort; the skipped boundary already warns.
        adk_session::clear_provider_session_id(key, shared.api_port).await;
        return;
    };
    let forget = || async {
        let cleared = adk_session::clear_provider_session_id_pg(pool, key).await;
        cleared.map_err(anyhow::Error::msg)
    };
    report.attempt("session_selector", forget).await;
}

/// Ends a live Herdr execution's provider CLI through the terminate service, otherwise the managed
/// process and tmux; the clear's transition guard is held until then (terminate takes its own).
pub(super) async fn end_session(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    (tmux, session_key): (Option<&str>, Option<&str>),
    mut transition: Option<OwnedMutexGuard<()>>,
    report: &mut ClearReport,
) {
    #[cfg(unix)]
    if crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some()
        && end_herdr(shared, session_key, &mut transition, report).await
    {
        return;
    }
    #[cfg(not(unix))]
    let _ = (shared, channel_id, session_key, &mut transition);
    let managed = managed_session_clear_behavior(provider);
    if let (ManagedSessionClearBehavior::ResetManagedProcess, Some(name)) = (managed, tmux)
        && super::managed_reset::end_managed_process_session(name).tmux_survived
    {
        tracing::warn!(tmux_session = name, "clear could not end its tmux session");
        report.end_unconfirmed = true;
    }
    drop(transition);
}

/// Terminates the channel's live Herdr execution; false when none is known to run, so the
/// caller ends a managed session instead. An execution it cannot read also warns.
#[cfg(unix)]
async fn end_herdr(
    shared: &Arc<SharedData>,
    session_key: Option<&str>,
    transition: &mut Option<OwnedMutexGuard<()>>,
    report: &mut ClearReport,
) -> bool {
    use super::super::super::herdr_terminate::{
        OperatorTerminate, TerminationResult, terminate_explicit_herdr,
    };
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
    };
    let (Some(pool), Some(key)) = (shared.pg_pool.as_ref(), session_key) else {
        tracing::warn!("herdr clear has no pool or session key to end its execution with");
        report.end_unconfirmed = true;
        return false;
    };
    let lookup = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
    let nonce = match lookup {
        HostedLookup::Found(row) => match row.record {
            HostedRecord::Known(record) if record.state != HostedState::Retired => {
                Some(record.execution_nonce)
            }
            HostedRecord::Known(_) | HostedRecord::Legacy => return false,
            HostedRecord::Unknown(_) => None,
        },
        HostedLookup::Missing => return false,
        HostedLookup::Unknown(_) | HostedLookup::Conflict(_) => None,
    };
    let Some(execution_nonce) = nonce else {
        tracing::warn!(
            session_key = key,
            "herdr clear cannot read the execution to end"
        );
        report.end_unconfirmed = true;
        return false;
    };
    let request = OperatorTerminate {
        session_key: key.to_owned(),
        execution_nonce,
    };
    drop(transition.take());
    let result = terminate_explicit_herdr(request, shared.clone(), pool).await;
    if result != TerminationResult::Retired {
        let (outcome, detail) = result.outcome();
        tracing::warn!(session_key = key, outcome, %detail, "herdr clear could not end its execution");
        report.end_unconfirmed = true;
    }
    true
}
