//! Session-host kind votes and the guard in front of the first state change.

use proptest::prelude::*;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::session_host::{
    AutomaticEffect, GuardVerdict, HostEvidence, HostKind, HostKindResolution, HostKindSource,
    HostLiveness, ResolvedSessionTarget, SessionTargetInput, StateChange, TargetHost, TargetSource,
    UnknownHost, guard_first_state_change, resolve_host_kind,
};

fn runtime_kind() -> impl Strategy<Value = Option<RuntimeHandoffKind>> {
    prop_oneof![
        Just(None),
        Just(Some(RuntimeHandoffKind::LegacyTmuxWrapper)),
        Just(Some(RuntimeHandoffKind::ClaudeTui)),
        Just(Some(RuntimeHandoffKind::CodexTui)),
        Just(Some(RuntimeHandoffKind::ProcessBackend)),
    ]
}

fn mapped(kind: RuntimeHandoffKind) -> HostKind {
    match kind {
        RuntimeHandoffKind::ProcessBackend => HostKind::Process,
        RuntimeHandoffKind::LegacyTmuxWrapper
        | RuntimeHandoffKind::ClaudeTui
        | RuntimeHandoffKind::CodexTui => HostKind::Tmux,
    }
}

fn effect() -> impl Strategy<Value = AutomaticEffect> {
    prop_oneof![
        Just(AutomaticEffect::Clear),
        Just(AutomaticEffect::Finalize),
        Just(AutomaticEffect::FailDispatch),
        Just(AutomaticEffect::Recreate),
        Just(AutomaticEffect::Kill),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]
    fn disagreeing_host_votes_never_resolve_to_one_kind(
        name in "[a-z][a-z0-9-]{0,12}",
        durable in runtime_kind(),
        marker in runtime_kind(),
        process_hit in any::<bool>(),
    ) {
        let resolution = resolve_host_kind(HostEvidence {
            session_name: Some(&name),
            durable_runtime_kind: durable,
            runtime_kind_marker: marker,
            process_registry_hit: process_hit,
        });
        let mut votes = Vec::new();
        if let Some(kind) = durable {
            votes.push(mapped(kind));
        }
        if let Some(kind) = marker {
            votes.push(mapped(kind));
        }
        if process_hit {
            votes.push(HostKind::Process);
        }
        match resolution {
            HostKindResolution::Unknown => prop_assert!(votes.is_empty()),
            HostKindResolution::Known { kind, .. } => {
                prop_assert!(votes.iter().all(|vote| *vote == kind));
            }
            HostKindResolution::Conflict { first, second } => {
                prop_assert_ne!(first.0, second.0);
                prop_assert!(votes.contains(&first.0) && votes.contains(&second.0));
            }
        }
    }
}

pub(super) fn run() -> Vec<String> {
    [
        (
            "disagreeing_host_votes_never_resolve_to_one_kind",
            disagreeing_host_votes_never_resolve_to_one_kind as fn(),
        ),
        (
            "blank_session_name_is_unknown_even_with_votes",
            blank_session_name_is_unknown_even_with_votes,
        ),
        (
            "automatic_effects_are_exclusive_to_a_known_legacy_host",
            automatic_effects_are_exclusive_to_a_known_legacy_host,
        ),
    ]
    .into_iter()
    .filter_map(|(name, test)| super::exercise(name, test))
    .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]
    fn blank_session_name_is_unknown_even_with_votes(
        durable in runtime_kind(),
        marker in runtime_kind(),
        process_hit in any::<bool>(),
        blank in "\\s{0,4}",
    ) {
        let resolution = resolve_host_kind(HostEvidence {
            session_name: Some(blank.as_str()).filter(|name| !name.is_empty()),
            durable_runtime_kind: durable,
            runtime_kind_marker: marker,
            process_registry_hit: process_hit,
        });
        prop_assert_eq!(resolution, HostKindResolution::Unknown);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, .. ProptestConfig::default() })]
    fn automatic_effects_are_exclusive_to_a_known_legacy_host(effect in effect(), observed in prop_oneof![
        Just(HostLiveness::Live),
        Just(HostLiveness::DeadOrAbsent),
        Just(HostLiveness::ProbeError),
    ]) {
        let hosts = [
            TargetHost::Unknown(UnknownHost::NoHostEvidence),
            TargetHost::Conflict {
                first: (HostKind::Tmux, TargetSource::HostMarker),
                second: (HostKind::Process, TargetSource::InflightLocator),
            },
            TargetHost::Known {
                kind: HostKind::Herdr,
                source: TargetSource::SessionRecord,
                name: "pane".into(),
            },
        ];
        for host in hosts {
            let target = ResolvedSessionTarget {
                input: SessionTargetInput::SessionKey("claude/h/host:session".into()),
                session_key: None,
                host,
            };
            let verdict = guard_first_state_change(
                &target,
                StateChange::Automatic { effect, observed: Some(observed) },
            );
            prop_assert_ne!(
                verdict,
                GuardVerdict::Proceed,
                "host {:?} effect {:?}",
                target.host,
                effect
            );
        }
        let tmux = ResolvedSessionTarget {
            input: SessionTargetInput::SessionKey("claude/h/host:session".into()),
            session_key: None,
            host: TargetHost::Known {
                kind: HostKind::Tmux,
                source: TargetSource::Legacy(HostKindSource::DurableRuntimeKind),
                name: "session".into(),
            },
        };
        let verdict = guard_first_state_change(
            &tmux,
            StateChange::Automatic { effect, observed: Some(observed) },
        );
        if observed == HostLiveness::ProbeError {
            prop_assert_ne!(verdict, GuardVerdict::Proceed);
        } else {
            prop_assert_eq!(verdict, GuardVerdict::Proceed);
        }
    }
}
