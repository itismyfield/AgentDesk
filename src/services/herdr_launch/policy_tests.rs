use super::*;
use crate::config::session_hosts::EndpointMode;
use crate::services::herdr_launch::policy::{
    LaunchGrantStore, PermitFuture, UNINTRODUCED, UserDefaultCreateRequest,
};
use crate::services::session_host::policy::permit::{LaunchAdmissionGrant, create_for_test};

use std::sync::atomic::AtomicBool;

type Events = Arc<Mutex<Vec<&'static str>>>;

struct Grants {
    pool: PgPool,
    events: Events,
    saved: Mutex<Option<LaunchAdmissionGrant>>,
    loads: AtomicUsize,
    saves: AtomicUsize,
    fail_save: AtomicBool,
    invalid_file_store: Option<crate::services::session_host::policy::grant_store::FileGrantStore>,
}

#[async_trait::async_trait]
impl LaunchGrantStore for Grants {
    async fn load(
        &self,
        _: &HerdrLaunchEndpoint,
        _: &str,
    ) -> Result<Option<LaunchAdmissionGrant>, String> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("grant_load");
        Ok(self.saved.lock().unwrap().clone())
    }

    async fn save(&self, grant: LaunchAdmissionGrant) -> Result<(), String> {
        self.saves.fetch_add(1, Ordering::SeqCst);
        let pending_committed = matches!(decoded(stored(&self.pool).await), HostedRecord::Known(row)
            if row.state == HostedState::Pending && row.location.is_none()
                && row.expected.is_none()
                && grant.witness_for(&endpoint(), &row.execution_nonce).is_ok());
        self.events.lock().unwrap().extend([
            if pending_committed {
                "pending_cas"
            } else {
                "pending_missing"
            },
            "grant_save",
        ]);
        if self.fail_save.load(Ordering::SeqCst) {
            return Err("injected grant durability failure".into());
        }
        if let Some(store) = &self.invalid_file_store {
            let mut witness = ServerWitness::for_test(1);
            witness.socket = "/different/socket".into();
            let invalid = crate::services::session_host::policy::permit::grant_for_test(
                grant.endpoint(),
                grant.nonce(),
                witness,
            );
            return store.save(invalid).await;
        }
        *self.saved.lock().unwrap() = Some(grant);
        Ok(())
    }
}

struct PolicyHost {
    inner: FakeHost,
    pool: PgPool,
    mode: EndpointMode,
    events: Events,
    grants: Grants,
    grant_accesses: AtomicUsize,
    permit_calls: AtomicUsize,
    user_creates: AtomicUsize,
    witness: Mutex<ServerWitness>,
    unintroduced: AtomicBool,
}

impl PolicyHost {
    fn new(pool: &PgPool, mode: EndpointMode) -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut witness = ServerWitness::for_test(1);
        witness.socket = endpoint().socket_addr.into();
        Self {
            inner: FakeHost::created("pane-policy"),
            pool: pool.clone(),
            mode,
            events: events.clone(),
            grants: Grants {
                pool: pool.clone(),
                events,
                saved: Mutex::default(),
                loads: AtomicUsize::new(0),
                saves: AtomicUsize::new(0),
                fail_save: AtomicBool::new(false),
                invalid_file_store: None,
            },
            grant_accesses: AtomicUsize::new(0),
            permit_calls: AtomicUsize::new(0),
            user_creates: AtomicUsize::new(0),
            witness: Mutex::new(witness),
            unintroduced: AtomicBool::new(false),
        }
    }

    fn prepare(&self, incarnation: &PreparedIncarnation) -> Result<HerdrLaunchCommand, String> {
        self.events.lock().unwrap().push("prepare_command");
        command(incarnation)
    }

    fn reset_calls(&self) {
        self.events.lock().unwrap().clear();
        self.grant_accesses.store(0, Ordering::SeqCst);
        self.permit_calls.store(0, Ordering::SeqCst);
        self.user_creates.store(0, Ordering::SeqCst);
        self.inner.creates.store(0, Ordering::SeqCst);
        self.inner.probes.store(0, Ordering::SeqCst);
        self.grants.loads.store(0, Ordering::SeqCst);
        self.grants.saves.store(0, Ordering::SeqCst);
    }

    fn assert_no_new_policy(&self) {
        assert_eq!(
            (
                self.permit_calls.load(Ordering::SeqCst),
                self.grant_accesses.load(Ordering::SeqCst),
                self.grants.loads.load(Ordering::SeqCst),
                self.grants.saves.load(Ordering::SeqCst),
            ),
            (0, 0, 0, 0),
            "legacy_launch_requires_no_proof_checker_or_grant_contract"
        );
    }
}

impl HerdrLaunchHost for PolicyHost {
    fn mode(&self, _: &HerdrLaunchEndpoint) -> EndpointMode {
        self.mode
    }

    fn create_permit<'a>(&'a self, endpoint: &'a HerdrLaunchEndpoint) -> PermitFuture<'a> {
        self.permit_calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("policy_check");
        Box::pin(async move {
            if self.mode == EndpointMode::LegacyDedicated {
                return Err("legacy does not require a new policy proof".into());
            }
            Ok(create_for_test(
                endpoint.clone(),
                self.witness.lock().unwrap().clone(),
            ))
        })
    }

    fn grants(&self) -> &dyn LaunchGrantStore {
        self.grant_accesses.fetch_add(1, Ordering::SeqCst);
        if self.unintroduced.load(Ordering::SeqCst) {
            &UNINTRODUCED
        } else {
            &self.grants
        }
    }

    fn current_witness(&self, _: &HerdrLaunchEndpoint) -> Result<ServerWitness, String> {
        self.events.lock().unwrap().push("current_witness");
        Ok(self.witness.lock().unwrap().clone())
    }

    fn create_user_default(&self, _: &UserDefaultCreateRequest) -> HerdrCreateOutcome {
        self.user_creates.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("user_create");
        self.inner.reply.clone()
    }

    fn restore_resume(&self, endpoint: &HerdrLaunchEndpoint) -> RestoreResume {
        self.events.lock().unwrap().push("legacy_e7");
        self.inner.restore_resume(endpoint)
    }

    fn create(&self, request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.events.lock().unwrap().push("legacy_create");
        self.inner.create(request)
    }

    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        let location_committed = matches!(decoded(run(stored(&self.pool))), HostedRecord::Known(row)
            if row.execution_nonce == probe.execution_nonce
                && row.location.as_ref() == Some(&probe.location));
        self.events.lock().unwrap().extend([
            if location_committed {
                "location_record"
            } else {
                "location_missing"
            },
            "evidence_probe",
        ]);
        self.inner.launch_evidence(probe)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn user_default_fresh_launch_commits_pending_and_grant_before_create_and_probe_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let host = Arc::new(PolicyHost::new(&pool, EndpointMode::UserDefault));
    let result = launch_herdr_session(
        &pool,
        launch(Some(endpoint())),
        |incarnation| host.prepare(incarnation),
        host.clone(),
    )
    .await;
    assert_eq!(
        *host.events.lock().unwrap(),
        [
            "policy_check",
            "prepare_command",
            "pending_cas",
            "grant_save",
            "user_create",
            "location_record",
            "evidence_probe"
        ],
        "fresh_pending_grant_create_location_probe_order_contract"
    );
    assert!(
        matches!(
            result,
            Ok(HerdrLaunchOutcome::Launched {
                evidence: Ok(()),
                ..
            })
        ),
        "fresh_user_default_launch_result_contract: {result:?}"
    );
    assert_eq!(host.permit_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.grants.saves.load(Ordering::SeqCst), 1);
    assert_eq!(host.grants.loads.load(Ordering::SeqCst), 0);
    assert_eq!(host.user_creates.load(Ordering::SeqCst), 1);
    assert_eq!(host.inner.creates.load(Ordering::SeqCst), 0);
    assert_eq!(host.inner.probes.load(Ordering::SeqCst), 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn user_default_pending_retry_reads_grant_and_current_witness_without_recreating_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for refusal in [
        "changed_witness",
        "other_nonce",
        "missing_grant",
        "unintroduced",
        "none",
    ] {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, None).await;
        let host = Arc::new(PolicyHost::new(&pool, EndpointMode::UserDefault));
        *host.inner.evidence.lock().unwrap() = Err(EvidenceGap::NoneYet);
        let first =
            launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
        assert!(
            matches!(
                first,
                Ok(HerdrLaunchOutcome::Launched {
                    evidence: Err(EvidenceGap::NoneYet),
                    ..
                })
            ),
            "retry_fixture_requires_a_recorded_pending_pane_contract: {first:?}"
        );
        let before = stored(&pool).await;
        host.reset_calls();
        *host.inner.evidence.lock().unwrap() = Ok(100);
        if refusal == "changed_witness" {
            host.witness.lock().unwrap().pid += 1;
        } else if refusal == "missing_grant" {
            *host.grants.saved.lock().unwrap() = None;
        } else if refusal == "unintroduced" {
            host.unintroduced.store(true, Ordering::SeqCst);
        } else if refusal == "other_nonce" {
            *host.grants.saved.lock().unwrap() = Some(
                create_for_test(endpoint(), host.witness.lock().unwrap().clone())
                    .grant("another-execution"),
            );
        }
        let prepared = AtomicUsize::new(0);
        let result = launch_herdr_session(
            &pool,
            launch(Some(endpoint())),
            |incarnation| {
                prepared.fetch_add(1, Ordering::SeqCst);
                host.prepare(incarnation)
            },
            host.clone(),
        )
        .await;
        let expected = match refusal {
            "changed_witness" => vec!["grant_load", "current_witness"],
            "other_nonce" | "missing_grant" => vec!["grant_load"],
            "unintroduced" => vec![],
            _ => vec![
                "grant_load",
                "current_witness",
                "location_record",
                "evidence_probe",
            ],
        };
        assert_eq!(
            *host.events.lock().unwrap(),
            expected,
            "pending_grant_witness_before_evidence_probe_order_contract: {refusal}"
        );
        assert_eq!(prepared.load(Ordering::SeqCst), 0);
        assert_eq!(
            host.permit_calls.load(Ordering::SeqCst),
            0,
            "Pending must not rerun the current-file checker"
        );
        assert_eq!(host.grants.saves.load(Ordering::SeqCst), 0);
        assert_eq!(host.inner.creates.load(Ordering::SeqCst), 0);
        assert_eq!(
            host.user_creates.load(Ordering::SeqCst),
            0,
            "Pending must not resend the command"
        );
        if refusal == "none" {
            assert!(
                matches!(
                    result,
                    Ok(HerdrLaunchOutcome::Launched {
                        evidence: Ok(()),
                        ..
                    })
                ),
                "pending_success_result_contract: {result:?}"
            );
            assert_eq!(host.inner.probes.load(Ordering::SeqCst), 1);
        } else {
            assert!(
                if refusal == "unintroduced" {
                    matches!(result, Err(HerdrLaunchError::GrantUnavailable(_)))
                } else {
                    matches!(result, Err(HerdrLaunchError::Prepare(_)))
                },
                "pending_refuses_before_probe_contract: {refusal}: {result:?}"
            );
            assert_eq!(host.inner.probes.load(Ordering::SeqCst), 0);
            assert_eq!(stored(&pool).await, before);
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn user_default_failed_or_unintroduced_grant_storage_never_mutates_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for unintroduced in [false, true] {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, None).await;
        let fixture = PolicyHost::new(&pool, EndpointMode::UserDefault);
        fixture.unintroduced.store(unintroduced, Ordering::SeqCst);
        fixture.grants.fail_save.store(true, Ordering::SeqCst);
        let host = Arc::new(fixture);
        let result =
            launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
        assert_eq!(
            (
                host.user_creates.load(Ordering::SeqCst),
                host.inner.creates.load(Ordering::SeqCst),
                host.inner.probes.load(Ordering::SeqCst)
            ),
            (0, 0, 0),
            "grant_save_failure_prevents_all_mutation_and_probe_contract: unintroduced={unintroduced}"
        );
        let expected = if unintroduced {
            "launch admission grant store is not introduced"
        } else {
            "injected grant durability failure"
        };
        assert_eq!(
            result,
            Err(HerdrLaunchError::GrantUnavailable(expected.into()))
        );
        assert!(
            matches!(decoded(stored(&pool).await), HostedRecord::Known(row) if row.state == HostedState::Retired && row.location.is_none() && row.expected.is_none())
        );
        assert_eq!(host.grants.loads.load(Ordering::SeqCst), 0);
        assert!(
            host.grants.saved.lock().unwrap().is_none(),
            "unintroduced storage cannot fabricate a grant"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn user_default_failed_pending_commit_never_records_a_grant_or_mutates_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    sqlx::query("CREATE FUNCTION reject_policy_pending() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected pending write failure'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_policy_pending BEFORE UPDATE ON sessions FOR EACH ROW EXECUTE FUNCTION reject_policy_pending()")
        .execute(&pool).await.unwrap();
    let host = Arc::new(PolicyHost::new(&pool, EndpointMode::UserDefault));
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
    assert_eq!(
        (
            host.grants.saves.load(Ordering::SeqCst),
            host.user_creates.load(Ordering::SeqCst),
            host.inner.creates.load(Ordering::SeqCst),
            host.inner.probes.load(Ordering::SeqCst)
        ),
        (0, 0, 0, 0),
        "pending_commit_failure_prevents_grant_mutation_and_probe_contract"
    );
    assert!(
        matches!(result, Err(HerdrLaunchError::Pending(_))),
        "pending_failure_is_returned_contract: {result:?}"
    );
    assert_eq!(stored(&pool).await, None);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_launch_fresh_and_pending_keep_e7_order_and_never_require_policy_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("grants");
    let mut fixture = PolicyHost::new(&pool, EndpointMode::LegacyDedicated);
    fixture.grants.invalid_file_store = Some(
        crate::services::session_host::policy::grant_store::FileGrantStore::new(Some(
            directory.clone(),
        )),
    );
    let host = Arc::new(fixture);
    *host.inner.evidence.lock().unwrap() = Err(EvidenceGap::NoneYet);
    let first = launch_herdr_session(
        &pool,
        launch(Some(endpoint())),
        |incarnation| host.prepare(incarnation),
        host.clone(),
    )
    .await;
    host.assert_no_new_policy();
    assert_eq!(
        *host.events.lock().unwrap(),
        [
            "legacy_e7",
            "prepare_command",
            "legacy_e7",
            "legacy_create",
            "location_record",
            "evidence_probe"
        ],
        "legacy_fresh_e7_prepare_create_probe_order_contract"
    );
    assert!(
        matches!(
            first,
            Ok(HerdrLaunchOutcome::Launched {
                evidence: Err(EvidenceGap::NoneYet),
                ..
            })
        ),
        "legacy_fresh_result_unchanged_contract: {first:?}"
    );
    host.reset_calls();
    *host.inner.evidence.lock().unwrap() = Ok(100);
    let prepared = AtomicUsize::new(0);
    let retry = launch_herdr_session(
        &pool,
        launch(Some(endpoint())),
        |incarnation| {
            prepared.fetch_add(1, Ordering::SeqCst);
            host.prepare(incarnation)
        },
        host.clone(),
    )
    .await;
    host.assert_no_new_policy();
    assert_eq!(
        *host.events.lock().unwrap(),
        ["legacy_e7", "location_record", "evidence_probe"],
        "legacy_pending_only_e7_then_evidence_probe_contract"
    );
    assert_eq!(prepared.load(Ordering::SeqCst), 0);
    assert_eq!(host.inner.creates.load(Ordering::SeqCst), 0);
    assert_eq!(host.user_creates.load(Ordering::SeqCst), 0);
    assert!(
        matches!(
            retry,
            Ok(HerdrLaunchOutcome::Launched {
                evidence: Ok(()),
                ..
            })
        ),
        "legacy_pending_result_unchanged_contract: {retry:?}"
    );
    assert!(
        !directory.exists(),
        "legacy_launch_policy_file_creation_zero_contract"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_launch_keeps_restore_errors_without_proof_or_grant_reads_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for first in [true, false] {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, None).await;
        let mut fixture = PolicyHost::new(&pool, EndpointMode::LegacyDedicated);
        let readings = if first {
            vec![RestoreResume::On]
        } else {
            vec![
                RestoreResume::Off {
                    witness: ServerWitness::for_test(1),
                },
                RestoreResume::On,
            ]
        };
        fixture.inner = fixture.inner.reading(&readings);
        let host = Arc::new(fixture);
        let result =
            launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
        host.assert_no_new_policy();
        assert_eq!(
            result,
            Err(if first {
                HerdrLaunchError::Unsupported(RESTORE_RESUME_NOT_OFF)
            } else {
                HerdrLaunchError::NotSent(RESTORE_RESUME_NOT_OFF.into())
            }),
            "legacy_restore_error_values_unchanged_contract: first={first}"
        );
        assert_eq!(host.inner.creates.load(Ordering::SeqCst), 0);
        assert_eq!(host.user_creates.load(Ordering::SeqCst), 0);
        assert_eq!(host.inner.probes.load(Ordering::SeqCst), 0);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn user_default_occupied_rows_refuse_before_checker_grant_or_prepare_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = owner(CHANNEL);
    for (raw, state) in [
        (
            Some(wire(&pending(&owner, "occupied"))),
            Some(HostedState::Pending),
        ),
        (
            Some(wire(&record(&owner, "occupied", HostedState::Bound))),
            Some(HostedState::Bound),
        ),
        (Some(serde_json::json!({"schema": 2})), None),
    ] {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, raw.clone()).await;
        let host = Arc::new(PolicyHost::new(&pool, EndpointMode::UserDefault));
        let result = launch_herdr_session(
            &pool,
            launch(Some(endpoint())),
            |incarnation| host.prepare(incarnation),
            host.clone(),
        )
        .await;
        assert_eq!(
            result,
            Err(HerdrLaunchError::Occupied(state)),
            "occupied_user_default_row_precedes_policy_contract"
        );
        assert!(
            host.events.lock().unwrap().is_empty(),
            "occupied_user_default_has_no_policy_or_probe_contract"
        );
        assert_eq!(host.permit_calls.load(Ordering::SeqCst), 0);
        assert_eq!(host.grant_accesses.load(Ordering::SeqCst), 0);
        assert_eq!(stored(&pool).await, raw);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn user_default_file_grant_roundtrip_failure_is_unavailable_and_never_creates_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("grants");
    let mut fixture = PolicyHost::new(&pool, EndpointMode::UserDefault);
    fixture.grants.invalid_file_store = Some(
        crate::services::session_host::policy::grant_store::FileGrantStore::new(Some(
            directory.clone(),
        )),
    );
    let host = Arc::new(fixture);
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
    assert!(
        matches!(result, Err(HerdrLaunchError::GrantUnavailable(_))),
        "invalid_file_grant_reaches_unavailable_launch_error_contract: {result:?}"
    );
    assert_eq!(
        (
            host.user_creates.load(Ordering::SeqCst),
            host.inner.creates.load(Ordering::SeqCst),
            host.inner.probes.load(Ordering::SeqCst)
        ),
        (0, 0, 0),
        "invalid_file_grant_creates_and_probes_nothing_contract"
    );
    assert!(
        !directory.exists(),
        "invalid_file_grant_launch_creates_no_files_contract"
    );
    pool.close().await;
    db.drop().await;
}
