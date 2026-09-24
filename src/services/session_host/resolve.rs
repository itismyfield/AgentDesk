use super::model::{HostKind, HostKindResolution, HostKindSource};
use super::process_host::ProcessHost;
use super::tmux_host::TmuxHost;
use super::traits::InteractiveSessionHost;
use crate::services::agent_protocol::RuntimeHandoffKind;

/// Host-kind evidence the caller already read at its own site. The resolver
/// performs no lookups, so each site keeps its probe order and timeouts.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HostEvidence<'a> {
    pub session_name: Option<&'a str>,
    /// Turn-bound `runtime_kind` (inflight row or cancel token).
    pub durable_runtime_kind: Option<RuntimeHandoffKind>,
    /// `tmux_common::resolve_tmux_runtime_kind_marker(name)` as read by the caller.
    pub runtime_kind_marker: Option<RuntimeHandoffKind>,
    /// `session_backend::process_session_pid(name).is_some()` as read by the caller.
    pub process_registry_hit: bool,
}

/// Pure: votes in fixed evidence order; disagreement reports the first vote and
/// the first vote that differs from it.
pub(crate) fn resolve_host_kind(evidence: HostEvidence<'_>) -> HostKindResolution {
    if !evidence
        .session_name
        .is_some_and(|name| !name.trim().is_empty())
    {
        return HostKindResolution::Unknown;
    }
    let mut votes: Vec<(HostKind, HostKindSource)> = Vec::with_capacity(3);
    if let Some(kind) = evidence.durable_runtime_kind {
        votes.push((kind_hint(kind), HostKindSource::DurableRuntimeKind));
    }
    if let Some(kind) = evidence.runtime_kind_marker {
        votes.push((kind_hint(kind), HostKindSource::RuntimeKindMarker));
    }
    if evidence.process_registry_hit {
        votes.push((HostKind::Process, HostKindSource::ProcessRegistry));
    }
    let Some(&first) = votes.first() else {
        return HostKindResolution::Unknown;
    };
    match votes.iter().copied().find(|(kind, _)| *kind != first.0) {
        None => HostKindResolution::Known {
            kind: first.0,
            source: first.1,
        },
        Some(second) => HostKindResolution::Conflict { first, second },
    }
}

// No wildcard arm: a new runtime kind must choose its host here.
fn kind_hint(kind: RuntimeHandoffKind) -> HostKind {
    match kind {
        RuntimeHandoffKind::ProcessBackend | RuntimeHandoffKind::ClaudeEAdapter => {
            HostKind::Process
        }
        RuntimeHandoffKind::LegacyTmuxWrapper
        | RuntimeHandoffKind::ClaudeTui
        | RuntimeHandoffKind::CodexTui => HostKind::Tmux,
    }
}

static TMUX_HOST: TmuxHost = TmuxHost;
static PROCESS_HOST: ProcessHost = ProcessHost;

pub(crate) fn host_for(kind: HostKind) -> &'static dyn InteractiveSessionHost {
    match kind {
        HostKind::Tmux => &TMUX_HOST,
        HostKind::Process => &PROCESS_HOST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HostKindSource::{DurableRuntimeKind, ProcessRegistry, RuntimeKindMarker};
    use RuntimeHandoffKind as R;

    fn named() -> HostEvidence<'static> {
        HostEvidence {
            session_name: Some("AgentDesk-claude-test"),
            ..HostEvidence::default()
        }
    }

    fn known(kind: HostKind, source: HostKindSource) -> HostKindResolution {
        HostKindResolution::Known { kind, source }
    }

    #[test]
    fn missing_or_blank_session_name_is_unknown_even_with_evidence() {
        for session_name in [None, Some(""), Some("   ")] {
            let evidence = HostEvidence {
                session_name,
                durable_runtime_kind: Some(R::ClaudeTui),
                runtime_kind_marker: Some(R::ClaudeTui),
                process_registry_hit: true,
            };
            assert_eq!(resolve_host_kind(evidence), HostKindResolution::Unknown);
        }
    }

    #[test]
    fn no_evidence_is_unknown_not_a_tmux_default() {
        assert_eq!(resolve_host_kind(named()), HostKindResolution::Unknown);
    }

    #[test]
    fn single_vote_is_known_with_its_source() {
        let durable = HostEvidence {
            durable_runtime_kind: Some(R::ClaudeTui),
            ..named()
        };
        assert_eq!(
            resolve_host_kind(durable),
            known(HostKind::Tmux, DurableRuntimeKind)
        );
        let marker = HostEvidence {
            runtime_kind_marker: Some(R::ProcessBackend),
            ..named()
        };
        assert_eq!(
            resolve_host_kind(marker),
            known(HostKind::Process, RuntimeKindMarker)
        );
        let registry = HostEvidence {
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(registry),
            known(HostKind::Process, ProcessRegistry)
        );
    }

    #[test]
    fn agreeing_votes_report_the_first_source() {
        let evidence = HostEvidence {
            durable_runtime_kind: Some(R::ClaudeEAdapter),
            runtime_kind_marker: Some(R::ProcessBackend),
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(evidence),
            known(HostKind::Process, DurableRuntimeKind)
        );
    }

    #[test]
    fn durable_tmux_with_registry_hit_is_a_conflict() {
        let evidence = HostEvidence {
            durable_runtime_kind: Some(R::ClaudeTui),
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(evidence),
            HostKindResolution::Conflict {
                first: (HostKind::Tmux, DurableRuntimeKind),
                second: (HostKind::Process, ProcessRegistry),
            }
        );
    }

    #[test]
    fn conflict_pairs_the_first_vote_with_the_first_disagreeing_vote() {
        // Process / Tmux / Process: the second Process vote agrees with the first.
        let evidence = HostEvidence {
            durable_runtime_kind: Some(R::ProcessBackend),
            runtime_kind_marker: Some(R::ClaudeTui),
            process_registry_hit: true,
            ..named()
        };
        assert_eq!(
            resolve_host_kind(evidence),
            HostKindResolution::Conflict {
                first: (HostKind::Process, DurableRuntimeKind),
                second: (HostKind::Tmux, RuntimeKindMarker),
            }
        );
    }

    #[test]
    fn kind_hint_maps_every_runtime_kind() {
        for (runtime, host) in [
            (R::ProcessBackend, HostKind::Process),
            (R::ClaudeEAdapter, HostKind::Process),
            (R::LegacyTmuxWrapper, HostKind::Tmux),
            (R::ClaudeTui, HostKind::Tmux),
            (R::CodexTui, HostKind::Tmux),
        ] {
            assert_eq!(kind_hint(runtime), host, "{runtime:?}");
        }
    }

    #[test]
    fn host_for_returns_the_host_of_that_kind() {
        for kind in [HostKind::Tmux, HostKind::Process] {
            assert_eq!(host_for(kind).kind(), kind);
        }
    }
}
