//! A channel whose first adoption met an open turn, a record Legacy owes, its custody or a lagging
//! cursor: O adopts once an idle Legacy owes nothing before O's start, or after `STALLED` behind.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::AlarmSink;
use super::activation;
use super::adoption::{self, At, Hold, LegacyEpoch, LegacyView, ReadVersion, Refused, Snapshot};
use super::binding::{BindingEvent, BindingEvents};
use super::host::{Custody, HostIo, release, stop, until_owned};
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::OStore;

/// How often a deferred channel is looked at again.
const RETRY: Duration = Duration::from_secs(5);
/// How long the current source stays unchanged before a retry, so Legacy's chrome lands first.
const QUIET: Duration = Duration::from_secs(10);
/// How long a refusal with nothing cheaper to watch waits, unless the current source moves.
const REREAD: Duration = Duration::from_secs(60);
/// How long Legacy may stay behind a closed, quiet source before O starts past it: longer than
/// Legacy's first redrive cycle, a threshold for giving up rather than proof Legacy is gone.
const STALLED: Duration = Duration::from_secs(40 * 60);

/// What a deferred channel's host retries with.
pub(super) struct Waiting<'a, I: HostIo> {
    pub(super) io: &'a I,
    pub(super) channel: u64,
    pub(super) provider: ShadowProvider,
    pub(super) candidate: &'a Candidate,
    pub(super) gate: &'a OwnershipGate,
    pub(super) store: &'a OStore,
    pub(super) log: &'a I::Bindings,
    pub(super) legacy: Arc<dyn LegacyView>,
    /// The source the log bound when the adoption was deferred, as of that log entry.
    pub(super) bound: (SourceId, u64),
}

/// How the first look at a channel that already holds output ends.
pub(super) enum First {
    Adopt(Snapshot),
    /// Legacy keeps the channel while the host retries.
    Wait(Refused),
    Leave(Refused),
}

/// An open turn, a record Legacy owes, its custody, or a cursor that lags a source whose end O could
/// start at waits; any other refusal leaves the channel to Legacy.
pub(super) async fn first<I: HostIo>(
    io: &I,
    channel: u64,
    provider: ShadowProvider,
    legacy: &Arc<dyn LegacyView>,
    events: Result<Vec<BindingEvent>, String>,
) -> First {
    let pinned = pin(Arc::clone(legacy), events.clone(), channel, At::Cursor).await;
    let refused = match pinned {
        Ok(snapshot) => {
            // Legacy resumes from its frontier after a restart, so what it owes is not given up yet.
            if let Some(owed) = snapshot.owed() {
                return First::Wait(Refused::retry(owed.to_string()));
            }
            return match io.local_custody(channel, provider) {
                Ok(Custody::Row | Custody::Active) => {
                    First::Wait(Refused::retry("Legacy retains delivery custody"))
                }
                // A failed read refuses under the adoption lock.
                Ok(Custody::Free) | Err(_) => First::Adopt(snapshot),
            };
        }
        Err(refused) => refused,
    };
    match refused.hold {
        Hold::OpenTurn(_) => First::Wait(refused),
        Hold::Cursor { .. } => match pin(Arc::clone(legacy), events, channel, At::End).await {
            Ok(_) => First::Wait(Refused::retry(refused.to_string())),
            Err(again) if matches!(again.hold, Hold::OpenTurn(_)) => First::Wait(again),
            Err(_) => First::Leave(refused),
        },
        _ => First::Leave(refused),
    }
}

/// Pins off the runtime's threads, as pinning reads whole transcripts.
pub(super) async fn pin(
    legacy: Arc<dyn LegacyView>,
    events: Result<Vec<BindingEvent>, String>,
    channel: u64,
    at: At,
) -> Result<Snapshot, Refused> {
    let pinned = tokio::task::spawn_blocking(move || {
        adoption::pin_at(&*legacy, &events.map_err(Refused::retry)?, channel, at)
    });
    let pinned = pinned.await.map_err(|error| format!("pin task: {error}"));
    pinned.map_err(Refused::retry).and_then(|pinned| pinned)
}

/// Legacy behind a closed source since `since`, as `snapshot` and `epoch` read it then.
struct Stall {
    snapshot: Snapshot,
    epoch: LegacyEpoch,
    since: Instant,
}

impl Stall {
    fn holds(&self, legacy: &dyn LegacyView, channel: u64) -> bool {
        self.snapshot.unchanged(legacy, channel) && legacy.epoch(channel) == self.epoch
    }
}

/// Retries until the channel commits (true) or is left to Legacy for good (false). `refused` is
/// the last refusal and `seq` the binding log entry its read ended at.
pub(super) async fn retry<I: HostIo>(waiting: Waiting<'_, I>, refused: Refused, seq: u64) -> bool {
    let Waiting {
        io,
        channel,
        provider,
        candidate,
        ..
    } = waiting;
    let alarms = io.alarms();
    let (mut refused, mut seq, mut pinned_at) = (refused, seq, Instant::now());
    let (mut quiet, mut read_at) = (Quiet::default(), None);
    let mut stall: Option<Stall> = None;
    'retry: loop {
        tokio::time::sleep(RETRY).await;
        if candidate.peek() != Adoption::Deferred {
            stop(
                candidate,
                &alarms,
                channel,
                "adoption left Deferred outside its host",
            );
            return false;
        }
        until_owned(waiting.gate).await;
        let Ok(events) = waiting.log.binding_events_since(channel, 0) else {
            continue;
        };
        let current = adoption::current(&events);
        let version = current
            .as_ref()
            .and_then(|(source, _)| ReadVersion::of(&source.path));
        let settled = quiet.settled(&version);
        // A running clock is checked every tick so no Legacy activity slips past it; a stopped one
        // is read again on the next quiet, idle tick, which may start a new one.
        if let Some(clock) = &stall
            && !(clock.holds(&*waiting.legacy, channel) && idle(&waiting, current.as_ref()).await)
        {
            (stall, read_at) = (None, None);
            refused = Refused::retry("Legacy moved while O waited past it");
        }
        let moved = events.last().map_or(0, |event| event.seq) != seq;
        let expired = stall
            .as_ref()
            .is_some_and(|clock| clock.since.elapsed() >= STALLED);
        // A running clock needs no reread: it ends by expiring or by being stopped above.
        let due = expired
            || stall.is_none()
                && match refused.hold {
                    Hold::Retry => version != read_at || pinned_at.elapsed() >= REREAD,
                    _ => refused.may_pass(&*waiting.legacy, channel),
                };
        if !settled || !(moved || due) || !idle(&waiting, current.as_ref()).await {
            continue;
        }
        let facts = io.activation_facts(channel, provider).await;
        if !matches!(waiting.gate.current(), GatewayOwnership::Owned { .. }) {
            continue;
        }
        match facts
            .as_ref()
            .map(|facts| (facts.final_blocker(), facts.transient_blocker()))
        {
            Ok((Some(detail), _)) => {
                release(candidate, &alarms, channel, &detail);
                return false;
            }
            Ok((None, None)) => {}
            Ok((None, Some(_))) | Err(_) => {
                if stall.take().is_some() {
                    read_at = None;
                    refused = Refused::retry("Legacy's intake moved while O waited past it");
                }
                continue;
            }
        }
        // Legacy may still owe output read from any other source bound since the deferral.
        let (source, since) = &waiting.bound;
        if let Some(rotated) = adoption::rotated(&events, *since, source) {
            let path = rotated.path.display();
            let detail = format!("source {path} was bound while the adoption waited");
            release(candidate, &alarms, channel, &detail);
            return false;
        }
        seq = events.last().map_or(0, |event| event.seq);
        (pinned_at, read_at) = (Instant::now(), version);
        // A stall that ran out is still checked under the lock, as Legacy may move before it.
        let (snapshot, ended) = 'read: {
            if expired && let Some(clock) = stall.take() {
                tracing::info!(
                    channel,
                    "[tui_o] Legacy stayed behind; O starts at the source's end"
                );
                break 'read (clock.snapshot, Some(clock.epoch));
            }
            let legacy = Arc::clone(&waiting.legacy);
            let pinned = pin(Arc::clone(&legacy), Ok(events.clone()), channel, At::Cursor).await;
            // Legacy may still send a record past its frontier.
            let (mut again, end) = match pinned {
                Ok(snapshot) => match snapshot.owed() {
                    None => break 'read (snapshot, None),
                    Some(again) => (again, Some(Ok(snapshot))),
                },
                Err(again) if matches!(again.hold, Hold::Cursor { .. }) => {
                    let end = pin(legacy, Ok(events), channel, At::End).await;
                    (again, Some(end))
                }
                Err(again) => (again, None),
            };
            tracing::info!(channel, refused = %again, "[tui_o] deferred adoption still waits");
            if again.hold == Hold::Final {
                release(candidate, &alarms, channel, &again.to_string());
                return false;
            }
            match end {
                // Legacy behind a closed source starts the clock; a running one keeps its start.
                Some(Ok(end)) if stall.is_none() && end.behind() => {
                    let epoch = waiting.legacy.epoch(channel);
                    let since = Instant::now();
                    stall = Some(Stall {
                        snapshot: end,
                        epoch,
                        since,
                    });
                }
                // An open turn at the end is waited on until the source moves.
                Some(Err(open)) if matches!(open.hold, Hold::OpenTurn(_)) => again = open,
                _ => {}
            }
            refused = again;
            continue 'retry;
        };
        stall = None;
        let mut rechecked = None;
        let sources = || {
            let legacy = &*waiting.legacy;
            let moved = ended.as_ref().is_some_and(|epoch| {
                !snapshot.unchanged(legacy, channel) || legacy.epoch(channel) != *epoch
            });
            let sources = if moved {
                Err(Refused::retry("Legacy moved before O took the channel"))
            } else {
                snapshot.recheck(legacy, waiting.log, channel)
            };
            sources.map_err(|refused| {
                let detail = refused.to_string();
                rechecked = Some(refused);
                detail
            })
        };
        let local = || {
            let custody = io.local_custody(channel, provider)?;
            Ok(custody == Custody::Active || io.relaying(channel))
        };
        let activated =
            activation::activate_with(waiting.store, channel, facts, local, candidate, sources);
        let detail = match activated {
            Ok(()) => {
                if let Some(alarm) = snapshot.abandoned() {
                    tracing::info!(
                        channel,
                        ?alarm,
                        "[tui_o] adopted past Legacy's stalled records"
                    );
                    alarms.raise(channel, alarm);
                }
                tracing::info!(channel, "[tui_o] deferred adoption committed");
                return true;
            }
            Err(detail) => detail,
        };
        if candidate.peek() != Adoption::Deferred {
            stop(
                candidate,
                &alarms,
                channel,
                &format!("first activation: {detail}"),
            );
            return false;
        }
        tracing::info!(channel, detail, "[tui_o] deferred adoption still waits");
        refused = match rechecked {
            Some(again) if again.hold == Hold::Final => {
                release(candidate, &alarms, channel, &again.to_string());
                return false;
            }
            Some(again) => again,
            None => Refused::retry(detail),
        };
    }
}

/// Legacy holds nothing for the channel: no tail, active custody, mailbox work or emission. An
/// inflight row alone does not count; what it may still owe is judged from the frontier.
async fn idle<I: HostIo>(waiting: &Waiting<'_, I>, current: Option<&(SourceId, String)>) -> bool {
    let (io, channel) = (waiting.io, waiting.channel);
    let tail = current.is_some_and(|(_, tmux)| waiting.legacy.tail_running(tmux));
    let custody = io.local_custody(channel, waiting.provider);
    let custody = !matches!(custody, Ok(Custody::Free | Custody::Row));
    !tail && !custody && !io.relaying(channel) && !io.legacy_busy(channel).await
}

/// How long the current source has stood unchanged, as this loop saw it.
#[derive(Default)]
struct Quiet {
    seen: Option<(Option<ReadVersion>, Instant)>,
}

impl Quiet {
    fn settled(&mut self, now: &Option<ReadVersion>) -> bool {
        match &self.seen {
            Some((seen, since)) if seen == now => since.elapsed() >= QUIET,
            _ => {
                self.seen = Some((now.clone(), Instant::now()));
                false
            }
        }
    }
}
