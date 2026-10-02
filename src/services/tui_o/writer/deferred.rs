//! A selected channel whose first adoption met an open Legacy turn. Legacy keeps it while its host
//! retries, and O adopts it only once Legacy is idle and has delivered every record before its cursor.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::activation;
use super::adoption::{self, Hold, LegacyView, ReadVersion, Refused, Snapshot};
use super::binding::{BindingEvent, BindingEvents};
use super::host::{HostIo, release, stop, until_owned};
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

/// Pins off the runtime's threads, as pinning reads whole transcripts.
pub(super) async fn pin(
    legacy: Arc<dyn LegacyView>,
    events: Result<Vec<BindingEvent>, String>,
    channel: u64,
) -> Result<Snapshot, Refused> {
    let pinned = tokio::task::spawn_blocking(move || {
        adoption::pin(&*legacy, &events.map_err(Refused::retry)?, channel)
    });
    let pinned = pinned.await.map_err(|error| format!("pin task: {error}"));
    pinned.map_err(Refused::retry).and_then(|pinned| pinned)
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
    loop {
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
        let moved = events.last().map_or(0, |event| event.seq) != seq;
        let due = match refused.hold {
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
            Ok((None, Some(_))) | Err(_) => continue,
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
        let legacy = Arc::clone(&waiting.legacy);
        let pinned = pin(legacy, Ok(events), channel).await;
        // Unlike a boot, Legacy may still send a record past its frontier.
        let owed = |snapshot: Snapshot| match snapshot.owed() {
            Some(refused) => Err(refused),
            None => Ok(snapshot),
        };
        let snapshot = match pinned.and_then(owed) {
            Ok(snapshot) => snapshot,
            Err(again) => {
                tracing::info!(channel, refused = %again, "[tui_o] deferred adoption still waits");
                match again.hold {
                    Hold::Final => {
                        release(candidate, &alarms, channel, &again.to_string());
                        return false;
                    }
                    _ => refused = again,
                }
                continue;
            }
        };
        let mut rechecked = None;
        let sources = || {
            let sources = snapshot.recheck(&*waiting.legacy, waiting.log, channel);
            sources.map_err(|refused| {
                let detail = refused.to_string();
                rechecked = Some(refused);
                detail
            })
        };
        let local = || Ok(io.local_custody(channel, provider)? || io.relaying(channel));
        let activated =
            activation::activate_with(waiting.store, channel, facts, local, candidate, sources);
        let detail = match activated {
            Ok(()) => {
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

/// Legacy holds nothing for the channel: no tail, custody, mailbox work or emission.
async fn idle<I: HostIo>(waiting: &Waiting<'_, I>, current: Option<&(SourceId, String)>) -> bool {
    let (io, channel) = (waiting.io, waiting.channel);
    let tail = current.is_some_and(|(_, tmux)| waiting.legacy.tail_running(tmux));
    let custody = io.local_custody(channel, waiting.provider).unwrap_or(true);
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
