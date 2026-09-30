//! Typed verdict a consumer takes before its first state change on a session. Unknown,
//! Conflict, a failed probe or Herdr never admit an automatic clear, finalize or kill.
#![cfg_attr(not(test), allow(dead_code))]

use super::model::{HostKind, HostLiveness, HostSessionRef};
use super::resolve::{ResolvedSessionTarget, TargetHost};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutomaticEffect {
    Clear,
    Finalize,
    FailDispatch,
    Recreate,
    Kill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateChange {
    /// Turn completion backed by a delivered transcript; host evidence never blocks it.
    TranscriptCompletion,
    /// Repair inferred from the host, with the probe result if one ran.
    Automatic {
        effect: AutomaticEffect,
        observed: Option<HostLiveness>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardRefusal {
    UnknownHost,
    HostConflict,
    ProbeFailed,
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardVerdict {
    /// The consumer's existing legacy decision may run.
    Proceed,
    /// A Herdr session: no automatic state change; its existing recovery owns it.
    DeferredToExistingRecovery,
    Refused(GuardRefusal),
}

pub(crate) fn guard_first_state_change(
    target: &ResolvedSessionTarget,
    change: StateChange,
) -> GuardVerdict {
    let StateChange::Automatic { observed, .. } = change else {
        return GuardVerdict::Proceed;
    };
    match &target.host {
        TargetHost::Unknown(_) => GuardVerdict::Refused(GuardRefusal::UnknownHost),
        TargetHost::Conflict { .. } => GuardVerdict::Refused(GuardRefusal::HostConflict),
        TargetHost::Known {
            kind: HostKind::Herdr,
            ..
        } => GuardVerdict::DeferredToExistingRecovery,
        TargetHost::Known { .. } if observed == Some(HostLiveness::ProbeError) => {
            GuardVerdict::Refused(GuardRefusal::ProbeFailed)
        }
        TargetHost::Known { .. } => GuardVerdict::Proceed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferReason {
    Herdr,
    UnknownHost,
    HostConflict,
}

/// Answer of the tmux-compatible liveness probe policies read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolicyProbe {
    Probed(HostLiveness),
    /// No probe ran and the whole row is left alone, inflight or not.
    DeferredToExistingRecovery(DeferReason),
}

impl PolicyProbe {
    /// The `hasLivePane` string; a deferred row reads `unknown`, never `dead`.
    pub(crate) fn js_state(self) -> &'static str {
        match self {
            Self::Probed(HostLiveness::Live) => "live",
            Self::Probed(HostLiveness::DeadOrAbsent) => "dead",
            Self::Probed(HostLiveness::ProbeError) | Self::DeferredToExistingRecovery(_) => {
                "unknown"
            }
        }
    }
}

/// Runs `probe` only on a known tmux/process target.
pub(crate) fn probe_for_policy(
    target: &ResolvedSessionTarget,
    probe: impl FnOnce(HostSessionRef<'_>) -> HostLiveness,
) -> PolicyProbe {
    if let Some(session) = target.legacy_ref() {
        return PolicyProbe::Probed(probe(session));
    }
    PolicyProbe::DeferredToExistingRecovery(match &target.host {
        TargetHost::Conflict { .. } => DeferReason::HostConflict,
        TargetHost::Known {
            kind: HostKind::Herdr,
            ..
        } => DeferReason::Herdr,
        _ => DeferReason::UnknownHost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::session_host::resolve::{SessionTargetInput, TargetSource, UnknownHost};

    const EFFECTS: [AutomaticEffect; 5] = [
        AutomaticEffect::Clear,
        AutomaticEffect::Finalize,
        AutomaticEffect::FailDispatch,
        AutomaticEffect::Recreate,
        AutomaticEffect::Kill,
    ];

    fn target(host: TargetHost) -> ResolvedSessionTarget {
        ResolvedSessionTarget {
            input: SessionTargetInput::SessionKey("claude/h/mac-mini:AgentDesk-claude-x".into()),
            session_key: None,
            host,
        }
    }

    fn known(kind: HostKind, name: &str) -> TargetHost {
        TargetHost::Known {
            kind,
            source: TargetSource::SessionRecord,
            name: name.to_string(),
        }
    }

    fn unknown() -> TargetHost {
        TargetHost::Unknown(UnknownHost::NoHostEvidence)
    }

    fn conflict() -> TargetHost {
        TargetHost::Conflict {
            first: (HostKind::Herdr, TargetSource::SessionRecord),
            second: (HostKind::Tmux, TargetSource::InflightLocator),
        }
    }

    #[test]
    fn automatic_state_change_verdict_for_every_host_state() {
        use HostLiveness::{DeadOrAbsent, Live, ProbeError};
        for effect in EFFECTS {
            for observed in [None, Some(Live), Some(DeadOrAbsent), Some(ProbeError)] {
                let change = StateChange::Automatic { effect, observed };
                let verdict = |host| guard_first_state_change(&target(host), change);
                let legacy = match observed {
                    Some(ProbeError) => GuardVerdict::Refused(GuardRefusal::ProbeFailed),
                    _ => GuardVerdict::Proceed,
                };
                let label = format!("{effect:?} after {observed:?}");
                assert_eq!(verdict(known(HostKind::Tmux, "t")), legacy, "tmux {label}");
                assert_eq!(
                    verdict(known(HostKind::Process, "p")),
                    legacy,
                    "process {label}"
                );
                assert_eq!(
                    verdict(known(HostKind::Herdr, "w1-1")),
                    GuardVerdict::DeferredToExistingRecovery,
                    "herdr {label}: no automatic state change"
                );
                assert_eq!(
                    verdict(unknown()),
                    GuardVerdict::Refused(GuardRefusal::UnknownHost),
                    "unknown {label}"
                );
                assert_eq!(
                    verdict(conflict()),
                    GuardVerdict::Refused(GuardRefusal::HostConflict),
                    "conflict {label}"
                );
            }
        }
    }

    #[test]
    fn transcript_completion_is_never_blocked_by_host_evidence() {
        for host in [
            known(HostKind::Tmux, "t"),
            known(HostKind::Process, "p"),
            known(HostKind::Herdr, "w1-1"),
            unknown(),
            conflict(),
        ] {
            assert_eq!(
                guard_first_state_change(&target(host.clone()), StateChange::TranscriptCompletion),
                GuardVerdict::Proceed,
                "{host:?}"
            );
        }
    }

    #[test]
    fn policy_probe_defers_every_non_legacy_host_without_probing() {
        for (host, reason) in [
            (known(HostKind::Herdr, "w1-1"), DeferReason::Herdr),
            (unknown(), DeferReason::UnknownHost),
            (conflict(), DeferReason::HostConflict),
        ] {
            let mut probes = 0;
            let answer = probe_for_policy(&target(host), |_| {
                probes += 1;
                HostLiveness::DeadOrAbsent
            });
            assert_eq!(
                answer,
                PolicyProbe::DeferredToExistingRecovery(reason),
                "{reason:?} must defer, never read as dead"
            );
            assert_eq!(answer.js_state(), "unknown");
            assert_eq!(probes, 0, "{reason:?}: no tmux probe may run");
        }

        for (kind, name) in [
            (HostKind::Tmux, "AgentDesk-claude-adk:cc"),
            (HostKind::Process, "proc-1"),
        ] {
            for (liveness, js) in [
                (HostLiveness::Live, "live"),
                (HostLiveness::DeadOrAbsent, "dead"),
                (HostLiveness::ProbeError, "unknown"),
            ] {
                let mut seen = Vec::new();
                let answer = probe_for_policy(&target(known(kind, name)), |session| {
                    seen.push((session.kind, session.name.to_string()));
                    liveness
                });
                assert_eq!(answer, PolicyProbe::Probed(liveness));
                assert_eq!(answer.js_state(), js);
                assert_eq!(
                    seen,
                    [(kind, name.to_string())],
                    "one probe on the resolved ref"
                );
            }
        }
    }
}
