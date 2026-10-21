use std::cell::Cell;

use super::*;
use crate::config::session_hosts::EndpointMode;
use crate::services::session_host::herdr::policy::Unverified;
use crate::services::session_host::herdr::policy::permit::{
    ExistingMutationEvidence, grant_for_test,
};

thread_local! {
    static RESTORE_READS: Cell<usize> = const { Cell::new(0) };
}

fn counted_off(transport: &dyn HerdrTransport, endpoint: &HerdrEndpoint) -> RestoreResume {
    RESTORE_READS.with(|reads| reads.set(reads.get() + 1));
    off_where_dialled(transport, endpoint)
}

fn counted_invalid(_: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
    RESTORE_READS.with(|reads| reads.set(reads.get() + 1));
    RestoreResume::Unverified(RestoreUnverified::ConfigNotCanonical)
}

fn witness(rig: &Rig, pid: u32) -> ServerWitness {
    ServerWitness {
        socket: rig.server.path.clone(),
        pid,
        start: start(1_000).identity,
    }
}

fn policy_target(
    rig: &Rig,
    mode: EndpointMode,
    evidence: ExistingMutationEvidence,
    restore: Restore,
    os: Arc<dyn ProcessOs>,
) -> HerdrTarget {
    let configured = endpoint(&rig.server).with_mode(mode);
    let registry = HerdrRegistry::with_transport(configured, transport(&rig.server, vec![SERVER]))
        .with_reads((restore, os));
    registry
        .target_with_evidence(&stored(&rig.server, HostedState::Bound), evidence)
        .expect("complete recorded execution makes a target")
}

fn admitted(rig: &Rig, nonce: &str, pid: u32) -> ExistingMutationEvidence {
    ExistingMutationEvidence {
        startup: None,
        grant: Some(grant_for_test(
            &crate::services::herdr_launch::HerdrLaunchEndpoint {
                execution_node: endpoint(&rig.server).execution_node().into(),
                config_key: endpoint(&rig.server).config_key().into(),
                socket_addr: rig.server.path.to_string_lossy().into_owned(),
                herdr_session: endpoint(&rig.server).herdr_session().into(),
            },
            nonce,
            witness(rig, pid),
        )),
    }
}

#[test]
fn policy_legacy_mutations_keep_e7_and_need_no_new_evidence() {
    if run_terminate_child("policy::policy_legacy_mutations_keep_e7_and_need_no_new_evidence") {
        return;
    }
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = policy_target(
        &rig,
        EndpointMode::LegacyDedicated,
        ExistingMutationEvidence::default(),
        counted_off,
        launched_os(&rig),
    );
    assert_eq!(
        target.pin(Mutation::Input),
        Ok(()),
        "legacy_mutation_no_new_proof_contract"
    );
    assert_eq!(target.send_text("x").unwrap(), HostMutation::Confirmed);
    target
        .pin(Mutation::Cancel)
        .expect("Legacy Cancel needs only E7");
    assert_eq!(
        target.send_keys(&[HostKey::Escape]).unwrap(),
        HostMutation::Confirmed
    );
    target
        .pin_terminate()
        .expect("Legacy Terminate needs only E7");
    assert_eq!(target.send_close_pinned(), CloseEffect::Acknowledged);
    assert_eq!(
        RESTORE_READS.with(Cell::get),
        3,
        "Legacy E7 call order/count unchanged"
    );
    assert_eq!(
        rig.server
            .mutations()
            .iter()
            .map(|call| call["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["pane.send_text", "pane.send_keys", "pane.close"]
    );
}

#[test]
fn policy_user_grant_allows_all_mutations_without_current_file_policy() {
    if run_terminate_child(
        "policy::policy_user_grant_allows_all_mutations_without_current_file_policy",
    ) {
        return;
    }
    crate::services::session_host::policy::permit::EVENTS
        .with(|events| events.borrow_mut().clear());
    let rig = rig();
    let _switch = LiveTerminateSwitch::new();
    let target = policy_target(
        &rig,
        EndpointMode::UserDefault,
        admitted(&rig, NONCE, SERVER),
        counted_invalid,
        launched_os(&rig),
    );
    assert_eq!(
        target.pin(Mutation::Input),
        Ok(()),
        "existing_grant_ignores_current_e7_contract"
    );
    assert_eq!(target.send_text("x").unwrap(), HostMutation::Confirmed);
    target
        .pin(Mutation::Cancel)
        .expect("historical grant admits exact Cancel");
    assert_eq!(
        target.send_keys(&[HostKey::Escape]).unwrap(),
        HostMutation::Confirmed
    );
    target
        .pin_terminate()
        .expect("historical grant admits exact Terminate");
    assert_eq!(target.send_close_pinned(), CloseEffect::Acknowledged);
    crate::services::session_host::policy::permit::EVENTS.with(|events| {
        assert_eq!(
            events.borrow().as_slice(),
            [
                "ExistingMutationPermit",
                "exact_identity_probe",
                "Input",
                "ExistingMutationPermit",
                "exact_identity_probe",
                "Cancel",
                "ExistingMutationPermit",
                "exact_identity_probe",
                "Close"
            ],
            "existing_mutation_order_contract"
        )
    });
    assert_eq!(
        target.revalidate_termination_reads(),
        Ok(witness(&rig, SERVER))
    );
    assert_eq!(
        RESTORE_READS.with(Cell::get),
        0,
        "UserDefault existing execution never rereads E7/config"
    );
    assert_eq!(rig.server.mutations().len(), 3);
}

#[test]
fn policy_missing_or_foreign_grant_refuses_before_any_exact_probe() {
    let rig = rig();
    for evidence in [
        ExistingMutationEvidence::default(),
        admitted(&rig, "another-nonce", SERVER),
        admitted(&rig, NONCE, SERVER + 1),
    ] {
        let target = policy_target(
            &rig,
            EndpointMode::UserDefault,
            evidence,
            counted_off,
            launched_os(&rig),
        );
        for mutation in [Mutation::Input, Mutation::Cancel, Mutation::Terminate] {
            assert_eq!(
                target.pin(mutation),
                Err(HerdrGateRefusal::PolicyUnverified(Unverified::Identity)),
                "permit refusal must precede exact identity probe"
            );
        }
    }
    assert_eq!(RESTORE_READS.with(Cell::get), 0);
    assert!(
        rig.server.mutations().is_empty(),
        "no Input/Cancel/Close after missing evidence"
    );
    assert!(
        rig.server.conns().iter().all(|seen| seen.request.is_none()),
        "no pane evidence probe before a permit"
    );
}

#[test]
fn policy_grant_does_not_adopt_replaced_root_or_provider() {
    let rig = rig();
    for (os, refusal) in [
        (replaced_root as Os, HerdrGateRefusal::RootReplaced),
        (replaced_provider as Os, HerdrGateRefusal::ProviderReplaced),
    ] {
        let target = policy_target(
            &rig,
            EndpointMode::UserDefault,
            admitted(&rig, NONCE, SERVER),
            counted_invalid,
            os(&rig),
        );
        for mutation in [Mutation::Input, Mutation::Cancel, Mutation::Terminate] {
            assert_eq!(
                target.pin(mutation),
                Err(refusal),
                "a grant cannot replace exact process identity"
            );
        }
    }
    assert!(rig.server.mutations().is_empty());
}

#[test]
fn policy_registry_default_evidence_does_not_fabricate_a_grant() {
    let rig = rig();
    let configured = endpoint(&rig.server).with_mode(EndpointMode::UserDefault);
    let registry = HerdrRegistry::with_transport(configured, transport(&rig.server, vec![SERVER]))
        .with_reads((counted_off, launched_os(&rig)));
    let target = registry
        .target(&stored(&rig.server, HostedState::Bound))
        .unwrap();
    assert_eq!(
        target.pin(Mutation::Cancel),
        Err(HerdrGateRefusal::PolicyUnverified(Unverified::Identity)),
        "unintroduced durable store must never fabricate evidence"
    );
    assert!(rig.server.mutations().is_empty());
    assert_eq!(RESTORE_READS.with(Cell::get), 0);
}

#[test]
fn policy_legacy_admission_and_terminate_version_refusals_stay_exact() {
    let rig = rig();
    let target = policy_target(
        &rig,
        EndpointMode::LegacyDedicated,
        ExistingMutationEvidence::default(),
        counted_invalid,
        launched_os(&rig),
    );
    let _off = herdr_admission::force_for_test(Admission::new(Some("off".as_ref()), None));
    assert_eq!(
        target.pin(Mutation::Input),
        Err(HerdrGateRefusal::AdmissionStopped(
            crate::services::herdr_admission::StopCause::Env
        ))
    );
    assert_eq!(
        RESTORE_READS.with(Cell::get),
        0,
        "admission refusal stays ahead of E7"
    );
    assert_eq!(
        target.pin(Mutation::Cancel),
        Err(HerdrGateRefusal::RestoreUnverified(
            RestoreUnverified::ConfigNotCanonical
        ))
    );
    assert_eq!(RESTORE_READS.with(Cell::get), 1);
    fn unsupported(_: &dyn HerdrTransport, _: &HerdrEndpoint) -> RestoreResume {
        RestoreResume::Unverified(RestoreUnverified::VersionNotVerified)
    }
    let target = policy_target(
        &rig,
        EndpointMode::LegacyDedicated,
        ExistingMutationEvidence::default(),
        unsupported,
        launched_os(&rig),
    );
    assert_eq!(
        target.pin_terminate(),
        Err(HerdrGateRefusal::TerminateUnsupportedServer)
    );
    assert!(rig.server.mutations().is_empty());
}
