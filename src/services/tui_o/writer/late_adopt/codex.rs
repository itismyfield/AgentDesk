//! Codex late takeover: a fresh rollout from its first byte, or one that holds output from the
//! first turn Legacy's records do not show delivered; a Legacy send since the baseline skips it.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use super::Wake;
use crate::services::tui_o::channel_policy::{Adoption, Candidate, FirstProof};
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::{InitOrigin, InitSource, OStore};
use crate::services::tui_o::writer::activation;
use crate::services::tui_o::writer::adoption;
use crate::services::tui_o::writer::binding::{
    BindingEvent, BindingEvents, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::host::{FencedFacts, HostIo, release, until_owned};
use crate::services::tui_o::writer::renumbered;
use crate::services::tui_o::writer::rotation::bound_source;

#[cfg(test)]
pub(crate) mod normalization;

mod delivered;
pub(super) mod prefix;
use delivered::Records;
use prefix::{Prefix, Verdict};

/// How far a turn opener's own clock may trail this host's when judged against a baseline.
const BIND_CLOCK_SLACK: chrono::TimeDelta = chrono::TimeDelta::seconds(1);

/// Legacy's sends, then the last binding seq: only a rollout bound after it, with no Legacy send
/// started since, may be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Baseline {
    pub(super) sends: (u64, u64),
    pub(super) seq: u64,
    pub(super) order: u64,
    pub(super) turn_mark: bool,
}

impl Baseline {
    pub(super) fn detail(self) -> String {
        format!(
            "baseline={} order={} sends={:?} seq={}",
            if self.turn_mark { "turn" } else { "host" },
            self.order,
            self.sends,
            self.seq
        )
    }
}

/// Why a takeover attempt did not commit.
#[derive(Debug)]
pub(super) enum Taken {
    /// The same rollout is judged again on the next wake.
    Retry(String),
    /// An eligible marked prefix hit a transient commit blocker.
    Soon(String),
    /// The rollout grew while still eligible; it is judged again at once.
    Again(String),
    /// A Legacy body started since the baseline; the rollout's open turn stays Legacy's.
    Skip(String),
    /// The channel stays Legacy's for this process.
    Final(String),
}

impl Taken {
    pub(super) fn detail(&self) -> &str {
        match self {
            Self::Retry(detail)
            | Self::Soon(detail)
            | Self::Again(detail)
            | Self::Skip(detail)
            | Self::Final(detail) => detail,
        }
    }
}

/// The init a takeover commits, judged again under the fence.
#[derive(Clone, Copy)]
pub(super) enum Plan<'p> {
    /// A rollout before any output, read from its first byte, after any source owed before it.
    Fresh(&'p Prefix, &'p [InitSource]),
    /// Sources started at their frontiers; the bytes before each must still hash the same.
    Warm(&'p [InitSource]),
}

/// Where O starts on a rollout that holds output.
enum Start {
    /// The earliest turn, ended or not, that Legacy's records do not show delivered.
    Owed(u64),
    /// The end of its last complete record: Legacy's records show every turn delivered.
    End(u64),
    /// Legacy may have sent part of that turn; it is judged again later.
    Legacy(String),
}

/// When the baseline's sends were read: a turn opened after then holds no Legacy send while
/// they hold.
#[derive(Clone, Copy)]
pub(super) struct Since {
    at: DateTime<Utc>,
    /// False for a turn mark, whose sends were read at a time this host does not know.
    proves: bool,
}

/// One provider's takeover: its prefix judge and the origin its init records.
pub(super) struct Takeover<'a, I: HostIo> {
    pub(super) io: &'a I,
    pub(super) channel: u64,
    pub(super) store: &'a OStore,
    pub(super) log: &'a I::Bindings,
    pub(super) candidate: &'a Candidate,
    pub(super) gate: &'a OwnershipGate,
    pub(super) provider: ShadowProvider,
    pub(super) judge: fn(&SourceId) -> Verdict,
    pub(super) origin: InitOrigin,
}

/// Waits for a rollout bound after the baseline that holds no output yet and commits it; false
/// once the channel left Deferred or stays Legacy's.
pub(super) async fn activate<I: HostIo>(
    io: &I,
    channel: u64,
    store: &OStore,
    log: &I::Bindings,
    candidate: &Candidate,
    gate: &OwnershipGate,
) -> bool {
    let alarms = io.alarms();
    let mut wake = Wake::new(log, channel);
    let takeover = Takeover {
        io,
        channel,
        store,
        log,
        candidate,
        gate,
        provider: ShadowProvider::Codex,
        judge: prefix::judge,
        origin: InitOrigin::LateCodexTakeover,
    };
    let mut admission_changes = candidate.first_changes();
    let (mut baseline, mut logged) = (None, None);
    let mut sampled_at = Utc::now();
    let (mut retry_order, mut short_retries) = (0, 0);
    // Rollouts a Legacy body was sent from while bound, this host run.
    let mut sent_from = Vec::new();
    // Rollouts judged owed or retried, until Legacy's records cover them or an init names them.
    let mut unresolved = Vec::new();
    loop {
        if candidate.peek() != Adoption::Deferred {
            return false;
        }
        if io.late_adopt_enabled() {
            while gate.current().epoch().is_none() {
                candidate.invalidate_first(None, None);
                tokio::select! {
                    () = until_owned(gate) => {},
                    changed = admission_changes.changed() => { if changed.is_err() { return false; } }
                }
            }
            let fresh = baseline.is_none();
            let sampled = baseline.map_or_else(|| takeover.baseline(), Ok);
            if fresh && sampled.is_ok() {
                sampled_at = Utc::now();
            }
            let mut used = None;
            let outcome = match sampled {
                Ok(at) => {
                    baseline = Some(at);
                    let at = takeover.newest(at);
                    used = Some(at);
                    if retry_order != at.order {
                        (retry_order, short_retries) = (at.order, 0);
                    }
                    let since = Since {
                        at: sampled_at,
                        proves: !at.turn_mark,
                    };
                    let seen = (&mut sent_from, &mut unresolved);
                    takeover.attempt(at, since, seen).await
                }
                Err(taken) => Err(taken),
            };
            match outcome {
                Ok(()) => {
                    let detail = used.expect("successful attempt has a baseline").detail();
                    tracing::info!(channel, detail, "[tui_o] Codex takeover committed");
                    return true;
                }
                Err(Taken::Final(detail)) => {
                    release(candidate, &alarms, channel, &detail);
                    return false;
                }
                Err(Taken::Again(_)) => continue,
                Err(Taken::Skip(detail)) => {
                    candidate.decline_first(used.expect("attempt baseline").order);
                    let detail = format!("{detail}; {}", used.expect("attempt baseline").detail());
                    tracing::info!(channel, detail, "[tui_o] Codex takeover skips this rollout");
                    baseline = None;
                    #[cfg(test)]
                    tokio::task::yield_now().await;
                    continue;
                }
                Err(Taken::Soon(detail)) => {
                    if candidate.first_ended() {
                        if let Some(at) = used {
                            candidate.decline_first(at.order);
                        }
                    }
                    if logged.as_ref() != Some(&detail) {
                        tracing::info!(channel, detail, "[tui_o] Codex takeover waits");
                        logged = Some(detail);
                    }
                    if short_retries < 8 {
                        short_retries += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        continue;
                    }
                }
                Err(Taken::Retry(detail)) if logged.as_ref() != Some(&detail) => {
                    if candidate.first_ended() {
                        if let Some(at) = used {
                            candidate.decline_first(at.order);
                        }
                    }
                    tracing::info!(channel, detail, "[tui_o] Codex takeover waits");
                    logged = Some(detail);
                }
                Err(Taken::Retry(_)) => {
                    if candidate.first_ended() {
                        if let Some(at) = used {
                            candidate.decline_first(at.order);
                        }
                    }
                }
            }
        } else {
            candidate.invalidate_first(None, None);
        }
        tokio::select! {
            () = wake.wait() => {},
            changed = admission_changes.changed() => { if changed.is_err() { return false; } }
        }
    }
}

impl<I: HostIo> Takeover<'_, I> {
    pub(super) fn events(&self) -> Result<Vec<BindingEvent>, Taken> {
        let events = self.log.binding_events_since(self.channel, 0);
        events.map(renumbered::first_named).map_err(Taken::Final)
    }

    /// Sends are read before the log, so a rollout bound in between counts as already there.
    fn baseline(&self) -> Result<Baseline, Taken> {
        let (sends, order) = {
            let _adoption = self.candidate.lock();
            self.candidate.baseline_sends()
        };
        let seq = self.events()?.last().map_or(0, |event| event.seq);
        Ok(Baseline {
            sends,
            seq,
            order,
            turn_mark: false,
        })
    }

    fn newest(&self, at: Baseline) -> Baseline {
        let first = self.candidate.first_mark();
        match first.or_else(|| self.candidate.turn_mark()) {
            Some(mark) if first.is_some() || mark.order > at.order => Baseline {
                sends: mark.sends,
                seq: mark.seq,
                order: mark.order,
                turn_mark: true,
            },
            _ => at,
        }
    }

    async fn attempt(
        &self,
        at: Baseline,
        since: Since,
        (sent_from, unresolved): (&mut Vec<SourceId>, &mut Vec<SourceId>),
    ) -> Result<(), Taken> {
        let admitted = self.candidate.first_changes().borrow().is_some();
        if !self.candidate.first_ready() || (admitted && self.candidate.first_mark().is_none()) {
            return Err(Taken::Retry(
                "first admission is not current and ready".into(),
            ));
        }
        let events = self.events()?;
        let fresh = bound_after(&events, at.seq);
        let Some(bound) = fresh.clone().or_else(|| bound_after(&events, 0)) else {
            return Err(Taken::Retry("no rollout bound since the baseline".into()));
        };
        if admitted {
            let bound = fresh.ok_or_else(|| {
                Taken::Retry("no admitted rollout bound since the baseline".into())
            })?;
            return match prefix::buffered(&bound.0) {
                Verdict::Eligible(prefix) if prefix.prompt_end.is_some() => {
                    // The admission proves only this rollout; earlier undelivered ones stay owed.
                    let records = self.records(&events)?;
                    let judged = (&records, sent_from.as_slice(), &mut *unresolved);
                    let owed = self.owed_unresolved(&events, &bound, (at, since), judged)?;
                    let taken = self.commit_owed(at, &bound, &prefix, &owed).await;
                    self.soon(at, taken)
                }
                Verdict::Eligible(_) => Err(Taken::Retry(
                    "the takeover rollout has no first user prompt yet".into(),
                )),
                Verdict::Wait(detail) => Err(Taken::Retry(detail)),
                Verdict::Refuse(detail) => Err(Taken::Skip(detail)),
            };
        }
        let verdict = fresh.map(|bound| prefix::judge(&bound.0));
        // A fresh rollout judged now stays a candidate until a record covers it, as a warm one does.
        if matches!(verdict, Some(Verdict::Eligible(_) | Verdict::Wait(_))) {
            remember(unresolved, &bound.0);
        }
        let result = match verdict {
            // Turn mode is confirmed after the taken turn, so a rollout with no prompt is not taken yet.
            Some(Verdict::Eligible(prefix)) if prefix.prompt_end.is_none() => Err(Taken::Retry(
                "the takeover rollout has no first user prompt yet".into(),
            )),
            // A Legacy send since the baseline resamples it before the earlier rollout is judged.
            Some(Verdict::Eligible(_)) if self.sends_moved(at) => Err(Taken::Skip(
                "a Legacy body send started since the baseline".into(),
            )),
            Some(Verdict::Eligible(prefix)) => {
                let records = self.records(&events)?;
                let judged = (&records, sent_from.as_slice(), &mut *unresolved);
                let owed = match self.owed(&events, &bound, (at, since), judged) {
                    Ok(owed) => owed,
                    Err(taken) => return Err(taken),
                };
                let taken = self.fenced(at, &bound, Plan::Fresh(&prefix, &owed)).await;
                self.soon(at, taken)
            }
            Some(Verdict::Wait(detail)) => Err(Taken::Retry(detail)),
            // A rollout that holds output, or none bound since the baseline, starts at its frontier.
            Some(Verdict::Refuse(_)) | None => {
                let records = self.records(&events)?;
                let judged = (&records, sent_from.as_slice(), &mut *unresolved);
                self.warm(&events, &bound, (at, since), judged).await
            }
        };
        if matches!(result, Err(Taken::Skip(_))) && !sent_from.contains(&bound.0) {
            sent_from.push(bound.0.clone());
        }
        result
    }

    /// A retry while the turn the baseline marked is still current is taken again after a short wait.
    fn soon(&self, at: Baseline, taken: Result<(), Taken>) -> Result<(), Taken> {
        taken.map_err(|taken| match taken {
            Taken::Retry(detail)
                if at.turn_mark
                    && self
                        .candidate
                        .turn_mark()
                        .is_some_and(|mark| mark.order == at.order) =>
            {
                Taken::Soon(detail)
            }
            other => other,
        })
    }

    /// Whether a Legacy body send started since `at` read the sends.
    fn sends_moved(&self, at: Baseline) -> bool {
        let _adoption = self.candidate.lock();
        self.candidate.sends().0 != at.sends.0
    }

    /// Legacy's Codex delivery records for the tmux the log binds now.
    fn records(&self, events: &[BindingEvent]) -> Result<Records, Taken> {
        let tmux = adoption::current(events).map(|(_, tmux)| tmux);
        let delivered = match tmux {
            Some(tmux) => self.io.legacy().codex_delivered(self.channel, &tmux),
            None => Ok(Default::default()),
        };
        Ok(Records::new(delivered.map_err(Taken::Retry)?, events))
    }

    /// Takes `bound` where Legacy's records stop, with every source owed before it.
    async fn warm(
        &self,
        events: &[BindingEvent],
        bound: &(SourceId, u64),
        (at, since): (Baseline, Since),
        (records, sent_from, unresolved): (&Records, &[SourceId], &mut Vec<SourceId>),
    ) -> Result<(), Taken> {
        if self.sends_moved(at) {
            return Err(Taken::Skip(
                "a Legacy body send started since the baseline".into(),
            ));
        }
        let start = match self.start_on(bound, (at, since), (records, sent_from)) {
            Ok(Start::Owed(start)) => {
                remember(unresolved, &bound.0);
                start
            }
            Ok(Start::End(start)) => {
                unresolved.retain(|judged| *judged != bound.0);
                start
            }
            Ok(Start::Legacy(detail)) => {
                remember(unresolved, &bound.0);
                return Err(Taken::Retry(detail));
            }
            Err(taken) => {
                remember(unresolved, &bound.0);
                return Err(taken);
            }
        };
        let judged = (records, sent_from, unresolved);
        let mut sources = self.owed(events, bound, (at, since), judged)?;
        sources.push(ranged(&bound.0, start)?);
        self.fenced(at, bound, Plan::Warm(&sources)).await
    }

    /// Where O starts on `source`: at its earliest turn, ended or not, that Legacy's records do not
    /// show delivered, once no Legacy send can be part of it. Turn time and custody prove nothing.
    fn start_on(
        &self,
        (source, seq): &(SourceId, u64),
        (at, since): (Baseline, Since),
        (records, sent_from): (&Records, &[SourceId]),
    ) -> Result<Start, Taken> {
        let frontier = prefix::frontier(source).map_err(retry)?;
        let floor = records.floor(source, &frontier);
        let covered = |end: Option<u64>| end.is_some_and(|end| end <= floor);
        let Some(turn) = frontier.turns.iter().find(|turn| !covered(turn.ended_end)) else {
            return Ok(Start::End(frontier.complete));
        };
        if turn.ended_at.is_none() && sent_from.contains(source) {
            return Ok(Start::Legacy(
                "a turn Legacy sent from is still open".into(),
            ));
        }
        // Sends read before a later bind or before the turn opened cannot hold it; nor can none.
        let opened_after = (turn.opened_at)
            .is_some_and(|opened| since.proves && opened > since.at + BIND_CLOCK_SLACK);
        if *seq > at.seq || at.sends.0 == 0 || opened_after {
            Ok(Start::Owed(turn.opened))
        } else {
            Ok(Start::Legacy(
                "Legacy may have sent from the earliest undelivered turn".into(),
            ))
        }
    }

    /// Every other source the current tmux generation bound, and every one judged before, from
    /// its earliest turn owed to O; one Legacy may have sent part of retries the takeover.
    fn owed(
        &self,
        events: &[BindingEvent],
        bound: &(SourceId, u64),
        (at, since): (Baseline, Since),
        (records, sent_from, unresolved): (&Records, &[SourceId], &mut Vec<SourceId>),
    ) -> Result<Vec<InitSource>, Taken> {
        let mut walked: Vec<SourceId> = unresolved.clone();
        walked.retain(|source| !records.generation().contains(source));
        walked.extend(records.generation());
        self.owed_among(
            walked,
            events,
            bound,
            (at, since),
            (records, sent_from, unresolved),
        )
    }

    /// `owed` for an admitted execution: its launch starts a new generation before the bind, so
    /// only rollouts this host still holds unresolved stay owed, not ones judged delivered before.
    fn owed_unresolved(
        &self,
        events: &[BindingEvent],
        bound: &(SourceId, u64),
        (at, since): (Baseline, Since),
        (records, sent_from, unresolved): (&Records, &[SourceId], &mut Vec<SourceId>),
    ) -> Result<Vec<InitSource>, Taken> {
        let walked = unresolved.clone();
        self.owed_among(
            walked,
            events,
            bound,
            (at, since),
            (records, sent_from, unresolved),
        )
    }

    fn owed_among(
        &self,
        walked: Vec<SourceId>,
        events: &[BindingEvent],
        bound: &(SourceId, u64),
        (at, since): (Baseline, Since),
        (records, sent_from, unresolved): (&Records, &[SourceId], &mut Vec<SourceId>),
    ) -> Result<Vec<InitSource>, Taken> {
        let mut owed = Vec::new();
        for source in walked.into_iter().filter(|source| *source != bound.0) {
            let seq = (events.iter().rev())
                .find(|event| bound_source(event) == Some(&source))
                .map_or(0, |event| event.seq);
            match self.start_on(&(source.clone(), seq), (at, since), (records, sent_from)) {
                Ok(Start::End(_)) => unresolved.retain(|judged| *judged != source),
                Ok(Start::Owed(start)) => {
                    remember(unresolved, &source);
                    owed.push(ranged(&source, start)?);
                }
                Ok(Start::Legacy(detail)) => {
                    remember(unresolved, &source);
                    return Err(Taken::Retry(format!("a rollout bound before: {detail}")));
                }
                Err(taken) => {
                    remember(unresolved, &source);
                    return Err(taken);
                }
            }
        }
        Ok(owed)
    }

    /// A fresh rollout from its first byte under the fence; see `fenced`.
    pub(super) async fn commit(
        &self,
        at: Baseline,
        bound: &(SourceId, u64),
        prefix: &Prefix,
    ) -> Result<(), Taken> {
        self.commit_owed(at, bound, prefix, &[]).await
    }

    /// `commit` with the earlier sources judged owed before this rollout.
    async fn commit_owed(
        &self,
        at: Baseline,
        bound: &(SourceId, u64),
        prefix: &Prefix,
        owed: &[InitSource],
    ) -> Result<(), Taken> {
        let admission = if self.provider == ShadowProvider::Codex {
            let guard = self.candidate.lock();
            let proof = self.candidate.first_proof(&bound.0).map_err(Taken::Skip)?;
            drop(guard);
            proof
        } else {
            None
        };
        if let Some(proof) = &admission {
            if proof.mark.order != at.order || prefix.prompt_digest.as_ref() != Some(&proof.digest)
            {
                return Err(Taken::Skip(
                    "prefix does not name the first admitted prompt".into(),
                ));
            }
            if !self
                .events()?
                .iter()
                .any(|event| event.seq == bound.1 && proof.names_binding(event))
            {
                return Err(Taken::Skip(
                    "binding event does not name the admitted execution".into(),
                ));
            }
            proof.with_current(|| Ok(())).map_err(Taken::Skip)?;
        }
        if matches!(prefix.class, prefix::PrefixClass::Buffered { .. }) && admission.is_none() {
            return Err(Taken::Skip(
                "buffered output has no admitted execution proof".into(),
            ));
        }
        self.fenced_proved(at, bound, Plan::Fresh(prefix, owed), admission.as_ref())
            .await
    }

    /// The intake fence is held from before the final checks until the init is published.
    async fn fenced(
        &self,
        at: Baseline,
        bound: &(SourceId, u64),
        plan: Plan<'_>,
    ) -> Result<(), Taken> {
        self.fenced_proved(at, bound, plan, None).await
    }

    async fn fenced_proved(
        &self,
        at: Baseline,
        bound: &(SourceId, u64),
        plan: Plan<'_>,
        admission: Option<&FirstProof>,
    ) -> Result<(), Taken> {
        let (io, channel) = (self.io, self.channel);
        let Some(expected) = self.gate.current().epoch() else {
            return Err(Taken::Retry(
                "gateway ownership changed before O activation".into(),
            ));
        };
        #[cfg(test)]
        activation::test_hook::run(channel, activation::test_hook::Step::BeforeFence)
            .map_err(Taken::Retry)?;
        let fence_sends = {
            let _adoption = self.candidate.lock();
            self.candidate.sends()
        };
        if fence_sends.0 != at.sends.0 {
            return Err(Taken::Skip(
                "a Legacy body send started since the baseline".into(),
            ));
        }
        let FencedFacts {
            hold,
            facts,
            queued_bodies,
        } = (io.intake_fence(channel, self.provider).await).map_err(Taken::Retry)?;
        let taken = (|| {
            #[cfg(test)]
            activation::test_hook::run(channel, activation::test_hook::Step::AfterFence)
                .map_err(Taken::Retry)?;
            if let Some(detail) = facts.final_blocker() {
                return Err(Taken::Final(detail));
            }
            let mut why = None;
            let sources = || {
                self.recheck(at, fence_sends, bound, plan, admission)
                    .map_err(|taken| {
                        let detail = taken.detail().to_owned();
                        why = Some(taken);
                        detail
                    })
            };
            // Custody reads are not atomic; the fence, the sends and the relay are judged too.
            let local = || {
                let custody = io.local_custody(channel, self.provider)?;
                Ok(custody.blocks_takeover() || io.relaying(channel))
            };
            let result = self.gate.admit(|epoch| {
                (epoch == expected).then(|| {
                    activation::activate_takeover_proved(
                        self.store,
                        channel,
                        (facts, queued_bodies),
                        local,
                        self.candidate,
                        sources,
                        (self.origin, admission),
                    )
                })
            });
            match result.flatten() {
                Some(Ok(())) => Ok(()),
                Some(Err(detail)) => Err(why.unwrap_or(Taken::Retry(detail))),
                None => Err(Taken::Retry(
                    "gateway ownership changed before O activation".into(),
                )),
            }
        })();
        if let Err(error) = hold.release().await {
            tracing::warn!(channel, %error, "[tui_o] intake fence rollback failed");
        }
        taken
    }

    /// Under the adoption lock: no Legacy send since the baseline or the fence, the same bind,
    /// the same eligible bytes of a fresh rollout and the same bytes before every frontier.
    fn recheck(
        &self,
        at: Baseline,
        fence_sends: (u64, u64),
        bound: &(SourceId, u64),
        plan: Plan<'_>,
        admission: Option<&FirstProof>,
    ) -> Result<Vec<InitSource>, Taken> {
        if self.provider == ShadowProvider::Codex
            && self
                .candidate
                .first_proof(&bound.0)
                .map_err(Taken::Skip)?
                .as_ref()
                != admission
        {
            return Err(Taken::Skip(
                "first admission changed during the intake fence".into(),
            ));
        }
        // A send started since the baseline may be this rollout's; one only finished is older.
        let sends = self.candidate.sends();
        if sends.0 != at.sends.0 || sends.0 != fence_sends.0 {
            return Err(Taken::Skip(
                "a Legacy body send started while O fenced the takeover".into(),
            ));
        }
        if sends != fence_sends {
            return Err(Taken::Retry(
                "a Legacy body send finished while O fenced the takeover".into(),
            ));
        }
        let events = if admission.is_some() {
            self.log
                .binding_history(self.channel)
                .map(renumbered::first_named)
                .map_err(Taken::Final)?
        } else {
            self.events()?
        };
        if admission.is_some_and(|proof| {
            !events
                .iter()
                .any(|event| event.seq == bound.1 && proof.names_binding(event))
        }) {
            return Err(Taken::Skip(
                "binding execution changed during the intake fence".into(),
            ));
        }
        if bound_after(&events, 0).as_ref() != Some(bound) {
            return Err(Taken::Retry(
                "the binding moved while O fenced the takeover".into(),
            ));
        }
        let (checked, fresh) = match plan {
            Plan::Fresh(prefix, owed) => (owed, Some(prefix)),
            Plan::Warm(sources) => (sources, None),
        };
        if let Some(prefix) = fresh {
            if admission.is_some() {
                prefix
                    .preserved_in(&bound.0)
                    .map_err(|verdict| match verdict {
                        Verdict::Wait(detail) => Taken::Retry(detail),
                        Verdict::Refuse(detail) => Taken::Skip(detail),
                        Verdict::Eligible(_) => unreachable!(),
                    })?;
            }
            let verdict = if admission.is_some() {
                prefix::buffered(&bound.0)
            } else {
                (self.judge)(&bound.0)
            };
            match verdict {
                Verdict::Eligible(now) if now == *prefix => {}
                Verdict::Eligible(_) => {
                    return Err(Taken::Again(
                        "the rollout grew while O fenced the takeover".into(),
                    ));
                }
                Verdict::Wait(detail) => return Err(Taken::Retry(detail)),
                // A Codex rollout that gained output is judged again at its frontier.
                Verdict::Refuse(detail)
                    if self.provider == ShadowProvider::Codex && admission.is_none() =>
                {
                    return Err(Taken::Again(detail));
                }
                Verdict::Refuse(detail) => return Err(Taken::Skip(detail)),
            }
        }
        for source in checked {
            let now = prefix::hash_before(&source.source_id, source.delivery_start);
            if now.map_err(retry)? != source.prefix_hash {
                return Err(Taken::Retry(
                    "the bytes before a frontier changed while O fenced the takeover".into(),
                ));
            }
        }
        let mut sources = checked.to_vec();
        if fresh.is_some() {
            sources.push(InitSource {
                source_id: bound.0.clone(),
                delivery_start: 0,
                prefix_hash: hex::encode(Sha256::digest(b"")),
            });
        }
        Ok(sources)
    }
}

/// A frontier read that does not pass now is judged again on the next wake.
fn retry(verdict: Verdict) -> Taken {
    match verdict {
        Verdict::Wait(detail) | Verdict::Refuse(detail) => Taken::Retry(detail),
        Verdict::Eligible(_) => Taken::Retry("the rollout was judged eligible".into()),
    }
}

/// `source` from `start`, over the hash of the bytes before it.
fn ranged(source: &SourceId, start: u64) -> Result<InitSource, Taken> {
    Ok(InitSource {
        source_id: source.clone(),
        delivery_start: start,
        prefix_hash: prefix::hash_before(source, start).map_err(retry)?,
    })
}

/// Keeps `source` among the judged rollouts.
fn remember(unresolved: &mut Vec<SourceId>, source: &SourceId) {
    if !unresolved.contains(source) {
        unresolved.push(source.clone());
    }
}

/// The source the log binds now with the seq that bound it, when that seq is above `after`.
pub(in crate::services::tui_o::writer) fn bound_after(
    events: &[BindingEvent],
    after: u64,
) -> Option<(SourceId, u64)> {
    let (source, _) = adoption::current(events)?;
    let seq = events.iter().rev().find_map(|event| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(bound),
            ..
        }
        | BindingRecord::Resolved { source: bound, .. }
            if *bound == source =>
        {
            Some(event.seq)
        }
        _ => None,
    })?;
    (seq > after).then_some((source, seq))
}
