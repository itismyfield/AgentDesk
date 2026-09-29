//! Whether a pane's durable Pending binding comes back after a restart, judged only from the
//! strict log, the spawn-nonce marker, the launch transcript and which transcripts exist.
#![cfg_attr(not(test), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

use crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, BindingEvent, BindingTarget, Corrupt, CorruptKind, HookSignal, SourceId,
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
        let exact_wait = (!self.exists).then(|| ExactPathWait {
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

#[cfg(all(test, unix))]
#[path = "pending_tests.rs"]
mod tests;
