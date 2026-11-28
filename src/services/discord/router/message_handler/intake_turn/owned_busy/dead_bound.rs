//! The dead-Bound recovery a person's entry and a synthetic start's pre-claim read share: under the
//! transition lease it proves the execution ended and no turn holds the channel, then retires it.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedRecord, HostedState,
};
use crate::services::claude::herdr_turn::HoldRelease;
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::herdr_launch::retire::{self, RetireRefusal};
use crate::services::session_host::PaneReading;

/// Why the recovery left the row as it was.
pub(in crate::services::discord) enum DeadBound {
    /// A later read may differ: no proof yet, a turn holds the channel, or the lease is taken.
    Reobserve(String),
    /// A dependency failed for now.
    Retry(String),
    /// This owner can never recover the row.
    Refused(String),
}

/// The transition exclusion held from the proof to the CAS.
pub(in crate::services::discord) enum LeaseRef<'a> {
    /// Taken here and never waited for; a busy lock is a later look's business.
    Acquire,
    /// The intake guard a person's entry already holds; it cannot block itself.
    Borrow(&'a tokio::sync::OwnedMutexGuard<()>),
}

/// Retires the channel's Bound execution of `provider` once proven ended; `Ok` means the row now
/// reads Retired and a fresh Herdr execution is admissible. `retired` runs right after the CAS.
pub(in crate::services::discord) async fn recover<F: FnOnce()>(
    shared: &Shared,
    (provider, channel, key, logical): (&ProviderKind, ChannelId, Option<&str>, &str),
    host: &TurnHost,
    mailbox: &ChannelMailboxHandle,
    lease: LeaseRef<'_>,
    retired: F,
) -> Result<(), DeadBound> {
    use DeadBound::{Refused, Retry};
    let refused = |reason: &str| Refused(reason.into());
    let TurnHost::Herdr(plan) = host else {
        return Err(refused("host is not a local Herdr execution"));
    };
    let row = plan
        .row
        .as_ref()
        .ok_or_else(|| refused("hosted row unavailable"))?;
    let HostedRecord::Known(record) = &row.record else {
        return Err(refused("hosted record unreadable"));
    };
    let root =
        crate::config::runtime_root().ok_or_else(|| Retry("runtime root unavailable".into()))?;
    let node = crate::config::session_hosts::local_node()
        .ok_or_else(|| Retry("local node unavailable".into()))?;
    let at = record
        .location
        .as_ref()
        .ok_or_else(|| refused("recorded pane unavailable"))?;
    let ids = (provider, shared.token_hash.as_str(), channel, logical);
    if owner_differs((record, at), plan, ids, (root.as_path(), node.as_str())) {
        return Err(refused("local execution owner or endpoint differs"));
    }
    let transition = exclusion(shared, channel, lease)?;
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or_else(|| Retry("database unavailable".into()))?;
    crate::services::discord::herdr_terminate::root_replaced::current(pool, row)
        .await
        .map_err(Retry)?;
    // The proof comes first: a turn or row a live provider started is already visible below.
    let proof = retire::prove(row).await.map_err(classify)?;
    no_turn(provider, channel, mailbox).await?;
    #[cfg(test)]
    tests::before_cas(channel.get());
    let done = retire::on_reading(pool, row, record, &proof.reading, proof.ended)
        .await
        .map_err(classify)?;
    if matches!(proof.reading, PaneReading::Missing) {
        retire::note_missing(record);
    }
    drop(transition);
    if done.hold != HoldRelease::Released {
        return Err(refused(
            "retired execution input hold could not be released",
        ));
    }
    crate::services::tui_prompt_dedupe::withhold_herdr_execution(
        logical,
        Some(&record.execution_nonce),
    );
    retired();
    let mut expected = record.clone();
    expected.state = HostedState::Retired;
    match crate::services::turn_host::for_turn(Some(pool), provider, channel.get(), key).await {
        TurnHost::Herdr(next)
            if next
                .row
                .as_ref()
                .is_some_and(|row| row.record == HostedRecord::Known(expected.clone())) =>
        {
            Ok(())
        }
        _ => Err(refused(
            "fresh Herdr execution is not admissible after retirement",
        )),
    }
}

/// Whether the row is not this runtime's own Bound execution of the asking `provider` at the
/// plan's endpoint.
fn owner_differs(
    (record, at): (&HostedExecution, &HostedLocation),
    plan: &crate::services::turn_host::HerdrTurnPlan,
    (provider, token_hash, channel, logical): (&ProviderKind, &str, ChannelId, &str),
    (root, node): (&std::path::Path, &str),
) -> bool {
    record.state != HostedState::Bound
        || record.owner.provider != provider.as_str()
        || record.owner.discord_token_hash != token_hash
        || record.owner.channel_id != channel.get().to_string()
        || record.owner.logical_key != logical
        || root != std::path::Path::new(&record.owner.runtime_root)
        || node != record.owner.owner_node
        || record.owner.owner_node != plan.endpoint.execution_node
        || at.execution_node != plan.endpoint.execution_node
        || at.endpoint_config_key != plan.endpoint.config_key
        || at.socket_addr != plan.endpoint.socket_addr
        || at.named_session != plan.endpoint.herdr_session
}

/// The channel transition lease from the proof to the CAS; a taken lock is a later look's.
fn exclusion<'a>(
    shared: &'a Shared,
    channel: ChannelId,
    lease: LeaseRef<'a>,
) -> Result<SessionTransitionLease<'a, crate::services::discord::SharedData>, DeadBound> {
    match lease {
        LeaseRef::Acquire => SessionTransitionLease::try_acquire(shared.as_ref(), channel),
        LeaseRef::Borrow(guard) => SessionTransitionLease::borrow(shared.as_ref(), channel, guard),
    }
    .map_err(|_| DeadBound::Reobserve("session transition busy".into()))
}

/// No turn token and an exactly absent inflight row; any other read leaves the row for later.
async fn no_turn(
    provider: &ProviderKind,
    channel: ChannelId,
    mailbox: &ChannelMailboxHandle,
) -> Result<(), DeadBound> {
    match mailbox.has_active_turn().await {
        Ok(false) => {}
        Ok(true) => return Err(DeadBound::Reobserve("a turn holds the channel".into())),
        Err(error) => return Err(DeadBound::Reobserve(format!("mailbox unread: {error:?}"))),
    }
    let read = crate::services::discord::inflight::load_inflight_state_read_only_result;
    match read(provider, channel.get()) {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(DeadBound::Reobserve(
            "an inflight row holds the channel".into(),
        )),
        Err(error) => Err(DeadBound::Reobserve(format!("inflight unread: {error}"))),
    }
}

fn classify(error: RetireRefusal) -> DeadBound {
    let reason = format!("retire: {error:?}");
    match error {
        RetireRefusal::Unproven(_) | RetireRefusal::ProviderRunning => DeadBound::Reobserve(reason),
        RetireRefusal::Changed(_) | RetireRefusal::NoLocalEndpoint => DeadBound::Retry(reason),
        RetireRefusal::NoRow(_) => DeadBound::Refused(reason),
    }
}

#[cfg(test)]
#[path = "dead_bound_tests.rs"]
mod tests;
#[cfg(test)]
pub(in crate::services::discord) use tests::install_before_cas;
