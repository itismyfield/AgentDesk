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

/// Adds what the inflight row carries without overwriting what the caller read.
/// A locator or tmux name that differs from an earlier reading is dropped, never guessed.
pub(in crate::services::discord) fn with_inflight_row(
    mut evidence: SessionTargetEvidence,
    row: &InflightTurnState,
) -> SessionTargetEvidence {
    let locator = locator_witness(row.host_locator.as_ref());
    evidence.inflight_locator = match evidence.inflight_locator {
        HostWitness::ReadFailed(_) => locator,
        earlier if earlier == locator => locator,
        earlier => HostWitness::Unrecognized(format!("{earlier:?} then {locator:?}")),
    };
    evidence.runtime_kind_unrecognized |= evidence
        .inflight_runtime_kind
        .is_some_and(|earlier| Some(earlier) != row.runtime_kind);
    evidence.inflight_runtime_kind = row.runtime_kind;
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
    fn caller_runtime_evidence_survives_the_inflight_row_merge() {
        use RuntimeHandoffKind::{ClaudeTui, LegacyTmuxWrapper, ProcessBackend};
        let tmux = || HostWitness::Known {
            kind: HostKind::Tmux,
            target: Some(TMUX_NAME.to_string()),
        };
        let herdr = PersistedHostLocator::Known(HostedRuntimeLocator {
            execution_node: None,
            host_kind: HostKind::Herdr,
            host_session_id: "herdr-session-1".to_string(),
            pane: Some("w1-1".to_string()),
        });
        let with_row_kind = |kind, locator| {
            let mut state = row(locator);
            state.runtime_kind = Some(kind);
            state
        };
        let caller = |kind, record, marker| SessionTargetEvidence {
            session_record: record,
            host_marker: marker,
            durable_runtime_kind: kind,
            ..recorded(Some(TMUX_NAME))
        };
        let (absent, process) = (|| HostWitness::Absent, Some(ProcessBackend));
        let conflicts = [
            ("record", caller(process, tmux(), absent()), row(None)),
            ("marker", caller(process, absent(), tmux()), row(None)),
            (
                "locator",
                caller(Some(LegacyTmuxWrapper), absent(), absent()),
                row(Some(herdr.clone())),
            ),
            (
                "row vote",
                caller(None, tmux(), absent()),
                with_row_kind(ProcessBackend, None),
            ),
            (
                "legacy votes",
                caller(Some(ClaudeTui), absent(), absent()),
                with_row_kind(ProcessBackend, None),
            ),
        ];
        for (label, recorded, row) in conflicts {
            let (host, verdict) = kill_verdict(recorded, row);
            assert!(
                matches!(host, TargetHost::Conflict { .. }),
                "{label}: {host:?}"
            );
            assert_eq!(
                verdict,
                GuardVerdict::Refused(GuardRefusal::HostConflict),
                "{label}"
            );
        }

        let read_twice = SessionTargetEvidence {
            inflight_locator: tmux(),
            ..recorded(Some(TMUX_NAME))
        };
        let kind_read_twice = SessionTargetEvidence {
            inflight_runtime_kind: process,
            ..recorded(Some(TMUX_NAME))
        };
        for (evidence, row) in [(read_twice, row(Some(herdr))), (kind_read_twice, row(None))] {
            let (host, verdict) = kill_verdict(evidence, row);
            assert!(matches!(host, TargetHost::Unknown(_)), "{host:?}");
            assert_eq!(verdict, GuardVerdict::Refused(GuardRefusal::UnknownHost));
        }

        let (host, verdict) = kill_verdict(caller(Some(ClaudeTui), absent(), absent()), row(None));
        assert!(
            matches!(&host, TargetHost::Known { kind: HostKind::Tmux, name, .. } if name == TMUX_NAME),
            "agreeing kinds keep the legacy reading: {host:?}"
        );
        assert_eq!(verdict, GuardVerdict::Proceed);
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
