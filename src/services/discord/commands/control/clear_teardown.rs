//! A committed clear's writes and teardown: each failure is reported, none undoes the clear.

pub(in crate::services::discord) mod execution_cleanup;

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use tokio::sync::OwnedMutexGuard;

use super::super::super::SharedData;
use super::super::super::turn_bridge::stop_active_turn_with_outcome;
use super::{ClearReport, ManagedSessionClearBehavior, managed_session_clear_behavior};
use crate::db::session_transcripts;
use crate::services::provider::{CancelToken, ProviderKind};

#[cfg(unix)]
mod clear_end_record;
#[cfg(all(test, unix))]
mod clear_end_record_tests;
#[cfg(unix)]
mod codex_end_record;
#[cfg(all(test, unix))]
mod codex_end_tests;
#[cfg(unix)]
pub(super) mod resume_fence;

pub(super) async fn tmux_name(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
) -> Option<String> {
    shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|session| session.channel_name.as_deref())
        .map(|name| provider.build_tmux_session_name(name))
}

pub(super) async fn write_channel_boundary(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    (session_key, logical): (Option<&str>, Option<&str>),
    report: &mut ClearReport,
) {
    use super::super::super::previous_provider_session::{self, Selection};
    let selected = Selection {
        session_key,
        logical,
        ..Selection::default()
    };
    let previous =
        match previous_provider_session::for_clear_key(shared, provider, channel, selected).await {
            Ok(previous) => previous,
            Err(error) => {
                report.note_previous_provider_session_error(&error);
                None
            }
        };
    write_boundary(shared, channel, previous, report).await;
}

/// Advances the channel's transcript clear boundary, retried once.
pub(super) async fn write_boundary(
    shared: &SharedData,
    channel_id: serenity::ChannelId,
    previous_session: Option<session_transcripts::PreviousProviderSession>,
    report: &mut ClearReport,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        report.fail("pg_boundary", "postgres pool unavailable");
        return;
    };
    let key = channel_id.get().to_string();
    let context = session_transcripts::RecentContextBoundary {
        kind: session_transcripts::RecentContextBoundaryKind::Cleared,
        previous_session,
    };
    let write = || session_transcripts::record_recent_context_boundary(Some(pool), &key, &context);
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
/// Unless the host is tmux, the stored Herdr row is ended first; a held host ends nothing else.
pub(super) async fn end_session(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    (tmux, session_key): (Option<&str>, Option<&str>),
    mut transition: Option<OwnedMutexGuard<()>>,
    clear_epoch: &crate::services::herdr_launch::clear_epoch::ClearGuard,
    report: &mut ClearReport,
) {
    let host = crate::services::discord::admin_host_guard::host_decision_of;
    let decision = host(shared, provider, channel_id.get()).await;
    let herdr = decision.is_herdr();
    #[cfg(unix)]
    if decision.tmux_refusal().is_some() {
        drop(transition.take());
        clear_epoch.wait_idle().await;
        match end_herdr(shared, session_key, &mut transition, report).await {
            // A channel's own Herdr endpoint with a legacy row ends its managed session as before.
            HerdrEnd::NoExecution if herdr => {}
            // Held: a legacy row or none does not confirm tmux either, so nothing is ended.
            HerdrEnd::NoExecution => {
                report.end_unconfirmed = true;
                return;
            }
            HerdrEnd::Ended | HerdrEnd::Unconfirmed => return,
        }
    }
    #[cfg(not(unix))]
    let _ = (
        shared,
        channel_id,
        session_key,
        &mut transition,
        clear_epoch,
    );
    let managed = managed_session_clear_behavior(provider);
    if let (ManagedSessionClearBehavior::ResetManagedProcess, Some(name)) = (managed, tmux) {
        use crate::services::tui_prompt_dedupe::clear_relaunch;
        let captured = if matches!(provider, ProviderKind::Codex) && !herdr {
            match clear_relaunch::capture(name) {
                Ok(captured) => captured,
                Err(error) => {
                    report.fail("codex_retirement_capture", &error);
                    None
                }
            }
        } else {
            None
        };
        #[cfg(unix)]
        let clear_end =
            clear_end_record::capture_for(provider, (channel_id.get(), herdr), name, &transition);
        #[cfg(unix)]
        let codex_end =
            codex_end_record::capture_for(provider, (channel_id.get(), herdr), name, &transition);
        let ended = super::managed_reset::end_managed_process_session(name);
        if ended.tmux_unconfirmed {
            tracing::warn!(tmux_session = name, "clear could not confirm its tmux end");
            report.end_unconfirmed = true;
        } else if let Some(captured) = captured
            && let Err(error) = clear_relaunch::commit(&captured, "explicit_clear")
        {
            report.fail("codex_retirement_commit", &error);
        }
        #[cfg(unix)]
        clear_end_record::commit_confirmed(clear_end, (ended.reset, ended.tmux_unconfirmed)).await;
        #[cfg(unix)]
        codex_end_record::commit_confirmed(codex_end, (ended.reset, ended.tmux_unconfirmed)).await;
    } else if tmux.is_none()
        && matches!(provider, ProviderKind::Codex)
        && channel_id.get() == crate::services::codex_tui::canary::CANARY_CHANNEL
        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
        && !shared.core.lock().await.sessions.contains_key(&channel_id)
    {
        use crate::services::tui_prompt_dedupe::clear_relaunch;
        let name = crate::services::codex_tui::canary::CANARY_TMUX;
        match clear_relaunch::capture(name) {
            Ok(Some(captured)) => {
                // No core target grants an end effect; commit rechecks the dead incarnation's vacancy.
                if let Err(error) = clear_relaunch::commit(&captured, "explicit_clear") {
                    report.fail("codex_retirement_commit", &error);
                }
            }
            Ok(None) => {}
            Err(error) => report.fail("codex_retirement_capture", &error),
        }
    }
    drop(transition);
}

/// What a clear's end found stored for a Herdr execution.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum HerdrEnd {
    /// The execution was ended or its retired selector handled (its result is reported).
    Ended,
    /// A legacy row or none: no Herdr execution is stored (it grants no tmux end by itself).
    NoExecution,
    /// The row could not be read; nothing is ended and the clear warns.
    Unconfirmed,
}

/// Terminates the channel's stored Herdr execution; an execution it cannot read warns and ends
/// nothing.
#[cfg(unix)]
async fn end_herdr(
    shared: &Arc<SharedData>,
    session_key: Option<&str>,
    transition: &mut Option<OwnedMutexGuard<()>>,
    report: &mut ClearReport,
) -> HerdrEnd {
    use super::super::super::herdr_terminate::{
        OperatorTerminate, TerminationResult, terminate_explicit_herdr,
    };
    use crate::db::dispatched_sessions::hosted_execution::http_force::clear_retired_selector_pg;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
    };
    let (Some(pool), Some(key)) = (shared.pg_pool.as_ref(), session_key) else {
        tracing::warn!("herdr clear has no pool or session key to end its execution with");
        report.end_unconfirmed = true;
        return HerdrEnd::Unconfirmed;
    };
    let lookup = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await;
    let nonce = match lookup {
        HostedLookup::Found(row) => match (row.session_id(), row.record) {
            (id, HostedRecord::Known(record)) if record.state != HostedState::Retired => {
                Some((id, record.execution_nonce))
            }
            (id, HostedRecord::Known(record)) => {
                drop(transition.take());
                let cleared = clear_retired_selector_pg(pool, id, &record).await;
                note_retired_selector(cleared, report);
                report.execution = Some(super::clear_report::ExecutionCleanup::Retired);
                return HerdrEnd::Ended;
            }
            (_, HostedRecord::Legacy) => return HerdrEnd::NoExecution,
            (_, HostedRecord::Unknown(_)) => None,
        },
        HostedLookup::Missing => return HerdrEnd::NoExecution,
        HostedLookup::Unknown(_) | HostedLookup::Conflict(_) => None,
    };
    let Some((session_id, execution_nonce)) = nonce else {
        tracing::warn!(
            session_key = key,
            "herdr clear cannot read the execution to end"
        );
        report.end_unconfirmed = true;
        return HerdrEnd::Unconfirmed;
    };
    // The keyed selector clear skips hosted rows: the retired execution's own is forgotten here,
    // inside the terminate's exclusion, so no successor can start in between.
    let (answer, forgotten) = tokio::sync::oneshot::channel();
    let selector_pool = pool.clone();
    let request = OperatorTerminate {
        session_key: key.to_owned(),
        execution_nonce,
    }
    .after_retire(move |exclusion| async move {
        if let Some(record) = exclusion.execution() {
            let _ =
                answer.send(clear_retired_selector_pg(&selector_pool, session_id, record).await);
        }
        drop(exclusion);
        TerminationResult::Retired
    });
    drop(transition.take());
    let result = terminate_explicit_herdr(request, shared.clone(), pool).await;
    if let Ok(cleared) = forgotten.await {
        note_retired_selector(cleared, report);
    }
    if result == TerminationResult::Retired {
        // A retired execution's reply then names what its cleanup left behind, not a fresh session.
        report.execution = Some(super::clear_report::ExecutionCleanup::Retired);
    } else {
        let (outcome, detail) = result.outcome();
        tracing::warn!(session_key = key, outcome, %detail, "herdr clear could not end its execution");
        report.end_unconfirmed = true;
    }
    HerdrEnd::Ended
}

/// Records a retired execution's selector the exact clear could not forget.
#[cfg(unix)]
pub(super) fn note_retired_selector(
    cleared: Result<bool, String>,
    report: &mut ClearReport,
) -> bool {
    match cleared {
        Ok(true) => {
            report.conversation_reset = true;
            return true;
        }
        Ok(false) => report.fail("session_selector", "retired execution row moved on"),
        Err(error) => report.fail("session_selector", &error),
    }
    false
}

/// Uses the canonical stop ordering; a host refusal leaves the released turn unconfirmed.
pub(super) async fn stop_released_turn(
    provider: &ProviderKind,
    token: Option<Arc<CancelToken>>,
    clear_source: &str,
) -> bool {
    let Some(token) = token else {
        return true;
    };
    let policy = super::super::super::turn_bridge::TmuxCleanupPolicy::PreserveSession;
    let stop = stop_active_turn_with_outcome(provider, &token, policy, clear_source).await;
    // Only a legacy settlement says the stop reached the turn's host.
    stop.may_clear_inflight()
}
