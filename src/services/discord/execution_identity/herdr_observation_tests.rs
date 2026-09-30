use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{location, owner, record};
use HerdrExecutionMatch::{Match, Mismatch, Unknown};

fn bound() -> HostedExecution {
    record(&owner("100"), "n1", HostedState::Bound)
}

/// What a reader would see for the stored execution while it is still running.
fn unchanged(stored: &HostedExecution) -> HerdrCurrentExecution {
    let evidence = stored.expected.clone().unwrap();
    HerdrCurrentExecution {
        location: stored.location.clone().unwrap(),
        binding_nonce: Some(stored.execution_nonce.clone()),
        root: Some(evidence.root),
        provider_process: Some(evidence.provider_process),
        marker: HerdrMarkerEvidence::Herdr,
    }
}

#[test]
fn herdr_stored_execution_matches_only_when_every_stored_field_is_confirmed() {
    let stored = bound();
    assert_eq!(compare_herdr_execution(&stored, &unchanged(&stored)), Match);

    let cases: Vec<(&str, fn(&mut HerdrCurrentExecution), HerdrExecutionMatch)> = vec![
        (
            "same nonce, provider process replaced",
            |c| c.provider_process.as_mut().unwrap().pid += 7,
            Mismatch(HerdrMismatch::ProviderReplaced),
        ),
        (
            "same nonce, provider restarted under the same pid",
            |c| c.provider_process.as_mut().unwrap().start = "1700009999".into(),
            Mismatch(HerdrMismatch::ProviderReplaced),
        ),
        (
            "root pid reused with a new start",
            |c| c.root.as_mut().unwrap().start = "1700009999".into(),
            Mismatch(HerdrMismatch::RootReplaced),
        ),
        (
            "root replaced",
            |c| c.root.as_mut().unwrap().pid += 50,
            Mismatch(HerdrMismatch::RootReplaced),
        ),
        (
            "another nonce in the pane",
            |c| c.binding_nonce = Some("n2".into()),
            Mismatch(HerdrMismatch::OtherNonce),
        ),
        (
            "another pane on the same endpoint",
            |c| c.location.pane_id = "pane-2".into(),
            Mismatch(HerdrMismatch::OtherPane),
        ),
        (
            "same pane id on another endpoint",
            |c| c.location.endpoint_config_key = "herdr.other".into(),
            Unknown(HerdrUnknown::EndpointChanged),
        ),
        (
            "same pane id behind another socket",
            |c| c.location.socket_addr = "/adk/other.sock".into(),
            Unknown(HerdrUnknown::EndpointChanged),
        ),
        (
            "same pane id in another named session",
            |c| c.location.named_session = "other".into(),
            Unknown(HerdrUnknown::EndpointChanged),
        ),
        (
            "same pane id on another node",
            |c| c.location.execution_node = "other-node".into(),
            Unknown(HerdrUnknown::EndpointChanged),
        ),
        (
            "lost marker",
            |c| c.marker = HerdrMarkerEvidence::Lost,
            Unknown(HerdrUnknown::MarkerLost),
        ),
        (
            "marker names tmux",
            |c| c.marker = HerdrMarkerEvidence::OtherHost,
            Mismatch(HerdrMismatch::OtherHostMarker),
        ),
        (
            "root not observed",
            |c| c.root = None,
            Unknown(HerdrUnknown::NotObserved),
        ),
        (
            "provider not observed",
            |c| c.provider_process = None,
            Unknown(HerdrUnknown::NotObserved),
        ),
        (
            "nonce not observed",
            |c| c.binding_nonce = None,
            Unknown(HerdrUnknown::NotObserved),
        ),
    ];
    for (name, change, verdict) in cases {
        let mut current = unchanged(&stored);
        change(&mut current);
        assert_eq!(
            compare_herdr_execution(&stored, &current),
            verdict,
            "{name}"
        );
    }
}

#[test]
fn herdr_pending_without_launch_evidence_is_never_a_match() {
    let owner = owner("100");
    let mut pending = record(&owner, "n1", HostedState::Pending);
    let current = unchanged(&bound());
    assert_eq!(
        compare_herdr_execution(&pending, &current),
        Unknown(HerdrUnknown::NoStoredEvidence)
    );
    pending.location = Some(location("pane-1"));
    assert_eq!(
        compare_herdr_execution(&pending, &current),
        Unknown(HerdrUnknown::NoStoredEvidence)
    );
}
