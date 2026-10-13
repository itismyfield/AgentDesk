//! Whether this node may take a channel's own ingress, judged from the channel's home row and this
//! node's renewal-confirmed gate. The result is an observation, never a permit: a turn, command or
//! POST still passes the gate's own admission. No runtime caller reads it yet.

use std::num::NonZeroU64;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::time::Instant;

use crate::db::o_channel_homes::{ChannelHome, HomeProvider, HomeState};
use crate::services::cluster::channel_home::{HOLD_FOR, HomeIntake, HomeOwnership};

/// The channel's ownership identity: its provider and the actual Discord channel, never a policy
/// channel, an agent, a user or a preferred node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IngressKey {
    provider: HomeProvider,
    channel: NonZeroU64,
}

impl IngressKey {
    /// Refuses a provider homes do not know and channel 0.
    pub(crate) fn parse(provider: &str, channel: u64) -> Option<Self> {
        Some(Self {
            provider: HomeProvider::parse(provider)?,
            channel: NonZeroU64::new(channel)?,
        })
    }
}

/// Where the input came from. A forwarded input keeps the home epoch its route stamped, `None` for
/// a gateway-rule row, so it is never re-admitted as a fresh source event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceEpoch {
    UnstampedSource,
    Routed(Option<i64>),
}

/// When the row was read: PG's clock at the read, and the local instant the query began.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReadClock {
    pub(crate) pg_now: Option<DateTime<Utc>>,
    pub(crate) started: Option<Instant>,
}

/// The home lookup; a failed read stays apart from a missing row.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HomeRead<'a> {
    Missing,
    Unreadable,
    Row(&'a ChannelHome, ReadClock),
}

/// One reading of this node's gate for the key; `expires_at` is the deadline its last confirmed
/// renewal set.
#[derive(Clone, Debug)]
pub(crate) struct LocalLeaseSample {
    pub(crate) key: IngressKey,
    pub(crate) holder: String,
    pub(crate) ownership: HomeOwnership,
    pub(crate) sampled_at: Instant,
    pub(crate) expires_at: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IngressObservation {
    LocalCandidate { home_epoch: i64 },
    /// This node is not a candidate; it does not vouch that the holder can run.
    IgnoreForeign { holder: String, home_epoch: i64 },
    Hold(IngressHoldReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngressHoldReason {
    NoLocalIdentity,
    AuthorityUnreadable,
    /// No row: the gateway rules still apply on the existing path; this contract never falls back.
    NoHomeAuthority,
    ChannelMismatch,
    ProviderMismatch,
    /// The row breaks the table's own constraints.
    InvalidHome,
    StaleSourceEpoch,
    /// Forwarded before the channel had a home; the claim fence never runs it on a home channel.
    RouteWithoutHomeEpoch,
    HomeNotTakingIngress(HomeState),
    TimeUnknown,
    RowExpired,
    NoLocalLease,
    LocalSampleMismatch,
    LocalLost,
    LocalEpochMismatch,
    LocalIntakeClosed,
    LocalExpired,
}

#[cfg(test)]
thread_local! {
    static OBSERVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many observations this thread made, so a test can show an entry point never makes one.
#[cfg(test)]
pub(crate) fn observed_calls() -> usize {
    OBSERVED.with(std::cell::Cell::get)
}

/// Checks identity, lookup, row shape, the source's epoch, state and freshness, then holder, then
/// the local gate, in that order, so an error is never hidden behind a foreign or local verdict.
pub(crate) fn observe(
    key: &IngressKey,
    local_instance_id: &str,
    source: SourceEpoch,
    home: HomeRead<'_>,
    local: Option<&LocalLeaseSample>,
    now: Instant,
) -> IngressObservation {
    #[cfg(test)]
    OBSERVED.with(|calls| calls.set(calls.get() + 1));
    judge(key, local_instance_id, source, home, local, now).unwrap_or_else(IngressObservation::Hold)
}

fn judge(
    key: &IngressKey,
    local_instance_id: &str,
    source: SourceEpoch,
    home: HomeRead<'_>,
    local: Option<&LocalLeaseSample>,
    now: Instant,
) -> Result<IngressObservation, IngressHoldReason> {
    use IngressHoldReason as Why;
    if local_instance_id.trim().is_empty() {
        return Err(Why::NoLocalIdentity);
    }
    let (row, clock) = match home {
        HomeRead::Unreadable => return Err(Why::AuthorityUnreadable),
        HomeRead::Missing => return Err(Why::NoHomeAuthority),
        HomeRead::Row(row, clock) => (row, clock),
    };
    if row.channel_id != key.channel.to_string() {
        return Err(Why::ChannelMismatch);
    }
    match HomeProvider::parse(&row.provider) {
        Some(provider) if provider.as_str() != row.provider => return Err(Why::InvalidHome),
        Some(provider) if provider != key.provider => return Err(Why::ProviderMismatch),
        Some(_) => {}
        None => return Err(Why::InvalidHome),
    }
    let (holder, renewed_at) = well_formed(row).ok_or(Why::InvalidHome)?;
    match source {
        SourceEpoch::Routed(Some(routed)) if routed != row.epoch => {
            return Err(Why::StaleSourceEpoch);
        }
        SourceEpoch::Routed(None) => return Err(Why::RouteWithoutHomeEpoch),
        SourceEpoch::Routed(Some(_)) | SourceEpoch::UnstampedSource => {}
    }
    let (HomeState::Worker, Some(holder), Some(renewed_at)) = (row.state, holder, renewed_at)
    else {
        return Err(Why::HomeNotTakingIngress(row.state));
    };
    let age = row_age(renewed_at, clock, now).ok_or(Why::TimeUnknown)?;
    if age >= HOLD_FOR {
        return Err(Why::RowExpired);
    }
    if holder != local_instance_id {
        return Ok(IngressObservation::IgnoreForeign {
            holder: holder.to_owned(),
            home_epoch: row.epoch,
        });
    }
    let sample = local.ok_or(Why::NoLocalLease)?;
    if sample.key != *key || sample.holder != local_instance_id {
        return Err(Why::LocalSampleMismatch);
    }
    // Only the row's epoch is compared; the gate's own acquisition count means something else.
    let HomeOwnership::Owned {
        home_epoch, intake, ..
    } = sample.ownership
    else {
        return Err(Why::LocalLost);
    };
    if home_epoch != row.epoch {
        return Err(Why::LocalEpochMismatch);
    }
    if intake != HomeIntake::Open {
        return Err(Why::LocalIntakeClosed);
    }
    let expires_at = sample.expires_at.ok_or(Why::TimeUnknown)?;
    if sample.sampled_at > now || sample.sampled_at >= expires_at {
        return Err(Why::TimeUnknown);
    }
    if now >= expires_at {
        return Err(Why::LocalExpired);
    }
    Ok(IngressObservation::LocalCandidate {
        home_epoch: row.epoch,
    })
}

/// The row's holder and lease time when it keeps the table's constraints: epoch >= 1, a holder
/// exactly in the holding states, a lease time with every holder, and no target on a worker row.
fn well_formed(row: &ChannelHome) -> Option<(Option<&str>, Option<DateTime<Utc>>)> {
    let holding = matches!(
        row.state,
        HomeState::Releasing | HomeState::Worker | HomeState::Reclaiming
    );
    let holder = row.holder.as_deref();
    let valid = row.epoch >= 1
        && holding == holder.is_some()
        && holder.is_none_or(|holder| !holder.trim().is_empty())
        && (holder.is_none() || row.renewed_at.is_some())
        && (row.state != HomeState::Worker || row.target.is_none());
    valid.then_some((holder, row.renewed_at))
}

/// An upper bound on the row's age now: its age on PG's clock at the read plus everything since the
/// query began, so a slow reply never lengthens it. Missing, negative or overflowing is unknown.
fn row_age(renewed_at: DateTime<Utc>, clock: ReadClock, now: Instant) -> Option<Duration> {
    let at_read = (clock.pg_now? - renewed_at).to_std().ok()?;
    let since_read = now.checked_duration_since(clock.started?)?;
    at_read.checked_add(since_read)
}

#[cfg(test)]
mod tests;
