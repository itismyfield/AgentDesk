//! Restores a pane's durable Pending binding after a restart, judged only from the strict log,
//! the spawn-nonce marker, the launch transcript and which transcripts exist.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::services::claude_tui::hook_server::adoption_retry;
use crate::services::tmux_common::with_tmux_source_authority;
use crate::services::tui_prompt_dedupe::binding_context::{
    SpawnNonceMarker, observe_spawn_nonce_marker,
};
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingEvent, BindingTarget, CauseSource, Corrupt, CorruptKind, HookSignal,
    Proposal, SourceId, record_source, records_strict, subscribe_binding_events,
};
use crate::services::tui_prompt_dedupe::{
    TuiRuntimeBinding, pane_registration, register_provider_session, resolve_tmux_session_name,
    runtime_binding_for_tmux_session, runtime_binding_for_tmux_session_under_source_authority,
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
}

/// The pane was bound to a transcript that does not exist yet: only this exact path may bind it,
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
}

impl PendingRestore {
    /// Whether the pane is settled until its log changes; anything else is judged again next poll.
    pub(crate) fn memo(&self) -> bool {
        match self {
            Self::BoundFromLedger { exact_wait, .. } => exact_wait.is_none(),
            Self::BlockedCorrupt(_) | Self::Unavailable(_) => false,
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
                | Self::Unavailable(Unavailable::NotRegistered)
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchTranscript {
    pub session_id: String,
    pub transcript: PathBuf,
}

/// What the caller registered before the restore; hooks keep naming the launch session, so its
/// alias to the pane is as necessary as the binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Registration {
    pub binding: bool,
    pub command_alias: bool,
}

impl Registration {
    fn complete(self) -> bool {
        self.binding && self.command_alias
    }
}

/// Launch A is on disk and B is Pending: register A and its alias, then seed B for adoption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchSeed {
    pub pending_seq: u64,
    pub launch: LaunchTranscript,
    pub payload_session_id: String,
    pub hook: HookSignal,
}

impl LaunchSeed {
    pub(crate) fn outcome(&self, registered: Registration) -> PendingRestore {
        if !registered.complete() {
            return PendingRestore::Unavailable(Unavailable::NotRegistered);
        }
        let pending_seq = self.pending_seq;
        PendingRestore::Seeded { pending_seq }
    }
}

/// Publish exactly `transcript` for `session_id` and map the launch session to the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExactBinding {
    pub pending_seq: u64,
    pub session_id: String,
    pub transcript: PathBuf,
    pub launch_session_id: String,
    exists: bool,
}

impl ExactBinding {
    pub(crate) fn outcome(&self, registered: Registration) -> PendingRestore {
        if !registered.complete() {
            return PendingRestore::Unavailable(Unavailable::NotRegistered);
        }
        // A transcript that vanished after the judgment is waited for like one never written.
        let exists = self.exists && self.transcript.is_file();
        let exact_wait = (!exists).then(|| ExactPathWait {
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
    }
}

/// First step of restoring `tmux_session`; a publish or seed reports its outcome only after the
/// caller hands back what it registered.
pub(crate) fn judge_restore(
    tmux_session: &str,
    records: io::Result<Result<Vec<BindingEvent>, Corrupt>>,
    marker: &SpawnNonceMarker,
    launch: Option<&LaunchTranscript>,
    exists: impl Fn(&Path) -> bool,
) -> RestoreStep {
    use PendingRestore::{BlockedCorrupt, NotEligible as Skip, Unavailable as Down};
    let done = RestoreStep::Finished;
    let records = match records {
        Err(error) => return done(Down(Unavailable::LogRead(error.kind()))),
        Ok(Err(corrupt)) => return done(BlockedCorrupt(corrupt)),
        Ok(Ok(records)) => records,
    };
    let live = match fold(tmux_session, &records) {
        Fold::Empty => return done(PendingRestore::HealthyNoPending),
        Fold::Superseded => return done(Skip(NotEligible::Superseded)),
        Fold::Live(live) => live,
    };
    if live.rejected && live.resolved.is_none() {
        return done(Skip(NotEligible::Rejected));
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
    // The session's transcript sits next to the launch one, the rule continuation adoption uses.
    let transcript = uuid::Uuid::parse_str(live.session)
        .ok()
        .and_then(|_| launch.transcript.parent())
        .map(|dir| dir.join(format!("{}.jsonl", live.session)));
    let Some(transcript) = transcript.filter(|t| live.path.map(Path::new) == Some(t.as_path()))
    else {
        return mismatch(live.pending.seq);
    };
    let exact = |exists| ExactBinding {
        pending_seq: live.pending.seq,
        session_id: live.session.to_owned(),
        transcript: transcript.clone(),
        launch_session_id: launch.session_id.clone(),
        exists,
    };
    if let Some((record, source)) = live.resolved {
        if source.path != transcript {
            return mismatch(record.seq);
        }
        return RestoreStep::PublishExact(exact(exists(&transcript)));
    }
    if live.session != launch.session_id && exists(&launch.transcript) {
        return RestoreStep::SeedAfterLaunch(LaunchSeed {
            pending_seq: live.pending.seq,
            launch: launch.clone(),
            payload_session_id: live.session.to_owned(),
            hook: restored_hook(live.pending, live.path),
        });
    }
    RestoreStep::PublishExact(exact(exists(&transcript)))
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

/// The bound transcript's mtime for the newer-candidate check; `Some(None)` for a binding this
/// incarnation's restore bound before its file exists, which any existing candidate follows.
pub(crate) fn bound_transcript_mtime(
    tmux_session: &str,
    binding: &TuiRuntimeBinding,
) -> Option<Option<SystemTime>> {
    let this_incarnation = || {
        let memo = outcomes()
            .get(tmux_session)
            .and_then(|((_, n, _), _)| n.clone());
        let current = observe_spawn_nonce_marker(tmux_session);
        matches!(current, SpawnNonceMarker::Known(n) if memo.as_deref() == Some(n.as_str()))
    };
    match std::fs::metadata(&binding.output_path).and_then(|m| m.modified()) {
        Ok(mtime) => Some(Some(mtime)),
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                && awaits_exact_path(tmux_session, binding)
                && this_incarnation() =>
        {
            Some(None)
        }
        Err(_) => None,
    }
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

fn restore_pane(
    tmux_session: &str,
    channel_id: u64,
    launch: Option<LaunchTranscript>,
    bind: impl Fn(&str, &Path) -> TuiRuntimeBinding,
) -> Option<PendingRestore> {
    let bound = runtime_binding_for_tmux_session(tmux_session).is_some();
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
    let records = records_strict(channel_id);
    let register = |session: &str, transcript: &Path, launch_session: &str| {
        let target = |b: &TuiRuntimeBinding| {
            b.session_id.as_deref() == Some(session) && Path::new(&b.output_path) == transcript
        };
        // A held binding keeps its read offsets and only gets its Resolved logged; any other is replaced.
        let held = with_tmux_source_authority(tmux_session, |authority| {
            let held = runtime_binding_for_tmux_session_under_source_authority(authority)?;
            let cause = CauseSource::Observed;
            let proposal =
                Proposal::for_binding(Some(channel_id), tmux_session, &held, None, cause);
            target(&held).then(|| proposal.as_ref().map_or(Ok(()), record_source).is_ok())
        });
        if held.is_none() {
            let binding = bind(session, transcript);
            pane_registration::register_claude_pane(tmux_session, channel_id, binding);
        }
        let binding = held.unwrap_or_else(|| {
            runtime_binding_for_tmux_session(tmux_session).is_some_and(|b| target(&b))
        });
        let alias = || resolve_tmux_session_name("claude", launch_session);
        if binding && alias().is_none() {
            register_provider_session("claude", launch_session, tmux_session);
        }
        let command_alias = alias().as_deref() == Some(tmux_session);
        Registration {
            binding,
            command_alias,
        }
    };
    let outcome = match judge_restore(
        tmux_session,
        records,
        &marker,
        launch.as_ref(),
        Path::is_file,
    ) {
        RestoreStep::Finished(outcome) => outcome,
        RestoreStep::SeedAfterLaunch(seed) => {
            let launch = &seed.launch;
            let registered = register(&launch.session_id, &launch.transcript, &launch.session_id);
            let outcome = seed.outcome(registered);
            if matches!(outcome, PendingRestore::Seeded { .. }) {
                let (command, payload) = (&launch.session_id, &seed.payload_session_id);
                adoption_retry::seed_restored(tmux_session, command, payload, &seed.hook);
            }
            outcome
        }
        RestoreStep::PublishExact(exact) => {
            let launch_session = &exact.launch_session_id;
            exact.outcome(register(
                &exact.session_id,
                &exact.transcript,
                launch_session,
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

#[cfg(all(test, unix))]
#[path = "pending_tests.rs"]
mod tests;
