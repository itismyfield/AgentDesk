//! Host witnesses an inflight row and the `.host_kind` marker give the session
//! target resolver. Unknown on-disk values stay unreadable, never absent.
#![cfg_attr(not(test), allow(dead_code))]

use super::InflightTurnState;
use super::host_locator::PersistedHostLocator;
use crate::services::session_host::{HostKind, HostWitness, SessionTargetEvidence};
use crate::services::tmux_common::host_marker::HostKindMarker;

pub(in crate::services::discord) fn locator_witness(
    locator: Option<&PersistedHostLocator>,
) -> HostWitness {
    match locator {
        None => HostWitness::Absent,
        Some(PersistedHostLocator::Known(locator)) => HostWitness::Known {
            kind: locator.host_kind,
            target: match locator.host_kind {
                HostKind::Herdr => locator.pane.clone(),
                HostKind::Tmux | HostKind::Process => Some(locator.host_session_id.clone()),
            },
        },
        Some(PersistedHostLocator::Unknown(raw)) => HostWitness::Unrecognized(raw.to_string()),
    }
}

pub(in crate::services::discord) fn marker_witness(marker: HostKindMarker) -> HostWitness {
    match marker {
        HostKindMarker::Absent => HostWitness::Absent,
        HostKindMarker::Known(kind) => HostWitness::Known { kind, target: None },
        HostKindMarker::Unrecognized(raw) => HostWitness::Unrecognized(raw),
        HostKindMarker::ReadFailed(error) => HostWitness::ReadFailed(error),
    }
}

/// Adds what the inflight row carries. A tmux name that differs from the
/// sessions row drops both, so no legacy name is guessed.
pub(in crate::services::discord) fn with_inflight_row(
    mut evidence: SessionTargetEvidence,
    row: &InflightTurnState,
) -> SessionTargetEvidence {
    evidence.inflight_locator = locator_witness(row.host_locator.as_ref());
    evidence.durable_runtime_kind = row.runtime_kind.or(evidence.durable_runtime_kind);
    evidence.runtime_kind_unrecognized |= row.runtime_kind_unknown_on_disk;
    match (&evidence.session_name, &row.tmux_session_name) {
        (None, name) => evidence.session_name = name.clone(),
        (Some(recorded), Some(name)) if recorded != name => evidence.session_name = None,
        _ => {}
    }
    evidence
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;
    use crate::services::session_host::{
        AutomaticEffect, GuardRefusal, GuardVerdict, HostedRuntimeLocator,
        SessionTargetEvidenceSource, SessionTargetInput, StateChange, TargetHost,
        guard_first_state_change, resolve_session_target,
    };

    const TMUX_NAME: &str = "AgentDesk-claude-adk-cc";

    struct RowSource {
        recorded: SessionTargetEvidence,
        row: InflightTurnState,
    }

    impl SessionTargetEvidenceSource for RowSource {
        fn read_evidence(&self, _input: &SessionTargetInput) -> SessionTargetEvidence {
            with_inflight_row(self.recorded.clone(), &self.row)
        }
    }

    fn row(locator: Option<PersistedHostLocator>) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            5340,
            Some("adk-cc".to_string()),
            222,
            333,
            444,
            "hello".to_string(),
            None,
            Some(TMUX_NAME.to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.host_locator = locator;
        state
    }

    fn recorded(session_name: Option<&str>) -> SessionTargetEvidence {
        SessionTargetEvidence {
            session_key: Some(format!("claude/hash/mac-mini:{TMUX_NAME}")),
            session_name: session_name.map(str::to_string),
            session_record: HostWitness::Absent,
            inflight_locator: HostWitness::ReadFailed("filled from the row".to_string()),
            host_marker: HostWitness::Absent,
            ..SessionTargetEvidence::unread()
        }
    }

    fn kill_verdict(
        recorded: SessionTargetEvidence,
        row: InflightTurnState,
    ) -> (TargetHost, GuardVerdict) {
        let input = SessionTargetInput::SessionKey(format!("claude/hash/mac-mini:{TMUX_NAME}"));
        let resolved = resolve_session_target(input, &RowSource { recorded, row });
        let change = StateChange::Automatic {
            effect: AutomaticEffect::Kill,
            observed: None,
        };
        let verdict = guard_first_state_change(&resolved, change);
        (resolved.host, verdict)
    }

    #[test]
    fn inflight_row_host_evidence_reaches_the_guard_verdict() {
        let herdr = PersistedHostLocator::Known(HostedRuntimeLocator {
            execution_node: None,
            host_kind: HostKind::Herdr,
            host_session_id: "herdr-session-1".to_string(),
            pane: Some("w1-1".to_string()),
        });
        let (host, verdict) = kill_verdict(recorded(None), row(Some(herdr)));
        assert!(
            matches!(&host, TargetHost::Known { kind: HostKind::Herdr, name, .. } if name == "w1-1"),
            "{host:?}"
        );
        assert_eq!(verdict, GuardVerdict::DeferredToExistingRecovery);

        let (host, verdict) = kill_verdict(recorded(Some(TMUX_NAME)), row(None));
        assert!(
            matches!(&host, TargetHost::Known { kind: HostKind::Tmux, name, .. } if name == TMUX_NAME),
            "legacy row keeps its tmux reading: {host:?}"
        );
        assert_eq!(verdict, GuardVerdict::Proceed);

        let unknown_locator = PersistedHostLocator::Unknown(json!({"host_kind": "zellij"}));
        let mut future_kind = row(None);
        future_kind.runtime_kind = None;
        future_kind.runtime_kind_unknown_on_disk = true;
        for (label, recorded, row) in [
            (
                "unknown locator",
                recorded(None),
                row(Some(unknown_locator)),
            ),
            ("unknown runtime kind", recorded(None), future_kind),
            (
                "renamed tmux session",
                recorded(Some("AgentDesk-claude-other")),
                row(None),
            ),
        ] {
            let (host, verdict) = kill_verdict(recorded, row);
            assert!(matches!(host, TargetHost::Unknown(_)), "{label}: {host:?}");
            assert_eq!(
                verdict,
                GuardVerdict::Refused(GuardRefusal::UnknownHost),
                "{label}"
            );
        }
    }

    #[test]
    fn host_kind_marker_maps_without_reading_a_failure_as_absent() {
        for (marker, witness) in [
            (HostKindMarker::Absent, HostWitness::Absent),
            (
                HostKindMarker::Known(HostKind::Herdr),
                HostWitness::Known {
                    kind: HostKind::Herdr,
                    target: None,
                },
            ),
            (
                HostKindMarker::Unrecognized("zellij".to_string()),
                HostWitness::Unrecognized("zellij".to_string()),
            ),
            (
                HostKindMarker::ReadFailed("EACCES".to_string()),
                HostWitness::ReadFailed("EACCES".to_string()),
            ),
        ] {
            assert_eq!(marker_witness(marker.clone()), witness, "{marker:?}");
        }
    }
}
