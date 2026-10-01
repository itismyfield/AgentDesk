//! Restores a pane's durable Pending, or its verified current source, after a restart, judged only
//! from the strict log, the spawn-nonce marker, the launch transcript and what the transcript holds.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::services::claude_tui::hook_server::adoption_retry;
use crate::services::claude_tui::source_verify::{
    Observation, OpenedTranscript, PinJudgment, is_top_level_transcript, judge_pin,
    observe_transcript,
};
use crate::services::tmux_common::{TmuxSourceAuthority, with_tmux_source_authority};
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingEvent, BindingTarget, CauseSource, Corrupt, CorruptKind, HookSignal,
    Proposal, SourceId, pinned_source, records_strict, subscribe_binding_events,
};
use crate::services::tui_prompt_dedupe::{
    Persisted, Record, TuiRuntimeBinding, pane_registration, register_provider_session,
    resolve_tmux_session_name, runtime_binding_for_tmux_session,
    runtime_binding_for_tmux_session_under_source_authority,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotEligible {
    NoNonceMarker,
    LegacyNonceNone,
    NonceMismatch,
    Superseded,
    /// The live system refused this Pending after it was recorded; a restart must not undo that.
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    LogRead(io::ErrorKind),
    NonceMarkerUnreadable,
    LaunchUnreadable,
    /// The binding or the launch-session alias the restore depends on was not registered.
    NotRegistered,
    /// The pinned transcript could not be read; the pane keeps what it has and is judged again.
    TranscriptUnreadable,
}

/// The pane was bound to a transcript not verified yet: only this exact path may bind it,
/// never the newest file of the same directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExactPathWait {
    pub session_id: String,
    pub transcript: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingRestore {
    HealthyNoPending,
    Seeded {
        pending_seq: u64,
    },
    BoundFromLedger {
        pending_seq: u64,
        exact_wait: Option<ExactPathWait>,
    },
    NotEligible(NotEligible),
    BlockedCorrupt(Corrupt),
    Unavailable(Unavailable),
    /// The file a verified record `line` pinned was replaced or is gone; nothing is bound over it.
    Anomaly {
        line: u64,
    },
}

impl PendingRestore {
    /// Whether the pane is settled until its log changes; anything else is judged again next poll.
    pub(crate) fn memo(&self) -> bool {
        match self {
            Self::BoundFromLedger { exact_wait, .. } => exact_wait.is_none(),
            Self::BlockedCorrupt(_) | Self::Unavailable(_) | Self::Anomaly { .. } => false,
            Self::HealthyNoPending | Self::Seeded { .. } | Self::NotEligible(_) => true,
        }
    }
}

impl PendingRestore {
    /// The rehydrate pass must not register the launch binding over this outcome: the restore
    /// registered the pane, found the log corrupt, or failed to register what the log names.
    pub(crate) fn skips_launch_refresh(&self) -> bool {
        matches!(
            self,
            Self::Seeded { .. }
                | Self::BoundFromLedger { .. }
                | Self::BlockedCorrupt(_)
                | Self::Anomaly { .. }
                | Self::Unavailable(Unavailable::NotRegistered | Unavailable::TranscriptUnreadable)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchTranscript {
    pub session_id: String,
    pub transcript: PathBuf,
}

/// What the caller registered before the restore; hooks keep naming the launch session, so its
/// alias to the pane is as necessary as the binding. `persisted` is what its record left in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Registration {
    pub binding: bool,
    pub command_alias: bool,
    pub persisted: Option<Persisted>,
}

impl Registration {
    fn complete(self) -> bool {
        self.binding && self.command_alias
    }

    /// The outcome when the log's pin refused the record; `line` is the record the pin came from.
    fn refused(self, line: u64) -> Option<PendingRestore> {
        match self.persisted? {
            Persisted::Anomaly => Some(PendingRestore::Anomaly { line }),
            Persisted::Recheck => Some(PendingRestore::Unavailable(
                Unavailable::TranscriptUnreadable,
            )),
            Persisted::Logged | Persisted::AwaitingExact => None,
        }
    }
}

/// B is Pending behind the pane's verified current, or else launch A on disk: register that source
/// and the launch alias, then seed B for adoption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchSeed {
    pub pending_seq: u64,
    /// The record a refusal of the registration names.
    pub line: u64,
    /// The verified current bound instead of the launch transcript, already judged against its pin.
    pub pinned: Option<SourceId>,
    pub launch: LaunchTranscript,
    pub payload_session_id: String,
    pub hook: HookSignal,
}

impl LaunchSeed {
    pub(crate) fn outcome(&self, registered: Registration) -> PendingRestore {
        if let Some(refused) = registered.refused(self.line) {
            return refused;
        }
        if !registered.complete() {
            return PendingRestore::Unavailable(Unavailable::NotRegistered);
        }
        let pending_seq = self.pending_seq;
        PendingRestore::Seeded { pending_seq }
    }
}

/// Publish exactly `transcript` for `session_id` and map the launch session to the pane.
/// `pending_seq` is the Pending, or the verified source record, the binding is restored from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExactBinding {
    pub pending_seq: u64,
    /// The record a refusal of the registration names.
    pub line: u64,
    pub session_id: String,
    pub transcript: PathBuf,
    pub launch_session_id: String,
    /// Set when the transcript passed the source check; otherwise the pane waits for it.
    verified: Option<SourceId>,
}

impl ExactBinding {
    pub(crate) fn outcome(&self, registered: Registration) -> PendingRestore {
        if let Some(refused) = registered.refused(self.line) {
            return refused;
        }
        if !registered.complete() {
            return PendingRestore::Unavailable(Unavailable::NotRegistered);
        }
        // Only a verified identity the registration logged settles the pane; a transcript replaced
        // or gone after the judgment is waited for like one never written.
        let settled = self.verified.is_some() && registered.persisted == Some(Persisted::Logged);
        let exact_wait = (!settled).then(|| ExactPathWait {
            session_id: self.session_id.clone(),
            transcript: self.transcript.clone(),
        });
        let pending_seq = self.pending_seq;
        PendingRestore::BoundFromLedger {
            pending_seq,
            exact_wait,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestoreStep {
    Finished(PendingRestore),
    SeedAfterLaunch(LaunchSeed),
    PublishExact(ExactBinding),
}

struct Candidate<'a> {
    pending: &'a BindingEvent,
    session: &'a str,
    path: Option<&'a str>,
    resolved: Option<(&'a BindingEvent, &'a SourceId)>,
    rejected: bool,
}

enum Fold<'a> {
    Empty,
    Superseded,
    Live(Candidate<'a>),
}

/// The pane's latest Pending (marked when later refused), or the Resolved that completed it,
/// unless a later record moved on.
fn fold<'a>(tmux_session: &str, records: &'a [BindingEvent]) -> Fold<'a> {
    let pane = records
        .iter()
        .filter(|r| r.tmux_session == tmux_session && r.provider == "claude");
    let mut state = Fold::Empty;
    for record in pane {
        state = match (&record.new, state) {
            (
                BindingTarget::Pending {
                    payload_session_id,
                    payload_transcript_path,
                },
                _,
            ) => Fold::Live(Candidate {
                pending: record,
                session: payload_session_id,
                path: payload_transcript_path.as_deref(),
                resolved: None,
                rejected: false,
            }),
            (
                BindingTarget::Resolved {
                    pending_seq,
                    source,
                },
                Fold::Live(live),
            ) if live.resolved.is_none()
                && live.pending.seq == *pending_seq
                && source.session_id == live.session =>
            {
                let resolved = Some((record, source));
                Fold::Live(Candidate { resolved, ..live })
            }
            (BindingTarget::Resolved { .. }, _) => Fold::Superseded,
            (
                BindingTarget::Rejected {
                    payload_session_id, ..
                },
                Fold::Live(live),
            ) if live.resolved.is_none() && *payload_session_id == live.session => {
                Fold::Live(Candidate {
                    rejected: true,
                    ..live
                })
            }
            (BindingTarget::Source(source), Fold::Live(live))
                if moved_on(record, source, &live) =>
            {
                Fold::Superseded
            }
            (_, state) => state,
        };
    }
    state
}

/// The pane's latest record naming its bound source, a Source or a Resolved.
fn current_record<'a>(
    tmux_session: &str,
    records: &'a [BindingEvent],
) -> Option<(&'a BindingEvent, &'a SourceId)> {
    let mut pane = records
        .iter()
        .rev()
        .filter(|r| r.tmux_session == tmux_session && r.provider == "claude");
    pane.find_map(|record| match &record.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
            Some((record, source))
        }
        _ => None,
    })
}

/// Binds exactly `session`'s `path` as the pane's pin judges what the path holds now: a pinned
/// file only while it is the same, an unpinned one verified or waited for, another session's never.
fn judged(
    pin: Option<&SourceId>,
    (session, path): (&str, &Path),
    (pending_seq, line): (u64, u64),
    launch: &LaunchTranscript,
    observe: &impl Fn(&Path) -> io::Result<OpenedTranscript>,
) -> RestoreStep {
    let opened = observe(path);
    let verified = match judge_pin(pin, session, path, Observation::Opened(opened.as_ref())) {
        PinJudgment::Verified(source) => Some(source),
        PinJudgment::Pending | PinJudgment::Unpinned => None,
        PinJudgment::Recheck => {
            let why = Unavailable::TranscriptUnreadable;
            return RestoreStep::Finished(PendingRestore::Unavailable(why));
        }
        PinJudgment::Anomaly => return RestoreStep::Finished(PendingRestore::Anomaly { line }),
        PinJudgment::Foreign | PinJudgment::Unidentified => {
            let kind = CorruptKind::PathMismatch;
            return RestoreStep::Finished(PendingRestore::BlockedCorrupt(Corrupt { line, kind }));
        }
    };
    RestoreStep::PublishExact(ExactBinding {
        pending_seq,
        line,
        session_id: session.to_owned(),
        transcript: path.to_path_buf(),
        launch_session_id: launch.session_id.clone(),
        verified,
    })
}

/// Whether `path` is a top-level transcript of `session` under the launch's projects root.
fn under_launch_root(launch: &LaunchTranscript, path: &Path, session: &str) -> bool {
    let root = launch.transcript.parent().and_then(Path::parent);
    uuid::Uuid::parse_str(session).is_ok()
        && root.is_some_and(|root| is_top_level_transcript(root, path, session))
}

/// A hook naming another session replaces a Pending; any other source replaces a Resolved.
fn moved_on(record: &BindingEvent, source: &SourceId, live: &Candidate) -> bool {
    match live.resolved {
        None => record.evidence.hook_event.is_some() && source.session_id != live.session,
        Some((_, bound)) => source.session_id != bound.session_id || source.path != bound.path,
    }
}

/// The hook that produced `pending`, rebuilt so a seeded adoption keeps its recorded cause.
fn restored_hook(pending: &BindingEvent, path: Option<&str>) -> HookSignal {
    let source = match pending.cause {
        BindingCause::Startup => Some("startup"),
        BindingCause::Resume => Some("resume"),
        BindingCause::Clear => Some("clear"),
        BindingCause::Compact => Some("compact"),
        BindingCause::Fork => Some("fork"),
        BindingCause::Continuation | BindingCause::Unknown => None,
    };
    HookSignal {
        event: pending.evidence.hook_event.clone().unwrap_or_default(),
        source: source.map(str::to_owned),
        transcript_path: path.map(str::to_owned),
        received_at: pending.evidence.received_at,
        published_at: None,
    }
}

/// First step of restoring `tmux_session`; a publish or seed reports its outcome only after the
/// caller hands back what it registered. `pinned` is the writer's verified current source.
pub(crate) fn judge_restore(
    tmux_session: &str,
    records: io::Result<Result<Vec<BindingEvent>, Corrupt>>,
    pinned: Option<&SourceId>,
    marker: &SpawnNonceMarker,
    launch: Option<&LaunchTranscript>,
    launch_exists: impl Fn(&Path) -> bool,
    observe: impl Fn(&Path) -> io::Result<OpenedTranscript>,
) -> RestoreStep {
    use PendingRestore::{BlockedCorrupt, NotEligible as Skip, Unavailable as Down};
    let done = RestoreStep::Finished;
    let records = match records {
        Err(error) => return done(Down(Unavailable::LogRead(error.kind()))),
        Ok(Err(corrupt)) => return done(BlockedCorrupt(corrupt)),
        Ok(Ok(records)) => records,
    };
    // The verified current, when these records still end on the source the writer pinned.
    let verified = current_record(tmux_session, &records).filter(|(_, s)| pinned == Some(*s));
    // With no Pending to restore, the verified current is restored on its exact path, but only in
    // this execution; anything else keeps the outcome the log alone gives.
    let known = match marker {
        SpawnNonceMarker::Known(nonce) => Some(nonce.as_str()),
        _ => None,
    };
    let restorable = verified.zip(launch).filter(|((record, source), launch)| {
        known.is_some()
            && record.execution_nonce.as_deref() == known
            && under_launch_root(launch, &source.path, &source.session_id)
    });
    let bind_current =
        |((record, pin), launch): ((&BindingEvent, &SourceId), &LaunchTranscript)| {
            let target = (pin.session_id.as_str(), pin.path.as_path());
            judged(pinned, target, (record.seq, record.seq), launch, &observe)
        };
    let current_or = |otherwise| restorable.map_or(done(otherwise), bind_current);
    let live = match fold(tmux_session, &records) {
        Fold::Live(live) => live,
        Fold::Empty => return current_or(PendingRestore::HealthyNoPending),
        Fold::Superseded => return current_or(Skip(NotEligible::Superseded)),
    };
    if live.rejected && live.resolved.is_none() {
        return current_or(Skip(NotEligible::Rejected));
    }
    let resolved = live.resolved.map(|(record, _)| record);
    let nonces: Vec<_> = std::iter::once(live.pending)
        .chain(resolved)
        .map(|record| record.execution_nonce.as_deref())
        .collect();
    if nonces.iter().any(Option::is_none) {
        return done(Skip(NotEligible::LegacyNonceNone));
    }
    let current = match marker {
        SpawnNonceMarker::Known(nonce) => nonce.as_str(),
        SpawnNonceMarker::Absent => return done(Skip(NotEligible::NoNonceMarker)),
        SpawnNonceMarker::Unreadable => return done(Down(Unavailable::NonceMarkerUnreadable)),
    };
    if nonces.iter().any(|nonce| *nonce != Some(current)) {
        return done(Skip(NotEligible::NonceMismatch));
    }
    let Some(launch) = launch else {
        return done(Down(Unavailable::LaunchUnreadable));
    };
    let mismatch = |line| {
        let kind = CorruptKind::PathMismatch;
        done(BlockedCorrupt(Corrupt { line, kind }))
    };
    // The Pending names a top-level transcript under the launch's projects root, as hook checks do.
    let transcript = live
        .path
        .map(PathBuf::from)
        .filter(|path| under_launch_root(launch, path, live.session));
    let Some(transcript) = transcript else {
        return mismatch(live.pending.seq);
    };
    let exact = |line| {
        let target = (live.session, transcript.as_path());
        judged(pinned, target, (live.pending.seq, line), launch, &observe)
    };
    if let Some((record, source)) = live.resolved {
        if source.path != transcript {
            return mismatch(record.seq);
        }
        return exact(record.seq);
    }
    let seed = |line, pinned| LaunchSeed {
        pending_seq: live.pending.seq,
        line,
        pinned,
        launch: launch.clone(),
        payload_session_id: live.session.to_owned(),
        hook: restored_hook(live.pending, live.path),
    };
    // The verified current is bound as its pin admits it, never re-registered by a stat, and the
    // Pending is seeded behind it.
    if let Some(bound) = restorable.filter(|((_, pin), _)| pin.session_id != live.session) {
        return match bind_current(bound) {
            RestoreStep::PublishExact(exact) => {
                RestoreStep::SeedAfterLaunch(seed(exact.line, exact.verified))
            }
            refused => refused,
        };
    }
    if live.session != launch.session_id && launch_exists(&launch.transcript) {
        let line = verified.map_or(live.pending.seq, |(record, _)| record.seq);
        return RestoreStep::SeedAfterLaunch(seed(line, None));
    }
    exact(live.pending.seq)
}

/// `(channel, spawn nonce, last committed seq)`: a memoized outcome holds until one changes.
type MemoKey = (u64, Option<String>, Option<u64>);

static OUTCOMES: LazyLock<Mutex<HashMap<String, (MemoKey, PendingRestore)>>> =
    LazyLock::new(Default::default);

fn outcomes() -> MutexGuard<'static, HashMap<String, (MemoKey, PendingRestore)>> {
    OUTCOMES.lock().unwrap_or_else(|p| p.into_inner())
}

/// The latest restore outcome of `tmux_session` in this process.
pub(crate) fn last_restore_outcome(tmux_session: &str) -> Option<PendingRestore> {
    outcomes()
        .get(tmux_session)
        .map(|(_, outcome)| outcome.clone())
}

/// Whether `binding` is a transcript a restore bound before it existed; nothing may stand in.
pub(crate) fn awaits_exact_path(tmux_session: &str, binding: &TuiRuntimeBinding) -> bool {
    matches!(
        last_restore_outcome(tmux_session),
        Some(PendingRestore::BoundFromLedger { exact_wait: Some(wait), .. })
            if wait.transcript == Path::new(&binding.output_path)
                && binding.session_id.as_deref() == Some(wait.session_id.as_str())
    )
}

/// Restores `tmux_session`'s durable Pending before the rehydrate pass judges its binding; `None`
/// is a bound pane whose restore already settled. `bind` builds a transcript's binding.
pub(crate) fn restore_claude_pane(
    tmux_session: &str,
    channel_id: u64,
    launch: Option<LaunchTranscript>,
    bind: impl Fn(&str, &Path) -> TuiRuntimeBinding,
) -> Option<PendingRestore> {
    let restored = restore_pane(tmux_session, channel_id, launch, bind);
    keep_channel(tmux_session, channel_id);
    restored
}

/// Reads the log, judges it and registers, seeds and memoizes the result in one hold of the pane's
/// source authority, so a hook adoption lands wholly before the judgment or after its registration.
fn restore_pane(
    tmux_session: &str,
    channel_id: u64,
    launch: Option<LaunchTranscript>,
    bind: impl Fn(&str, &Path) -> TuiRuntimeBinding,
) -> Option<PendingRestore> {
    with_tmux_source_authority(tmux_session, |authority| {
        restore_under_source_authority(authority, channel_id, launch, bind)
    })
}

fn restore_under_source_authority(
    authority: &TmuxSourceAuthority<'_>,
    channel_id: u64,
    launch: Option<LaunchTranscript>,
    bind: impl Fn(&str, &Path) -> TuiRuntimeBinding,
) -> Option<PendingRestore> {
    let tmux_session = authority.session();
    let runtime = || runtime_binding_for_tmux_session_under_source_authority(authority);
    let bound = runtime().is_some();
    let settled = last_restore_outcome(tmux_session).filter(PendingRestore::memo);
    // A binding restored from the log keeps the launch refresh off until the log or nonce moves on.
    let protected = |o: &PendingRestore| matches!(o, PendingRestore::BoundFromLedger { .. });
    if bound && settled.is_some_and(|o| !protected(&o)) {
        return None;
    }
    let marker = observe_spawn_nonce_marker(tmux_session);
    let nonce = match &marker {
        SpawnNonceMarker::Known(nonce) => Some(nonce.clone()),
        _ => None,
    };
    let last_seq = subscribe_binding_events(channel_id)
        .ok()
        .map(|rx| *rx.borrow());
    let key = (channel_id, nonce, last_seq);
    let memoized = outcomes()
        .get(tmux_session)
        .filter(|(memo, outcome)| {
            *memo == key
                && last_seq.is_some()
                && outcome.memo()
                && (bound || !outcome.skips_launch_refresh())
        })
        .map(|(_, outcome)| outcome.clone());
    if let Some(outcome) = memoized {
        return Some(outcome);
    }
    let (records, pinned) = match pinned_source(channel_id, tmux_session) {
        Ok(pinned) => (records_strict(channel_id), pinned),
        Err(error) => (Err(error), None),
    };
    let register = |session: &str, transcript: &Path, launch_session: &str, record: Record| {
        let target = |b: &TuiRuntimeBinding| {
            b.session_id.as_deref() == Some(session) && Path::new(&b.output_path) == transcript
        };
        #[cfg(test)]
        crate::services::tui_prompt_dedupe::after_check();
        // A held binding keeps its read offsets and only gets its record logged; any other is replaced.
        let persisted = match runtime().filter(target) {
            Some(held) => {
                let cause = CauseSource::Observed;
                let proposal =
                    Proposal::for_binding(Some(channel_id), tmux_session, &held, None, cause);
                let persist = |proposal: Proposal| record.persist(&proposal).ok();
                let persisted = proposal.map_or(Some(record.unlogged()), persist);
                // A kept binding completes a registration an earlier pass left unready, as the pass would.
                if persisted.is_some_and(Persisted::published) {
                    let launch = Some(session);
                    pane_registration::note_claude_pane_registered_under_source_authority(
                        authority, launch,
                    );
                }
                persisted
            }
            None => pane_registration::register_claude_pane_under_source_authority(
                authority,
                channel_id,
                bind(session, transcript),
                record,
            ),
        };
        let binding =
            persisted.is_some_and(Persisted::published) && runtime().is_some_and(|b| target(&b));
        let alias = || resolve_tmux_session_name("claude", launch_session);
        if binding && alias().is_none() {
            register_provider_session("claude", launch_session, tmux_session);
        }
        let command_alias = alias().as_deref() == Some(tmux_session);
        Registration {
            binding,
            command_alias,
            persisted,
        }
    };
    let outcome = match judge_restore(
        tmux_session,
        records,
        pinned.as_ref(),
        &marker,
        launch.as_ref(),
        Path::is_file,
        observe_transcript,
    ) {
        RestoreStep::Finished(outcome) => outcome,
        RestoreStep::SeedAfterLaunch(seed) => {
            let launch = &seed.launch;
            let command = &launch.session_id;
            let registered = match &seed.pinned {
                Some(pin) => register(
                    &pin.session_id,
                    &pin.path,
                    command,
                    Record::Verified(pin.clone()),
                ),
                None => register(command, &launch.transcript, command, Record::Stat),
            };
            let outcome = seed.outcome(registered);
            if matches!(outcome, PendingRestore::Seeded { .. }) {
                let (command, payload) = (&launch.session_id, &seed.payload_session_id);
                adoption_retry::seed_restored(authority, command, payload, &seed.hook);
            }
            outcome
        }
        RestoreStep::PublishExact(exact) => {
            let record = exact
                .verified
                .clone()
                .map_or(Record::AwaitFirstRecord, Record::Verified);
            let launch_session = &exact.launch_session_id;
            exact.outcome(register(
                &exact.session_id,
                &exact.transcript,
                launch_session,
                record,
            ))
        }
    };
    note(tmux_session, key, &outcome);
    Some(outcome)
}

/// Every pass that reaches a bound pane renews its channel mapping, whichever way the restore
/// returned, so the mapping's TTL cannot lapse under a live binding the hook still logs through.
fn keep_channel(tmux_session: &str, channel_id: u64) {
    if channel_id != 0 && runtime_binding_for_tmux_session(tmux_session).is_some() {
        let mut state = super::STATE.lock().unwrap_or_else(|p| p.into_inner());
        state.purge_expired();
        let recorded_at = std::time::Instant::now();
        let mapping = super::TimedValue {
            value: channel_id,
            recorded_at,
        };
        state
            .channel_by_tmux
            .insert(tmux_session.to_owned(), mapping);
    }
}

fn note(tmux_session: &str, key: MemoKey, outcome: &PendingRestore) {
    let mut outcomes = outcomes();
    let changed = outcomes.get(tmux_session).is_none_or(|(_, o)| o != outcome);
    match outcome {
        _ if !changed => {}
        PendingRestore::BlockedCorrupt(corrupt) => tracing::error!(
            tmux_session,
            ?corrupt,
            "binding event log is corrupt; the pane keeps its current binding"
        ),
        PendingRestore::Anomaly { line } => tracing::error!(
            tmux_session,
            line,
            "the pinned transcript was replaced; the pane is not bound over it"
        ),
        PendingRestore::Unavailable(why) => {
            tracing::warn!(
                tmux_session,
                ?why,
                "durable Pending restore retried next poll"
            )
        }
        _ => tracing::info!(tmux_session, ?outcome, "durable Pending restore judged"),
    }
    outcomes.insert(tmux_session.to_owned(), (key, outcome.clone()));
}

#[cfg(test)]
pub(crate) fn reset_restore_outcomes_for_tests() {
    outcomes().clear();
}

/// Ages the pane's channel mapping past its TTL while its runtime binding stays fresh.
#[cfg(test)]
pub(crate) fn expire_channel_mapping_for_tests(tmux_session: &str) {
    let mut state = super::STATE.lock().unwrap_or_else(|p| p.into_inner());
    let aged = std::time::Instant::now() - super::SESSION_MAPPING_TTL;
    if let Some(mapping) = state.channel_by_tmux.get_mut(tmux_session) {
        mapping.recorded_at = aged - std::time::Duration::from_secs(1);
    }
}

/// Ages the pane's runtime binding past its TTL; its alias, channel mapping and outcome stay.
#[cfg(test)]
pub(crate) fn expire_runtime_binding_for_tests(tmux_session: &str) {
    let mut state = super::STATE.lock().unwrap_or_else(|p| p.into_inner());
    let aged = std::time::Instant::now() - super::SESSION_MAPPING_TTL;
    if let Some(binding) = state.runtime_by_tmux.get_mut(tmux_session) {
        binding.recorded_at = aged - std::time::Duration::from_secs(1);
    }
}

#[cfg(test)]
pub(crate) const CHANNEL_MAPPING_TTL: std::time::Duration = super::SESSION_MAPPING_TTL;

/// Moves the pane's channel mapping `by` into the past while its runtime binding stays fresh.
#[cfg(test)]
pub(crate) fn age_channel_mapping_for_tests(tmux_session: &str, by: std::time::Duration) {
    let mut state = super::STATE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(mapping) = state.channel_by_tmux.get_mut(tmux_session) {
        mapping.recorded_at = mapping
            .recorded_at
            .checked_sub(by)
            .expect("uptime spans the age");
    }
}

#[cfg(all(test, unix))]
#[path = "pending_tests.rs"]
mod tests;
