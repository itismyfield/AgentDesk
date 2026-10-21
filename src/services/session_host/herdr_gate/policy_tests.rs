// close 송신 시험(fake rig)
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
    let mutations = rig.server.mutations();
    assert_eq!(mutations.len(), 3);
    assert_eq!(
        mutations[..2]
            .iter()
            .map(|call| call["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["pane.send_text", "pane.send_keys"]
    );
    assert_closed_method(&mutations[2]["method"]);
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

#[test]
fn policy_grant_cache_failure_is_distinct_and_probes_nothing() {
    for cache in ["grants", "witnessed", "proof"] {
        RESTORE_READS.with(|reads| reads.set(0));
        let rig = rig();
        let configured = endpoint(&rig.server).with_mode(EndpointMode::UserDefault);
        let host = crate::services::session_host::SocketHerdrLaunchHost::new(
            vec![configured.clone()],
            Default::default(),
        );
        host.seed_witness_for_test(&configured, witness(&rig, SERVER));
        let contract = match cache {
            "grants" => {
                host.poison_grants_for_test();
                "grant_cache_failure_is_not_absent_authority_contract"
            }
            "witnessed" => {
                host.poison_witnessed_for_test();
                "witnessed_cache_failure_is_not_absent_authority_contract"
            }
            _ => {
                host.poison_proof_for_test();
                "proof_cache_failure_is_not_absent_authority_contract"
            }
        };
        let registry =
            HerdrRegistry::with_transport(configured, transport(&rig.server, vec![SERVER]))
                .with_launch_host(host)
                .with_reads((counted_off, launched_os(&rig)));
        let target = registry
            .target(&stored(&rig.server, HostedState::Bound))
            .unwrap();
        assert_eq!(
            target.pin(Mutation::Cancel),
            Err(HerdrGateRefusal::PolicyUnverified(Unverified::Cache)),
            "{contract}"
        );
        assert!(
            rig.server.conns().is_empty(),
            "{cache}_cache_failure_probes_zero_contract"
        );
        assert!(
            rig.server.mutations().is_empty(),
            "{cache}_cache_failure_mutates_zero_contract"
        );
        assert_eq!(
            RESTORE_READS.with(Cell::get),
            0,
            "{cache}_cache_failure_reads_e7_zero_contract"
        );
    }
}

#[test]
fn policy_prepared_store_failure_is_distinct_from_absent_authority() {
    for (prepared, refusal, contract) in [
        (
            Ok(Vec::new()),
            Unverified::Identity,
            "prepared_store_absence_is_identity_contract",
        ),
        (
            Err(Unverified::File),
            Unverified::Cache,
            "prepared_store_read_failure_is_cache_contract",
        ),
    ] {
        RESTORE_READS.with(|reads| reads.set(0));
        let rig = rig();
        let configured = endpoint(&rig.server).with_mode(EndpointMode::UserDefault);
        let host = crate::services::session_host::SocketHerdrLaunchHost::new(
            vec![configured.clone()],
            Default::default(),
        )
        .with_prepared_for_test(prepared);
        host.seed_witness_for_test(&configured, witness(&rig, SERVER));
        let registry =
            HerdrRegistry::with_transport(configured, transport(&rig.server, vec![SERVER]))
                .with_launch_host(host)
                .with_reads((counted_off, launched_os(&rig)));
        let target = registry
            .target(&stored(&rig.server, HostedState::Bound))
            .unwrap();
        assert_eq!(
            target.pin(Mutation::Cancel),
            Err(HerdrGateRefusal::PolicyUnverified(refusal)),
            "{contract}"
        );
        assert!(
            if refusal == Unverified::Cache {
                rig.server.conns().is_empty()
            } else {
                rig.server.conns().iter().all(|seen| seen.request.is_none())
            },
            "prepared_store_refusal_probes_zero_contract"
        );
        assert!(
            rig.server.mutations().is_empty(),
            "prepared_store_refusal_mutates_zero_contract"
        );
        assert_eq!(
            RESTORE_READS.with(Cell::get),
            0,
            "prepared_store_refusal_reads_e7_zero_contract"
        );
    }
}

#[tokio::test]
async fn policy_restored_durable_grant_reaches_input_gate_without_reading_store_again() {
    use crate::services::herdr_launch::policy::LaunchGrantStore;
    use crate::services::session_host::policy::grant_store::FileGrantStore;

    RESTORE_READS.with(|reads| reads.set(0));
    let rig = rig();
    let temporary = tempfile::tempdir().unwrap();
    let grant_root = temporary.path().join("grants");
    let grant = admitted(&rig, NONCE, SERVER).grant.unwrap();
    let launch_endpoint = grant.endpoint().clone();
    let store = FileGrantStore::new(Some(grant_root.clone()));
    store.save(grant.clone()).await.unwrap();
    drop(store);
    let restarted_store = FileGrantStore::new(Some(grant_root.clone()));
    let restored = restarted_store.load_all(&[launch_endpoint]).await.unwrap();
    assert_eq!(
        restored,
        [grant],
        "restored_grant_exact_historical_authority_contract"
    );
    drop(restarted_store);
    std::fs::remove_dir_all(&grant_root).unwrap();
    assert!(
        !grant_root.exists(),
        "restored_grant_gate_fixture_store_is_absent_contract"
    );

    let configured = endpoint(&rig.server).with_mode(EndpointMode::UserDefault);
    let make_registry = |grants| {
        let host = crate::services::session_host::SocketHerdrLaunchHost::new(
            vec![configured.clone()],
            Default::default(),
        )
        .with_prepared_for_test(Ok(grants));
        HerdrRegistry::with_transport(configured.clone(), transport(&rig.server, vec![SERVER]))
            .with_launch_host(host)
            .with_reads((counted_invalid, launched_os(&rig)))
    };
    let absent = make_registry(Vec::new());
    let target = absent
        .target(&stored(&rig.server, HostedState::Bound))
        .unwrap();
    assert_eq!(
        target.pin(Mutation::Input),
        Err(HerdrGateRefusal::PolicyUnverified(Unverified::Identity)),
        "restored_grant_absence_refuses_input_contract"
    );
    assert!(
        rig.server.mutations().is_empty(),
        "restored_grant_absence_mutates_zero_contract"
    );
    let restored_registry = make_registry(restored);
    let target = restored_registry
        .target(&stored(&rig.server, HostedState::Bound))
        .unwrap();
    assert_eq!(
        target.pin(Mutation::Input),
        Ok(()),
        "restored_grant_memory_reaches_input_gate_contract"
    );
    assert_eq!(
        target.send_text("restored grant input").unwrap(),
        HostMutation::Confirmed,
        "restored_grant_memory_confirms_input_contract"
    );
    assert_eq!(
        rig.server.mutations().len(),
        1,
        "restored_grant_input_mutates_once_contract"
    );
    assert_eq!(
        RESTORE_READS.with(Cell::get),
        0,
        "restored_grant_does_not_reread_current_file_policy_contract"
    );
    assert!(
        !grant_root.exists(),
        "restored_grant_mutation_does_not_recreate_store_contract"
    );
}
