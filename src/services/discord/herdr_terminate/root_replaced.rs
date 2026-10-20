//! Retires only a proved-ended replaced-root execution; never writes or closes a pane.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::services::cluster::channel_home::{CommandPermit, admit_command, command_worker_scope};
use crate::services::discord::recovery_engine::host_reconcile::HostReconcile;
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::discord::tmux::execution_identity::herdr_observation::{
    HerdrExecutionMatch, HerdrMismatch,
};
use crate::services::discord::tui_prompt_relay::herdr_source::HerdrSourceAttach;
use crate::services::herdr_launch::retired_hold::{self, RetiredHoldOutcome};
use crate::services::tui_prompt_dedupe as dedupe;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    Held(String),
    RetiredCleanupPending(String),
    Retired,
}
/// An exact RootReplaced refusal for one complete Bound observation; Missing cannot construct it.
#[derive(Clone, Debug)]
pub(crate) struct RootReplacedCandidate(HostedObservation);
impl RootReplacedCandidate {
    pub(in crate::services::discord) fn from_refusal(
        row: &HostedObservation,
        attach: &HerdrSourceAttach,
    ) -> Option<Self> {
        matches!((&row.record, attach),
            (HostedRecord::Known(record), HerdrSourceAttach::Refused(HostReconcile::Herdr(
                HerdrExecutionMatch::Mismatch(HerdrMismatch::RootReplaced)))) if record.state == HostedState::Bound
        ).then(|| Self(row.clone()))
    }
    pub(crate) fn matches(&self, row: &HostedObservation) -> bool {
        self.0 == *row
    }
}
#[derive(Clone)]
struct Cached {
    row: HostedObservation,
    result: Recovery,
    candidates: Vec<RootReplacedCandidate>,
}
#[cfg(not(test))]
static CACHE: std::sync::LazyLock<std::sync::Mutex<BTreeMap<i64, Cached>>> =
    std::sync::LazyLock::new(Default::default);
#[cfg(test)]
thread_local! {
    static CACHE: std::cell::RefCell<BTreeMap<i64, Cached>> = const { std::cell::RefCell::new(BTreeMap::new()) };
    pub(crate) static BEFORE_CAS: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(crate) static AFTER_FINAL_PROOF: ProbeSlot = const { std::cell::RefCell::new(None) };
    pub(crate) static CAS_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static CURRENT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static TRANSITIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
    cache(|entries| {
        entries.retain(|_, entry| {
            entry
                .candidates
                .retain(|candidate| rows.iter().any(|row| candidate.matches(row)));
            rows.contains(&entry.row) || !entry.candidates.is_empty()
        })
    });
}
pub(in crate::services::discord) fn remember_candidate(candidate: &RootReplacedCandidate) {
    cache(|entries| {
        let row = &candidate.0;
        let entry = entries.entry(row.session_id()).or_insert_with(|| Cached {
            row: row.clone(),
            result: Recovery::Held("candidate observed".into()),
            candidates: Vec::new(),
        });
        if !entry.candidates.iter().any(|known| known.matches(row)) {
            entry.candidates.push(candidate.clone());
        }
    });
}
pub(in crate::services::discord) fn candidate_for(
    row: &HostedObservation,
) -> Option<RootReplacedCandidate> {
    cache(|entries| {
        entries
            .get(&row.session_id())
            .and_then(|entry| {
                entry
                    .candidates
                    .iter()
                    .find(|candidate| candidate.matches(row))
            })
            .cloned()
    })
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
        let candidates = entries
            .get(&row.session_id())
            .map(|old| old.candidates.clone())
            .unwrap_or_default();
        entries.insert(
            row.session_id(),
            Cached {
                row: row.clone(),
                result: result.clone(),
                candidates,
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
    #[cfg(test)]
    CURRENT_READS.with(|count| count.set(count.get() + 1));
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
pub(crate) async fn recover(
    shared: &Arc<SharedData>,
    pool: &PgPool,
    candidate: &RootReplacedCandidate,
    transition: Option<&SessionTransitionLease<'_>>,
) -> Recovery {
    remember_candidate(candidate);
    recover_report(shared, pool, &candidate.0, transition).await
}
pub(crate) async fn recover_retired(
    shared: &Arc<SharedData>,
    pool: &PgPool,
    row: &HostedObservation,
    transition: Option<&SessionTransitionLease<'_>>,
) -> Recovery {
    let HostedRecord::Known(record) = &row.record else {
        return Recovery::Held("unknown retired execution".into());
    };
    if record.state != HostedState::Retired {
        return Recovery::Held("execution is not Retired".into());
    }
    // A completed exact row with no hold has no remaining mutation to authorize.
    if transition.is_none()
        && cache(|entries| {
            entries
                .get(&row.session_id())
                .is_some_and(|old| old.row == *row && old.result == Recovery::Retired)
        })
        && matches!(
            crate::services::claude::herdr_turn::input_held(&record.execution_nonce),
            Ok(false)
        )
    {
        return Recovery::Retired;
    }
    recover_report(shared, pool, row, transition).await
}
fn transition<'a>(
    shared: &'a SharedData,
    channel: ChannelId,
    borrowed: Option<&SessionTransitionLease<'_>>,
) -> Result<Option<SessionTransitionLease<'a>>, String> {
    #[cfg(test)]
    TRANSITIONS.with(|count| count.set(count.get() + 1));
    if let Some(lease) = borrowed {
        return lease
            .matches(shared, channel)
            .then_some(None)
            .ok_or("borrowed transition differs".into());
    }
    SessionTransitionLease::try_acquire(shared, channel)
        .map(Some)
        .map_err(|_| "transition busy".into())
}
async fn recover_report(
    shared: &Arc<SharedData>,
    pool: &PgPool,
    row: &HostedObservation,
    transition: Option<&SessionTransitionLease<'_>>,
) -> Recovery {
    let result = recover_inner(shared, pool, row, transition).await;
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
    borrowed: Option<&SessionTransitionLease<'_>>,
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
        let _transition = match transition(shared, channel, borrowed) {
            Ok(lease) => lease,
            Err(why) => return held(why),
        };
        if let Err(e) = current(pool, row).await {
            return held(e);
        }
        if let Err(e) = quiet(shared, &provider, channel, &record.owner.logical_key).await {
            return held(e);
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
    let _transition = match transition(shared, channel, borrowed) {
        Ok(lease) => lease,
        Err(why) => return held(why),
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
        #[cfg(test)]
        run_probe(&AFTER_FINAL_PROOF).await;
        current(pool, row).await?;
        quiet(shared, &provider, channel, &record.owner.logical_key).await
    }
    .await;
    if let Err(e) = checks {
        fence.reopen();
        return held(e);
    }
    // Once CAS is attempted, its result may be uncertain: the nonce never reopens.
    #[cfg(test)]
    CAS_ATTEMPTS.with(|count| count.set(count.get() + 1));
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
