//! Retires only a proved-ended replaced-root execution; never writes or closes a pane.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::services::cluster::channel_home::{CommandPermit, admit_command, command_worker_scope};
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::herdr_launch::retired_hold::{self, RetiredHoldOutcome};
use crate::services::tui_prompt_dedupe as dedupe;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    Held(String),
    RetiredCleanupPending(String),
    Retired,
}
#[derive(Clone)]
struct Cached {
    row: HostedObservation,
    result: Recovery,
}
#[cfg(not(test))]
static CACHE: std::sync::LazyLock<std::sync::Mutex<BTreeMap<i64, Cached>>> =
    std::sync::LazyLock::new(Default::default);
#[cfg(test)]
thread_local! {
    static CACHE: std::cell::RefCell<BTreeMap<i64, Cached>> = const { std::cell::RefCell::new(BTreeMap::new()) };
    pub(crate) static BEFORE_CAS: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(crate) static AFTER_CAS: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(crate) static CLEANUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn cache<R>(use_it: impl FnOnce(&mut BTreeMap<i64, Cached>) -> R) -> R {
    #[cfg(test)]
    {
        CACHE.with_borrow_mut(use_it)
    }
    #[cfg(not(test))]
    {
        use_it(
            &mut CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}
pub(crate) fn prune(rows: &[HostedObservation]) {
    cache(|entries| entries.retain(|_, entry| rows.contains(&entry.row)));
}
fn report(row: &HostedObservation, result: Recovery) -> Recovery {
    cache(|entries| {
        if !entries
            .get(&row.session_id())
            .is_some_and(|old| old.row == *row && old.result == result)
        {
            tracing::info!(
                row_id = row.session_id(),
                ?result,
                "herdr_root_replaced_recovery"
            );
        }
        entries.insert(
            row.session_id(),
            Cached {
                row: row.clone(),
                result: result.clone(),
            },
        );
    });
    result
}
fn local(
    shared: &SharedData,
    record: &HostedExecution,
) -> Result<(ProviderKind, ChannelId), String> {
    let owner = &record.owner;
    let provider = ProviderKind::from_str(&owner.provider).ok_or("provider unavailable")?;
    if provider != ProviderKind::Claude && provider != ProviderKind::Codex {
        return Err("provider has no supported Herdr recovery".into());
    }
    let channel = owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or("channel unavailable")?;
    let node = crate::config::session_hosts::local_node().ok_or("local node unavailable")?;
    let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
    let endpoint =
        crate::config::session_hosts::herdr_endpoint(channel).ok_or("endpoint unavailable")?;
    if owner.discord_token_hash != shared.token_hash
        || owner.owner_node != node
        || owner.runtime_root != root.display().to_string()
        || record.location.as_ref().is_none_or(|at| {
            at.host != "herdr"
                || at.execution_node != node
                || endpoint.execution_node != node
                || at.endpoint_config_key != endpoint.key
                || std::path::Path::new(&at.socket_addr) != endpoint.socket_path
                || at.named_session != endpoint.herdr_session
                || at.pane_id.is_empty()
        })
    {
        return Err("local owner or endpoint differs".into());
    }
    Ok((provider, ChannelId::new(channel)))
}
pub(crate) async fn current(pool: &PgPool, row: &HostedObservation) -> Result<(), String> {
    let HostedRecord::Known(record) = &row.record else {
        return Err("unknown record".into());
    };
    let owner = &record.owner;
    let canonical = HostedLookupKey::Canonical {
        provider: &owner.provider,
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &owner.discord_token_hash,
            channel_id: &owner.channel_id,
        },
    };
    if hosted_execution::load_hosted_execution_pg(pool, canonical).await
        != HostedLookup::Found(row.clone())
    {
        return Err("canonical execution changed or unreadable".into());
    }
    let (id, key) = crate::db::dispatched_session_canonical_identity::resolve_session_row_pg(
        pool,
        None,
        Some(&owner.provider),
        Some(CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &owner.discord_token_hash,
            channel_id: &owner.channel_id,
        }),
    )
    .await
    .map_err(|e| format!("exact session key: {e:?}"))?
    .ok_or("exact session key unavailable")?;
    if id != row.session_id() {
        return Err("exact session id differs".into());
    }
    if hosted_execution::load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await
        != HostedLookup::Found(row.clone())
    {
        return Err("exact execution changed or unreadable".into());
    }
    Ok(())
}
async fn quiet(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    logical: &str,
) -> Result<(), String> {
    if let Some(mailbox) = shared.mailbox_peek(channel) {
        let state = mailbox
            .try_snapshot()
            .await
            .map_err(|_| "mailbox unreadable")?;
        if state.cancel_token.is_some()
            || state.active_user_message_id.is_some()
            || state.active_turn_nonce.is_some()
            || state.active_request_owner.is_some()
            || !state.active_absorbed_source_ids.is_empty()
            || state.pending_user_dispatch.is_some()
            || !state.pending_user_dispatch_source_ids.is_empty()
        {
            return Err("actor or dispatch is active".into());
        }
    }
    if super::super::inflight::load_inflight_state_read_only_result(provider, channel.get())?
        .is_some()
    {
        return Err("inflight execution exists".into());
    }
    if dedupe::herdr_claim_admission(logical, true).is_ok() {
        return Err("Herdr execution is admitted".into());
    }
    Ok(())
}
/// The sequential restart pass identified this Bound nonce as RootReplaced, or selected Retired cleanup.
pub(crate) async fn recover(
    shared: &Arc<SharedData>,
    pool: &PgPool,
    row: &HostedObservation,
) -> Recovery {
    let result = recover_inner(shared, pool, row).await;
    let reported_retired = cache(|entries| {
        entries
            .get(&row.session_id())
            .is_some_and(|entry| match (&row.record, &entry.row.record) {
                (HostedRecord::Known(before), HostedRecord::Known(after)) => {
                    let mut retired = before.clone();
                    retired.state = HostedState::Retired;
                    before.state == HostedState::Bound && *after == retired
                }
                _ => false,
            })
    });
    if reported_retired || result == Recovery::Retired {
        result
    } else {
        report(row, result)
    }
}
async fn recover_inner(
    shared: &Arc<SharedData>,
    pool: &PgPool,
    row: &HostedObservation,
) -> Recovery {
    let held = Recovery::Held;
    if !shared
        .restart
        .reconcile_done
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return held("restart reconciliation incomplete".into());
    }
    let HostedRecord::Known(record) = &row.record else {
        return held("unknown execution".into());
    };
    let (provider, channel) = match local(shared, record) {
        Ok(v) => v,
        Err(e) => return held(e),
    };
    let permit = match admit_command(&channel.to_string(), provider.as_str()) {
        Ok(permit) => permit,
        Err(e) => return held(format!("home: {e:?}")),
    };
    if let Err(e) = current(pool, row).await {
        return held(e);
    }
    if record.state == HostedState::Retired {
        let Ok(_transition) = SessionTransitionLease::try_acquire(shared, channel) else {
            return held("transition busy".into());
        };
        if let Err(e) = current(pool, row).await {
            return held(e);
        }
        if let Err(e) = quiet(shared, &provider, channel, &record.owner.logical_key).await {
            return held(e);
        }
        let complete = cache(|entries| {
            entries
                .get(&row.session_id())
                .is_some_and(|old| old.row == *row && old.result == Recovery::Retired)
        });
        match crate::services::claude::herdr_turn::input_held(&record.execution_nonce) {
            Ok(false) if complete => return Recovery::Retired,
            Err(e) => return Recovery::RetiredCleanupPending(e),
            _ => {}
        }
        return cleanup(shared, &provider, row, record, permit).await;
    }
    if record.state != HostedState::Bound {
        return held("execution is not Bound".into());
    }
    let Some(target) = herdr_endpoints().target(record) else {
        return held("registered target unavailable".into());
    };
    let proof_target = target.clone();
    let proof_record = record.clone();
    let proof_permit = permit.clone();
    let first = match tokio::task::spawn_blocking(move || {
        let _scope = command_worker_scope(proof_permit);
        proof_target.replaced_root_end(&proof_record, None)
    })
    .await
    {
        Ok(Ok(proof)) => proof,
        other => return held(format!("initial strict proof: {other:?}")),
    };
    let Ok(_transition) = SessionTransitionLease::try_acquire(shared, channel) else {
        return held("transition busy".into());
    };
    let fence = match target.termination_fence() {
        Ok(fence) => fence,
        Err(e) => return held(format!("fence: {e:?}")),
    };
    #[cfg(test)]
    run_probe(&BEFORE_CAS).await;
    let checks = async {
        local(shared, record)?;
        current(pool, row).await?;
        quiet(shared, &provider, channel, &record.owner.logical_key).await?;
        let proof_target = target.clone();
        let proof_record = record.clone();
        let proof_permit = permit.clone();
        tokio::task::spawn_blocking(move || {
            let _scope = command_worker_scope(proof_permit);
            proof_target.replaced_root_end(&proof_record, Some(&first))
        })
        .await
        .map_err(|e| e.to_string())??;
        current(pool, row).await?;
        quiet(shared, &provider, channel, &record.owner.logical_key).await
    }
    .await;
    if let Err(e) = checks {
        fence.reopen();
        return held(e);
    }
    // Once CAS is attempted, its result may be uncertain: the nonce never reopens.
    if let Err(e) = retire_confirmed(pool, row, record).await {
        return held(format!("{e:?}"));
    }
    #[cfg(test)]
    run_probe(&AFTER_CAS).await;
    let identity = HostedLookupKey::Canonical {
        provider: &record.owner.provider,
        identity: CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: &record.owner.discord_token_hash,
            channel_id: &record.owner.channel_id,
        },
    };
    let retired = match hosted_execution::load_hosted_execution_pg(pool, identity).await {
        HostedLookup::Found(retired) => retired,
        other => return Recovery::RetiredCleanupPending(format!("retired read: {other:?}")),
    };
    let mut expected = record.clone();
    expected.state = HostedState::Retired;
    if retired.session_id() != row.session_id()
        || retired.record != HostedRecord::Known(expected.clone())
    {
        return Recovery::RetiredCleanupPending("retired identity differs".into());
    }
    if let Err(e) = current(pool, &retired).await {
        return Recovery::RetiredCleanupPending(e);
    }
    if let Err(e) = quiet(shared, &provider, channel, &record.owner.logical_key).await {
        return Recovery::RetiredCleanupPending(e);
    }
    cleanup(shared, &provider, &retired, &expected, permit).await
}
async fn cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    row: &HostedObservation,
    record: &HostedExecution,
    permit: Option<CommandPermit>,
) -> Recovery {
    #[cfg(test)]
    CLEANUPS.with(|n| n.set(n.get() + 1));
    let nonce = record.execution_nonce.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let _scope = command_worker_scope(permit);
        retired_hold::release(&nonce)
    })
    .await;
    let result = match outcome {
        Ok(RetiredHoldOutcome::Durable) => match settle_captured(shared, provider, None).await {
            TerminationResult::Retired => Recovery::Retired,
            other => Recovery::RetiredCleanupPending(format!("empty turn settlement: {other:?}")),
        },
        other => Recovery::RetiredCleanupPending(format!("hold release: {other:?}")),
    };
    report(row, result)
}
