//! A channel whose first adoption met an open turn, a record Legacy owes, its custody or a lagging
//! cursor: O adopts once an idle Legacy owes nothing, or at EOF after delivery stops progressing.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::AlarmSink;
use super::activation;
use super::adoption::{self, At, Hold, LegacyEpoch, LegacyView, ReadVersion, Refused, Snapshot};
use super::binding::{BindingEvent, BindingEvents};
use super::host::{Custody, FencedFacts, HostIo, release, stop, until_owned};
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
/// No delivery progress for longer than Legacy's first redrive cycle permits an EOF handoff.
const STALLED: Duration = Duration::from_secs(40 * 60);
/// Frontier progress alone resets this cap, so failed sends or redrive churn cannot defer forever.
const STALLED_HARD: Duration = Duration::from_secs(80 * 60);

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

/// Delivery progress, independent of transcript growth and Legacy's activity markers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Progress {
    frontier: Option<u64>,
    sends: (u64, u64),
    epoch: LegacyEpoch,
}

impl Progress {
    fn at<I: HostIo>(waiting: &Waiting<'_, I>, tmux: &str, eof: u64) -> Self {
        Self {
            frontier: waiting.legacy.frontier(waiting.channel, tmux, eof),
            sends: waiting.candidate.sends(),
            epoch: waiting.legacy.epoch(waiting.channel),
        }
    }

    fn read<I: HostIo>(
        waiting: &Waiting<'_, I>,
        current: Option<&(SourceId, String)>,
    ) -> Option<Self> {
        let (source, tmux) = current?;
        let eof = std::fs::metadata(&source.path).ok()?.len();
        let _adoption = waiting.candidate.lock();
        Some(Self::at(waiting, tmux, eof))
    }

    fn in_flight(&self) -> bool {
        activation::body_in_flight(self.sends)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StallKind {
    Soft,
    Hard,
}

impl StallKind {
    fn name(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }
}

struct Clocks {
    seen: Option<Progress>,
    soft_since: Instant,
    hard_since: Instant,
}

impl Clocks {
    fn new(at: Instant, seen: Option<Progress>) -> Self {
        Self {
            seen,
            soft_since: at,
            hard_since: at,
        }
    }

    fn observe(&mut self, now: Option<Progress>) {
        let Some(now) = now else {
            return;
        };
        if let Some(seen) = &self.seen {
            let at = Instant::now();
            if seen != &now {
                self.soft_since = at;
            }
            if seen.frontier != now.frontier {
                self.hard_since = at;
            }
        }
        self.seen = Some(now);
    }

    fn expired(&self) -> Option<StallKind> {
        if self.soft_since.elapsed() >= STALLED {
            Some(StallKind::Soft)
        } else if self.hard_since.elapsed() >= STALLED_HARD {
            Some(StallKind::Hard)
        } else {
            None
        }
    }

    fn elapsed(&self, kind: StallKind) -> Duration {
        match kind {
            StallKind::Soft => self.soft_since.elapsed(),
            StallKind::Hard => self.hard_since.elapsed(),
        }
    }
}

fn events<I: HostIo>(waiting: &Waiting<'_, I>) -> Result<Vec<BindingEvent>, String> {
    waiting
        .log
        .binding_events_since(waiting.channel, 0)
        .map(super::renumbered::first_named)
}

fn latest_progress<I: HostIo>(waiting: &Waiting<'_, I>) -> Option<Progress> {
    let events = events(waiting).ok()?;
    let current = adoption::current(&events);
    Progress::read(waiting, current.as_ref())
}

/// Retries until the channel commits (true) or is left to Legacy for good (false). `refused` is
/// the last refusal and `seq` the binding log entry its read ended at.
pub(super) async fn retry<I: HostIo>(waiting: Waiting<'_, I>, refused: Refused, seq: u64) -> bool {
    let Waiting {
        io,
        channel,
        candidate,
        ..
    } = waiting;
    let alarms = io.alarms();
    let started = Instant::now();
    #[cfg(test)]
    if let Err(detail) =
        activation::test_hook::run(channel, activation::test_hook::Step::DeferredStarted)
    {
        stop(candidate, &alarms, channel, &detail);
        return false;
    }
    let mut clocks = Clocks::new(started, latest_progress(&waiting));
    let mut last_logged = Some(refused.to_string());
    let (mut refused, mut seq, mut pinned_at) = (refused, seq, started);
    let (mut quiet, mut read_at) = (Quiet::default(), None);
    let mut handoff_refused: Option<Refused> = None;
    loop {
        #[cfg(test)]
        if let Err(detail) =
            activation::test_hook::run(channel, activation::test_hook::Step::DeferredTick)
        {
            stop(candidate, &alarms, channel, &detail);
            return false;
        }
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
        let Ok(observed) = events(&waiting) else {
            continue;
        };
        let current = adoption::current(&observed);
        let version = current
            .as_ref()
            .and_then(|(source, _)| ReadVersion::of(&source.path));
        clocks.observe(Progress::read(&waiting, current.as_ref()));
        let settled = quiet.settled(&version);
        let moved = observed.last().map_or(0, |event| event.seq) != seq;
        let due = match &refused.hold {
            Hold::Retry => version != read_at || pinned_at.elapsed() >= REREAD,
            _ => refused.may_pass(&*waiting.legacy, channel),
        };
        if settled && (moved || due) && idle(&waiting, current.as_ref()).await {
            if let Some(detail) = rotation(&waiting, &observed) {
                release(candidate, &alarms, channel, &detail);
                return false;
            }
            seq = observed.last().map_or(0, |event| event.seq);
            (pinned_at, read_at) = (Instant::now(), version);
            let pinned = pin(
                Arc::clone(&waiting.legacy),
                Ok(observed),
                channel,
                At::Cursor,
            )
            .await;
            let again = match pinned {
                Ok(snapshot) => match snapshot.owed() {
                    Some(again) => again,
                    None => match commit(&waiting, &snapshot, None, None).await {
                        Ok(()) => {
                            committed(&waiting, &alarms, &snapshot);
                            return true;
                        }
                        Err(again) => {
                            clocks.observe(latest_progress(&waiting));
                            again
                        }
                    },
                },
                Err(again) => again,
            };
            if leaves(&waiting, &alarms, &again) {
                return false;
            }
            note_wait(
                channel,
                &again,
                &mut last_logged,
                clocks.expired().is_some(),
            );
            refused = again;
        }
        if clocks.expired().is_none() {
            continue;
        }
        // A failed normal commit may have changed a source; EOF handoff still waits for quiet.
        let Ok(observed) = events(&waiting) else {
            continue;
        };
        let current = adoption::current(&observed);
        let version = current
            .as_ref()
            .and_then(|(source, _)| ReadVersion::of(&source.path));
        clocks.observe(Progress::read(&waiting, current.as_ref()));
        let Some(kind) = clocks.expired() else {
            continue;
        };
        if !quiet.settled(&version) {
            continue;
        }
        if handoff_refused.as_ref().is_some_and(|again| {
            matches!(again.hold, Hold::OpenTurn(_) | Hold::Delivery { .. })
                && !again.may_pass(&*waiting.legacy, channel)
        }) {
            continue;
        }
        let Some(token) = clocks.seen.clone() else {
            continue;
        };
        if token.in_flight() {
            let again = Refused::retry("Legacy body send in flight");
            note_wait(channel, &again, &mut last_logged, true);
            continue;
        }
        let relaxed = relaxed(&waiting, current.as_ref()).await;
        if relay_blocks(Some(kind), relaxed.contains(&"relaying")) {
            let again = Refused::retry("Legacy is relaying at the hard stall limit");
            note_wait(channel, &again, &mut last_logged, true);
            continue;
        }
        if let Some(detail) = rotation(&waiting, &observed) {
            release(candidate, &alarms, channel, &detail);
            return false;
        }
        let pinned = pin(Arc::clone(&waiting.legacy), Ok(observed), channel, At::End).await;
        let snapshot = match pinned {
            Ok(snapshot) => snapshot,
            Err(again) => {
                if leaves(&waiting, &alarms, &again) {
                    return false;
                }
                note_wait(channel, &again, &mut last_logged, true);
                handoff_refused = Some(again);
                continue;
            }
        };
        // An EOF repin may observe fresh output after this tick's quiet check.
        let version_after_pin = current
            .as_ref()
            .and_then(|(source, _)| ReadVersion::of(&source.path));
        if !quiet.settled(&version_after_pin) {
            continue;
        }
        let activated = commit(&waiting, &snapshot, Some((&token, kind)), current.as_ref()).await;
        match activated {
            Ok(()) => {
                tracing::info!(
                    channel,
                    kind = kind.name(),
                    stalled_secs = clocks.elapsed(kind).as_secs(),
                    ?relaxed,
                    "[tui_o] Legacy delivered nothing through the stall; O starts at the source's end"
                );
                committed(&waiting, &alarms, &snapshot);
                return true;
            }
            Err(again) => {
                clocks.observe(latest_progress(&waiting));
                if leaves(&waiting, &alarms, &again) {
                    return false;
                }
                note_wait(channel, &again, &mut last_logged, true);
                handoff_refused = Some(again);
            }
        }
    }
}

/// Fresh facts, ownership and Legacy claims protect the entire synchronous init publication.
async fn commit<I: HostIo>(
    waiting: &Waiting<'_, I>,
    snapshot: &Snapshot,
    end: Option<(&Progress, StallKind)>,
    current: Option<&(SourceId, String)>,
) -> Result<(), Refused> {
    let (io, channel, provider) = (waiting.io, waiting.channel, waiting.provider);
    let GatewayOwnership::Owned { epoch: expected } = waiting.gate.current() else {
        return Err(Refused::retry(
            "gateway ownership changed before O activation",
        ));
    };
    #[cfg(test)]
    activation::test_hook::run(channel, activation::test_hook::Step::BeforeFence)
        .map_err(Refused::retry)?;
    let fence_sends = {
        let _adoption = waiting.candidate.lock();
        waiting.candidate.sends()
    };
    let FencedFacts {
        hold,
        facts,
        queued_bodies,
    } = io
        .intake_fence(channel, provider)
        .await
        .map_err(Refused::retry)?;
    let activated = (|| {
        #[cfg(test)]
        activation::test_hook::run(channel, activation::test_hook::Step::AfterFence)
            .map_err(Refused::retry)?;
        if let Some(detail) = facts.final_blocker() {
            return Err(Refused::new(Hold::Final, detail));
        }
        if let Some(detail) = facts.transient_blocker() {
            return Err(Refused::retry(detail));
        }
        if queued_bodies != 0 {
            return Err(Refused::retry(format!(
                "{queued_bodies} queued Legacy bodies"
            )));
        }
        let mut rechecked = None;
        let sources = || {
            let sources = (|| {
                if waiting.candidate.sends() != fence_sends {
                    return Err(Refused::retry(
                        "Legacy body send changed while O fenced activation",
                    ));
                }
                if let Some((token, _)) = end {
                    let (_, tmux) = current.ok_or_else(|| {
                        Refused::retry("no current source to recheck Legacy progress")
                    })?;
                    let now = Progress::at(waiting, tmux, snapshot.start());
                    if now.in_flight() || now.sends.0 != token.sends.0 {
                        return Err(Refused::retry(
                            "Legacy body send in flight or newly started",
                        ));
                    }
                    if &now != token {
                        return Err(Refused::retry("Legacy moved before O took the channel"));
                    }
                    snapshot.recheck_past_stall(&*waiting.legacy, waiting.log, channel)
                } else {
                    snapshot.recheck(&*waiting.legacy, waiting.log, channel)
                }
            })();
            sources.map_err(|again| {
                let detail = again.to_string();
                rechecked = Some(again);
                detail
            })
        };
        let local = || {
            let custody = io.local_custody(channel, provider)?;
            Ok((end.is_none() && custody == Custody::Active)
                || relay_blocks(end.map(|(_, kind)| kind), io.relaying(channel)))
        };
        let result = waiting
            .gate
            .admit(|epoch| {
                (epoch == expected).then(|| {
                    activation::activate_with(
                        waiting.store,
                        channel,
                        Ok(facts),
                        local,
                        waiting.candidate,
                        sources,
                    )
                })
            })
            .flatten();
        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(detail)) => Err(rechecked.unwrap_or_else(|| Refused::retry(detail))),
            None => Err(Refused::retry(
                "gateway ownership changed before O activation",
            )),
        }
    })();
    if let Err(error) = hold.release().await {
        tracing::warn!(channel, %error, "[tui_o] intake fence rollback failed");
    }
    activated
}

// Hard expiry retains the relay slot through locked activation; soft expiry may pass it.
fn relay_blocks(kind: Option<StallKind>, relaying: bool) -> bool {
    relaying && kind != Some(StallKind::Soft)
}

fn rotation<I: HostIo>(waiting: &Waiting<'_, I>, events: &[BindingEvent]) -> Option<String> {
    let (source, since) = &waiting.bound;
    let rotated = adoption::rotated(events, *since, source)?;
    Some(format!(
        "source {} was bound while the adoption waited",
        rotated.path.display()
    ))
}

fn leaves<I: HostIo>(waiting: &Waiting<'_, I>, alarms: &I::Alarms, again: &Refused) -> bool {
    if waiting.candidate.peek() != Adoption::Deferred {
        stop(
            waiting.candidate,
            alarms,
            waiting.channel,
            &format!("first activation: {again}"),
        );
        return true;
    }
    if again.hold == Hold::Final {
        release(
            waiting.candidate,
            alarms,
            waiting.channel,
            &again.to_string(),
        );
        return true;
    }
    false
}

fn note_wait(channel: u64, again: &Refused, last: &mut Option<String>, stalled: bool) {
    let detail = again.to_string();
    if last.as_ref() != Some(&detail) {
        if stalled {
            tracing::info!(channel, refused = %again, "[tui_o] stalled adoption still waits");
        } else {
            tracing::info!(channel, refused = %again, "[tui_o] deferred adoption still waits");
        }
        *last = Some(detail);
    }
}

fn committed<I: HostIo>(waiting: &Waiting<'_, I>, alarms: &I::Alarms, snapshot: &Snapshot) {
    if let Some(alarm) = snapshot.abandoned() {
        tracing::info!(
            channel = waiting.channel,
            ?alarm,
            "[tui_o] adopted past Legacy's stalled records"
        );
        alarms.raise(waiting.channel, alarm);
    }
    tracing::info!(
        channel = waiting.channel,
        "[tui_o] deferred adoption committed"
    );
}

async fn relaxed<I: HostIo>(
    waiting: &Waiting<'_, I>,
    current: Option<&(SourceId, String)>,
) -> Vec<&'static str> {
    let (io, channel) = (waiting.io, waiting.channel);
    let mut markers = Vec::new();
    if current.is_some_and(|(_, tmux)| waiting.legacy.tail_running(tmux)) {
        markers.push("tail");
    }
    if matches!(
        io.local_custody(channel, waiting.provider),
        Ok(Custody::Active)
    ) {
        markers.push("custody");
    }
    if io.legacy_busy(channel).await {
        markers.push("busy");
    }
    if io.relaying(channel) {
        markers.push("relaying");
    }
    markers
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
