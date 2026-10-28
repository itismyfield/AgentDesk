//! End the stored incarnation under its local transition, without clear's queue mutations.
use crate::services::cluster::intake_router_hook::owner_record::cleanup::{self, Record, Response};
use crate::services::discord::{self as discord, SharedData};
use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
use poise::serenity_prelude::ChannelId;
use std::{future::Future, sync::Arc};
use tokio::sync::OwnedMutexGuard;

pub(in crate::services::discord) async fn run(shared: &Arc<SharedData>, row: &Record) -> Response {
    let end_shared = shared.clone();
    run_with(shared, row, move |owner, transition| async move {
        normal_end(&end_shared, row, &owner, transition).await
    })
    .await
}

async fn run_with<F, Fut>(shared: &Arc<SharedData>, row: &Record, end: F) -> Response
where
    F: FnOnce(String, OwnedMutexGuard<()>) -> Fut,
    Fut: Future<Output = Response>,
{
    let Ok(channel) = row.raw_channel_id.parse::<u64>() else {
        return Response::NeedsOperator;
    };
    let channel_id = ChannelId::new(channel);
    let Some(original) = row.routing_cleanup.native.as_ref() else {
        return Response::NeedsOperator;
    };
    let proven = if original["host"] == "herdr" {
        use crate::db::dispatched_sessions::hosted_execution::{
            HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
        };
        let Some(pool) = &shared.pg_pool else {
            return Response::Retry;
        };
        let (Some(owner), Some(nonce)) = (
            original["owner"].as_str(),
            original["execution_nonce"].as_str(),
        ) else {
            return Response::NeedsOperator;
        };
        match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(owner)).await {
            HostedLookup::Found(row) if matches!(row.record,HostedRecord::Known(ref known) if known.execution_nonce==nonce) => {
                true
            }
            HostedLookup::Found(_) => return Response::IdentityChanged,
            HostedLookup::Unknown(_) => return Response::Retry,
            _ => return Response::NeedsOperator,
        }
    } else {
        match validate_native(row, original) {
            Ok(()) => true,
            Err(Response::IdentityChanged)
                if original["owner"].as_str().is_some_and(|owner| {
                    binding_context::observe_spawn_nonce_marker(owner) == SpawnNonceMarker::Absent
                }) =>
            {
                false
            }
            Err(response) if !identity_mutant() => return response,
            _ => true,
        }
    };
    let Ok(transition) = shared.acquire_session_transition(channel_id).await else {
        return if proven {
            Response::Busy
        } else {
            Response::Retry
        };
    };
    let mailbox = discord::mailbox_snapshot(shared, channel_id).await;
    if mailbox.cancel_token.is_some()
        || mailbox.pending_user_dispatch.is_some()
        || shared.relay_emission_in_flight(channel_id)
        || discord::terminal_delivery_custody::retains_channel(channel) != Ok(false)
    {
        return if proven {
            Response::Busy
        } else {
            Response::Retry
        };
    }
    if let Err(response) = inflight_ready(shared, row).await {
        return response;
    }
    let Some(native) = row.routing_cleanup.native.as_ref() else {
        return Response::NeedsOperator;
    };
    let (Some(owner), Some(nonce)) = (native["owner"].as_str(), native["execution_nonce"].as_str())
    else {
        return Response::NeedsOperator;
    };
    if native["host"] == "herdr" {
        #[cfg(unix)]
        {
            use discord::herdr_terminate::{
                OperatorTerminate, TerminationResult, terminate_explicit_herdr,
            };
            let Some(pool) = &shared.pg_pool else {
                return Response::Retry;
            };
            if !cleanup::begin_end(pool, row).await.unwrap_or(false) {
                return Response::Retry;
            }
            drop(transition);
            let (answer, result) = tokio::sync::oneshot::channel();
            let finish_shared = shared.clone();
            let finish_row = row.clone();
            let request = OperatorTerminate {
                session_key: owner.into(),
                execution_nonce: nonce.into(),
            }
            .after_retire(move |exclusion| async move {
                let response = finish_herdr(&finish_shared, &finish_row, &exclusion).await;
                let _ = answer.send(response);
                if response == Response::Ended {
                    TerminationResult::Retired
                } else {
                    TerminationResult::Indeterminate(format!(
                        "owner cleanup retained: {response:?}"
                    ))
                }
            });
            let terminated = terminate_explicit_herdr(request, shared.clone(), pool).await;
            #[cfg(test)]
            eprintln!("owner cleanup Herdr termination: {terminated:?}");
            if let Ok(response) = result.await {
                return response;
            }
            return match terminated {
                TerminationResult::Retired => Response::Retry,
                TerminationResult::Disabled | TerminationResult::Refused(_) => {
                    Response::NeedsOperator
                }
                TerminationResult::Close(crate::services::termination_audit::host_terminate::herdr_terminate::HerdrTerminateResult::ConfirmationRequired | crate::services::termination_audit::host_terminate::herdr_terminate::HerdrTerminateResult::Refused(_)) => Response::NeedsOperator,
                _ => Response::Retry,
            };
        }
        #[cfg(not(unix))]
        {
            return Response::NeedsOperator;
        }
    }
    if !matches!(native["host"].as_str(), Some("tmux" | "process")) {
        return Response::NeedsOperator;
    }
    if native["root"] != crate::services::tmux_common::current_tmux_owner_marker()
        || native["endpoint"] != row.owner_instance_id
    {
        return Response::IdentityChanged;
    }
    match binding_context::observe_spawn_nonce_marker(owner) {
        SpawnNonceMarker::Known(current) if current != nonce && !identity_mutant() => {
            return Response::IdentityChanged;
        }
        SpawnNonceMarker::Unreadable => return Response::Retry,
        SpawnNonceMarker::Absent => {
            #[cfg(unix)]
            {
                use crate::services::platform::tmux::{self, SessionPresence};
                return if tmux::session_presence(owner) == SessionPresence::Missing
                    && crate::services::session_backend::process_session_pid(owner).is_none()
                {
                    archive_completed(shared, row).await
                } else {
                    Response::IdentityChanged
                };
            }
            #[cfg(not(unix))]
            {
                return Response::NeedsOperator;
            }
        }
        _ => {}
    }
    if let Err(response) = validate_native(row, native)
        && !identity_mutant()
    {
        return response;
    }
    if super::super::managed_reset::verified_codex_reset_refusal_for_target(
        shared,
        &shared.provider,
        channel_id,
        Some(owner),
    )
    .await
    .is_some()
    {
        return Response::NeedsOperator;
    }
    let Some(pool) = &shared.pg_pool else {
        return Response::Retry;
    };
    if !cleanup::begin_end(pool, row).await.unwrap_or(false) {
        return Response::Retry;
    }
    #[cfg(test)]
    let transition = if cleanup::tests::mutant("pin-lifetime") {
        drop(transition);
        Arc::new(tokio::sync::Mutex::new(())).lock_owned().await
    } else {
        transition
    };
    end(owner.into(), transition).await
}

#[cfg(unix)]
async fn finish_herdr(
    shared: &Arc<SharedData>,
    row: &Record,
    exclusion: &discord::herdr_terminate::RetiredExclusion,
) -> Response {
    use crate::db::dispatched_sessions::hosted_execution::{
        self, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
    };
    let Some(binding) = &row.routing_cleanup.native else {
        return Response::NeedsOperator;
    };
    let (Some(key), Some(nonce), Some(original)) = (
        binding["owner"].as_str(),
        binding["execution_nonce"].as_str(),
        exclusion.execution(),
    ) else {
        return Response::NeedsOperator;
    };
    if exclusion.provider().as_str() != row.provider
        || shared.provider.as_str() != row.provider
        || exclusion.channel().get().to_string() != row.raw_channel_id
        || original.owner.owner_node != row.owner_instance_id
        || original.owner.discord_token_hash != shared.token_hash
        || original.execution_nonce != nonce
        || binding["endpoint"] != row.owner_instance_id
        || binding["root"] != crate::services::tmux_common::current_tmux_owner_marker()
        || crate::config::runtime_root()
            .is_none_or(|root| root.to_string_lossy() != original.owner.runtime_root)
    {
        return Response::IdentityChanged;
    }
    let Some(pool) = &shared.pg_pool else {
        return Response::Retry;
    };
    let mut expected = original.clone();
    expected.state = HostedState::Retired;
    match hosted_execution::load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(current) if matches!(current.record, HostedRecord::Known(ref record) if record == &expected) =>
            {}
        HostedLookup::Unknown(_) => return Response::Retry,
        _ => return Response::IdentityChanged,
    }
    #[cfg(test)]
    if cleanup::tests::mutant("herdr-postlude") {
        return Response::Ended;
    }
    let response = archive_completed(shared, row).await;
    if response != Response::Ended {
        return response;
    }
    if let Some(session) = shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&exclusion.channel())
    {
        session.clear_provider_session();
    }
    Response::Ended
}

async fn normal_end(
    shared: &Arc<SharedData>,
    row: &Record,
    owner: &str,
    transition: OwnedMutexGuard<()>,
) -> Response {
    let transition = Some(transition);
    #[cfg(unix)]
    let claude = super::clear_end_record::capture_for(
        &shared.provider,
        row.raw_channel_id.parse().unwrap(),
        owner,
        &transition,
    );
    #[cfg(unix)]
    let codex = super::codex_end_record::capture_for(
        &shared.provider,
        row.raw_channel_id.parse().unwrap(),
        owner,
        &transition,
    );
    let ended = crate::services::tmux_common::try_with_tmux_source_authority(owner, |authority| {
        let validation = validate_native(
            row,
            row.routing_cleanup
                .native
                .as_ref()
                .ok_or(Response::NeedsOperator)?,
        );
        #[cfg(test)]
        let validation = if cleanup::tests::mutant("normal-source") {
            Ok(())
        } else {
            validation
        };
        validation?;
        let ended = super::super::managed_reset::end_under_source(authority);
        // A confirmed vacant slot still carries the exact old launch files after a host restart.
        if !ended.tmux_unconfirmed
            && !ended.reset
            && crate::services::session_backend::process_session_pid(owner).is_none()
        {
            crate::services::tmux_common::cleanup_session_temp_files_under_source_authority(
                authority, owner,
            );
        }
        Ok(ended)
    });
    let ended = match ended {
        Some(Ok(ended)) => ended,
        Some(Err(response)) => return response,
        None => return Response::Retry,
    };
    if ended.tmux_unconfirmed {
        return Response::Retry;
    }
    #[cfg(unix)]
    super::clear_end_record::commit_confirmed(claude, (ended.reset, ended.tmux_unconfirmed)).await;
    #[cfg(unix)]
    super::codex_end_record::commit_confirmed(codex, (ended.reset, ended.tmux_unconfirmed)).await;
    let archived = archive_completed(shared, row).await;
    if archived != Response::Ended {
        return archived;
    }
    let channel = ChannelId::new(row.raw_channel_id.parse().unwrap());
    if let Some(session) = shared.core.lock().await.sessions.get_mut(&channel) {
        session.clear_provider_session();
    }
    drop(transition);
    Response::Ended
}

// A completed sidecar can survive a crash; its original receipt and native binding remain authority.
async fn inflight_ready(
    shared: &Arc<SharedData>,
    row: &Record,
) -> Result<Option<discord::inflight::InflightTurnState>, Response> {
    let channel = row
        .raw_channel_id
        .parse()
        .map_err(|_| Response::NeedsOperator)?;
    let state = discord::inflight::load_inflight_state_read_only_result(&shared.provider, channel)
        .map_err(|_| Response::Retry)?;
    let Some(state) = state else { return Ok(None) };
    #[cfg(test)]
    if cleanup::tests::mutant("inflight-busy") {
        return Err(Response::Busy);
    }
    let Some(native) = row.routing_cleanup.native.as_ref() else {
        return Err(Response::NeedsOperator);
    };
    if state.provider != row.provider
        || state.channel_id != channel
        || state.tmux_session_name.as_deref() != native["owner"].as_str()
    {
        return Err(Response::IdentityChanged);
    }
    let pool = shared.pg_pool.as_ref().ok_or(Response::Retry)?;
    let authority: Option<(String, bool)> = sqlx::query_as("SELECT r.replay_disposition, o.status<>'active' FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id JOIN intake_session_owners o ON o.provider=d.provider AND o.raw_channel_id=d.channel_id AND o.owner_instance_id=d.owner_instance_id AND o.generation=d.owner_generation WHERE r.id=$1 AND d.provider=$2 AND d.channel_id=$3 AND d.owner_instance_id=$4 AND d.owner_generation=$5 AND r.user_msg_id=$6 AND r.replay_preserved #> '{projection,binding}'=$7")
        .bind(state.replay_receipt_id).bind(&row.provider).bind(&row.raw_channel_id)
        .bind(&row.owner_instance_id).bind(row.generation).bind(state.user_msg_id.to_string()).bind(native)
        .fetch_optional(pool).await.map_err(|_| Response::Retry)?;
    let Some((disposition, fenced)) = authority else {
        return Err(Response::NeedsOperator);
    };
    if state.terminal_delivery_completed() {
        return Ok(Some(state));
    }
    #[cfg(unix)]
    let gone = crate::services::platform::tmux::session_presence(native["owner"].as_str().unwrap())
        == crate::services::platform::tmux::SessionPresence::Missing
        && crate::services::session_backend::process_session_pid(native["owner"].as_str().unwrap())
            .is_none();
    #[cfg(not(unix))]
    let gone = false;
    if fenced && disposition == "withheld" && (gone || native_completed(shared, &state)?) {
        return Ok(Some(state));
    }
    Err(Response::Busy)
}

fn native_completed(
    shared: &Arc<SharedData>,
    state: &discord::inflight::InflightTurnState,
) -> Result<bool, Response> {
    read_native_completion(shared, state, 64)
}

fn read_native_completion(
    shared: &Arc<SharedData>,
    state: &discord::inflight::InflightTurnState,
    max_chunks: usize,
) -> Result<bool, Response> {
    use crate::services::tui_o::{
        shadow::{SourceBinding, SourceId},
        writer::input_facts::{InputFacts, TurnState},
    };
    let (Some(path), Some(session), Some(offset), Some((dev, ino))) = (
        &state.output_path,
        &state.session_id,
        state.turn_start_offset,
        state.tui_terminal_source_file_identity,
    ) else {
        return Ok(false);
    };
    let source = SourceId {
        session_id: session.clone(),
        path: path.into(),
        dev,
        ino,
    };
    let provider = match shared.provider {
        crate::services::provider::ProviderKind::Claude => {
            crate::services::tui_o::shadow::ShadowProvider::Claude
        }
        crate::services::provider::ProviderKind::Codex => {
            crate::services::tui_o::shadow::ShadowProvider::Codex
        }
        _ => return Ok(false),
    };
    let Ok(mut facts) = InputFacts::open_at(
        SourceBinding {
            channel_id: state.channel_id,
            provider,
            source,
        },
        offset,
    ) else {
        return Err(Response::Retry);
    };
    for _ in 0..max_chunks {
        let fact = facts.poll(1024 * 1024).map_err(|_| Response::Retry)?;
        if facts.caught_up().map_err(|_| Response::Retry)? {
            return Ok(fact.state == TurnState::Idle
                && shared.turn_finalizer.completion_signal_state(
                    &shared.provider,
                    Some(state.runtime_kind_for_recovery()),
                    std::path::Path::new(path),
                ) == discord::turn_finalizer::CompletionSignal::Done);
        }
        #[cfg(test)]
        if cleanup::tests::mutant("single-poll") {
            return Ok(false);
        }
    }
    Err(Response::Retry)
}

async fn archive_completed(shared: &Arc<SharedData>, row: &Record) -> Response {
    use discord::inflight::{self, GuardedClearOutcome, InflightTurnIdentity};
    let state = match inflight_ready(shared, row).await {
        Ok(None) => return Response::Ended,
        Ok(Some(state)) => state,
        Err(response) => return response,
    };
    let channel = row.raw_channel_id.parse().unwrap();
    match inflight::archive_inflight_state_if_matches_identity_generation(
        &shared.provider,
        channel,
        &InflightTurnIdentity::from_state(&state),
        &state.updated_at,
        state.save_generation,
        "owner-normal-end",
    ) {
        GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing => Response::Ended,
        _ => Response::Retry,
    }
}

#[cfg(test)]
pub(crate) mod tests;

fn identity_mutant() -> bool {
    #[cfg(test)]
    {
        cleanup::tests::mutant("identity")
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn validate_native(row: &Record, native: &serde_json::Value) -> Result<(), Response> {
    use crate::services::tui_o::ownership::execution::{NativeRefFailure, validate_local};
    let channel = row
        .raw_channel_id
        .parse()
        .map_err(|_| Response::NeedsOperator)?;
    validate_local(&row.provider, channel, &row.owner_instance_id, native).map_err(|error| {
        match error {
            NativeRefFailure::Malformed => Response::NeedsOperator,
            NativeRefFailure::Unavailable => Response::Retry,
            NativeRefFailure::Changed => Response::IdentityChanged,
        }
    })
}
