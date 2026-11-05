use super::*;
use crate::services::herdr_launch::policy::LaunchGrantStore;
use crate::services::session_host::HerdrEndpoint;
use crate::services::session_host::policy::{
    ConfigChecker, PeerSource, Unverified, file,
    grant_store::FileGrantStore,
    peer::PeerSnapshot,
    permit::{ExistingMutationEvidence, grant_for_test},
};
use std::time::{SystemTime, UNIX_EPOCH};

struct Source {
    peer: PeerSnapshot,
    failure: Mutex<Option<Unverified>>,
    reads: AtomicUsize,
    fail_once_at: AtomicUsize,
    _config: tempfile::NamedTempFile,
}

impl PeerSource for Source {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_once_at.load(Ordering::SeqCst) == read {
            return Err(Unverified::Peer);
        }
        match *self.failure.lock().unwrap() {
            Some(why) => Err(why),
            None => Ok(self.peer.clone()),
        }
    }
}

struct CountChecker {
    calls: AtomicUsize,
    result: Result<(), Unverified>,
}

#[async_trait::async_trait]
impl ConfigChecker for CountChecker {
    async fn check(&self, _: &PeerSnapshot) -> Result<(), Unverified> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result
    }
}

struct PolicyCase {
    _registry: Box<dyn std::any::Any>,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _intake: IntakeGate,
    case: Case,
    endpoint: HerdrEndpoint,
    source: Arc<Source>,
    checker: Arc<CountChecker>,
    original_grant: Option<Vec<u8>>,
    _grants: tempfile::TempDir,
}

impl PolicyCase {
    fn new(
        tag: &str,
        granted: Option<u32>,
        postdated: bool,
        failing: bool,
        cache_error: bool,
    ) -> Self {
        Self::mode(
            tag,
            granted,
            postdated,
            failing,
            cache_error,
            crate::config::session_hosts::EndpointMode::UserDefault,
        )
    }

    fn mode(
        tag: &str,
        granted: Option<u32>,
        postdated: bool,
        failing: bool,
        cache_error: bool,
        mode: crate::config::session_hosts::EndpointMode,
    ) -> Self {
        let case = Case::launched(tag);
        let intake = IntakeGate::configure(&case);
        let mut config = (*crate::config_live_reload::current().unwrap()).clone();
        let configured = config
            .session_hosts
            .herdr
            .endpoints
            .values_mut()
            .next()
            .unwrap();
        configured.mode = mode;
        configured.socket_path = case.fx.rig.socket().into();
        let hosts = crate::config::session_hosts::force_config_for_test(&config);
        let configured = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
        let endpoint = HerdrEndpoint::new(
            &configured.execution_node,
            &configured.key,
            &configured.socket_path,
            &configured.herdr_session,
        )
        .unwrap()
        .with_mode(mode);
        let HostedRecord::Known(mut record) = case.fx.record() else {
            panic!("launched N1")
        };
        record.location.as_mut().unwrap().socket_addr =
            configured.socket_path.display().to_string();
        case.fx.store(&record);
        let launch = HerdrLaunchEndpoint {
            execution_node: configured.execution_node,
            config_key: configured.key,
            socket_addr: configured.socket_path.display().to_string(),
            herdr_session: configured.herdr_session,
        };
        let witness = |pid| ServerWitness {
            socket: endpoint.socket_path().into(),
            pid,
            start: crate::services::session_host::StartIdentity::Darwin {
                seconds: 1_000,
                micros: 0,
            },
        };
        let policy =
            tempfile::NamedTempFile::new_in(endpoint.socket_path().parent().unwrap()).unwrap();
        std::fs::write(policy.path(), "[session]\nresume_agents_on_restore=false\n").unwrap();
        let source = Arc::new(Source {
            peer: PeerSnapshot {
                witness: witness(7),
                started: if postdated {
                    UNIX_EPOCH + Duration::from_secs(1_000)
                } else {
                    SystemTime::now() + Duration::from_secs(60)
                },
                home: policy.path().parent().unwrap().into(),
                config: policy.path().into(),
                checker: policy.path().into(),
                image: file::image(policy.path()).unwrap(),
                socket_dir: file::directory_identity(policy.path().parent().unwrap()).unwrap(),
            },
            failure: Mutex::new(None),
            reads: AtomicUsize::new(0),
            fail_once_at: AtomicUsize::new(0),
            _config: policy,
        });
        let checker = Arc::new(CountChecker {
            calls: AtomicUsize::new(0),
            result: if failing {
                Err(Unverified::Diagnostics)
            } else {
                Ok(())
            },
        });
        let grants = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            grants.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let store = FileGrantStore::new(Some(grants.path().into()));
        let original_grant = granted.map(|pid| {
            let grant = grant_for_test(&launch, &case.nonce, witness(pid));
            case.fx.rt.block_on(store.save(grant.clone())).unwrap();
            grant.encode().unwrap()
        });
        let restored = case.fx.rt.block_on(store.load_all(&[launch])).unwrap();
        let registry = case
            .fx
            .rig
            .policy_registry_on_this_thread(endpoint.clone(), |host| {
                let host = host
                    .with_restored_for_test(restored)
                    .with_policy_source_for_test(source.clone())
                    .with_policy_checker_for_test(checker.clone());
                if cache_error {
                    host.with_prepared_for_test(Err(Unverified::Cache))
                } else {
                    host
                }
            });
        let this = Self {
            _registry: Box::new(registry),
            _hosts: hosts,
            _intake: intake,
            case,
            endpoint,
            source,
            checker,
            original_grant,
            _grants: grants,
        };
        if !cache_error {
            let evidence = this.evidence();
            assert!(
                evidence.startup.is_none(),
                "a new dcserver starts with no startup proof"
            );
            assert_eq!(
                evidence.grant.map(|g| g.encode().unwrap()),
                this.original_grant
            );
        }
        assert_eq!(this.checker.calls.load(Ordering::SeqCst), 0);
        this
    }

    fn pass(&self) {
        assert!(
            self.case
                .shared
                .restart
                .reconcile_done
                .load(Ordering::Acquire)
        );
        let _runtime = self.case.fx.rt.enter();
        reconnect_restarted_herdr_with_shared(&self.case.shared, &ProviderKind::Claude);
    }

    fn evidence(&self) -> ExistingMutationEvidence {
        herdr_endpoints()
            .launch_host()
            .unwrap()
            .mutation_evidence(&self.endpoint, &self.case.nonce)
            .unwrap()
    }

    fn result(&self) -> recovery::Recovery {
        use crate::db::dispatched_sessions::hosted_execution::{
            HostedLookup, HostedLookupKey, load_hosted_execution_pg,
        };
        let key = super::super::super::session_key(&self.case.fx.owner);
        let HostedLookup::Found(row) = self.case.fx.rt.block_on(load_hosted_execution_pg(
            &self.case.fx.pool,
            HostedLookupKey::SessionKey(&key),
        )) else {
            panic!("exact fixture row");
        };
        recovery::result_for_test(&row).expect("the production reader records its recovery result")
    }

    fn assert_bound(&self, reason: &str, before: &[Value]) {
        assert_eq!(self.case.fx.row(), Some(HostedState::Bound));
        assert!(
            matches!(self.result(), recovery::Recovery::Held(ref why) if why.contains(reason)),
            "{}: {:?}",
            reason,
            self.result()
        );
        assert_eq!(self.case.cleanups(), 0);
        assert_eq!(
            self.case.effects(),
            before,
            "held recovery creates/writes/closes no pane"
        );
    }
}

fn reader_produces_current_policy(tag: &str, granted: Option<u32>) {
    let case = PolicyCase::new(tag, granted, false, false, false);
    let before = case.case.effects();
    case.pass();
    assert!(
        ATTACHES.with(|count| count.get() > 0),
        "the production reader observes RootReplaced"
    );
    let result = case.result();
    if case.case.fx.row() == Some(HostedState::Bound) {
        assert!(
            matches!(result, recovery::Recovery::Held(ref why)
            if why.contains("PolicyUnverified(Identity)")),
            "the unmodified entry stops specifically at policy admission: {result:?}"
        );
    }
    assert_eq!(
        result,
        recovery::Recovery::Retired,
        "the reader must produce current startup policy before constructing the strict target"
    );
    assert_eq!(case.case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.case.cleanups(), 1);
    assert!(!case.case.hold_path().exists());
    assert_eq!(
        case.case.effects(),
        before,
        "reader recovery creates/writes/closes no pane"
    );
    let evidence = case.evidence();
    assert!(
        evidence.startup.is_some(),
        "only the production observation produces startup proof"
    );
    assert_eq!(
        evidence.grant.map(|g| g.encode().unwrap()),
        case.original_grant,
        "observing W1 never rewrites N1's historical W0 grant"
    );
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn rr_policy_reader_observes_rebooted_server_with_old_grant_pg() {
    reader_produces_current_policy("rr-policy-old-grant", Some(8));
}

#[test]
fn rr_policy_reader_observes_rebooted_server_without_grant_pg() {
    reader_produces_current_policy("rr-policy-no-grant", None);
}

#[test]
fn rr_policy_postdated_config_stays_bound_and_never_creates_pg() {
    let case = PolicyCase::new("rr-policy-postdated", Some(8), true, false, false);
    let before = case.case.effects();
    case.pass();
    case.assert_bound("PolicyUnverified(Identity)", &before);
    assert!(
        case.evidence().startup.is_none(),
        "next-restart proof cannot admit this server"
    );
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn rr_policy_postdated_failed_check_is_observed_once_in_three_passes_pg() {
    let case = PolicyCase::new("rr-policy-retries", Some(8), true, true, false);
    let before = case.case.effects();
    for _ in 0..3 {
        case.pass();
        assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    }
    assert_eq!(
        case.checker.calls.load(Ordering::SeqCst),
        1,
        "the same endpoint/witness/file token must not rerun a failing checker every pass"
    );
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(case.case.effects(), before);
}

#[test]
fn rr_policy_peer_read_failure_is_not_replaced_by_missing_proof_pg() {
    let case = PolicyCase::new("rr-policy-peer", Some(8), false, false, false);
    *case.source.failure.lock().unwrap() = Some(Unverified::Peer);
    let before = case.case.effects();
    case.pass();
    case.assert_bound("Peer", &before);
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn rr_policy_transient_producer_peer_failure_retries_on_next_reader_pass_pg() {
    let case = PolicyCase::new("rr-policy-transient-peer", Some(8), false, false, false);
    case.source.fail_once_at.store(2, Ordering::SeqCst);
    let before = case.case.effects();
    case.pass();
    case.assert_bound("Peer", &before);
    assert_eq!(
        case.source.reads.load(Ordering::SeqCst),
        2,
        "preflight succeeds before the producer's first snapshot fails once"
    );
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 0);
    assert!(case.evidence().startup.is_none());

    case.pass();
    assert_eq!(
        case.case.fx.row(),
        Some(HostedState::Retired),
        "the next reader pass retries a transient producer failure"
    );
    assert_eq!(case.result(), recovery::Recovery::Retired);
    assert_eq!(case.case.cleanups(), 1);
    assert!(!case.case.hold_path().exists());
    assert_eq!(case.case.effects(), before);
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 1);
    let evidence = case.evidence();
    assert!(evidence.startup.is_some());
    assert_eq!(
        evidence.grant.map(|g| g.encode().unwrap()),
        case.original_grant,
    );
}

#[test]
fn rr_policy_grant_cache_failure_is_not_repaired_by_observation_pg() {
    let case = PolicyCase::new("rr-policy-cache", Some(8), false, false, true);
    let before = case.case.effects();
    case.pass();
    case.assert_bound("Cache", &before);
    assert_eq!(
        case.checker.calls.load(Ordering::SeqCst),
        0,
        "a failed grant restoration remains fail-closed before policy observation"
    );
    assert_eq!(case.source.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn rr_policy_same_root_live_execution_never_observes_or_retires_pg() {
    let case = PolicyCase::new("rr-policy-same-root", Some(7), false, false, false);
    case.case
        .fx
        .rig
        .run_provider(&context_of(&case.case.nonce), false);
    case.case.fx.rig.foreground(&[PROVIDER]);
    case.case.fx.rig.exists(SHELL, true);
    case.case.fx.rig.exists(PROVIDER, true);
    let before = case.case.effects();
    case.pass();
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 0);
    assert_eq!(case.source.reads.load(Ordering::SeqCst), 0);
    assert_eq!(case.case.effects(), before);
}

#[test]
fn rr_policy_exact_current_grant_survives_invalid_config_without_observation_pg() {
    let case = PolicyCase::new("rr-policy-exact-grant", Some(7), false, false, false);
    std::fs::write(
        &case.source.peer.config,
        "[session]\nresume_agents_on_restore=true\n",
    )
    .unwrap();
    let before = case.case.effects();
    case.pass();
    assert_eq!(case.result(), recovery::Recovery::Retired);
    assert_eq!(case.case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 0);
    assert_eq!(case.source.reads.load(Ordering::SeqCst), 0);
    assert_eq!(case.case.effects(), before);
    assert_eq!(
        case.evidence().grant.map(|g| g.encode().unwrap()),
        case.original_grant,
    );
}

#[test]
fn rr_policy_original_provider_alive_never_retires_pg() {
    let case = PolicyCase::new("rr-policy-live-old", Some(8), false, false, false);
    case.case.fx.rig.exists(PROVIDER, true);
    let before = case.case.effects();
    case.pass();
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.case.cleanups(), 0);
    assert_eq!(case.case.effects(), before);
}

#[test]
fn rr_policy_legacy_dedicated_retires_without_policy_observation_pg() {
    let case = PolicyCase::mode(
        "rr-policy-legacy",
        None,
        false,
        false,
        false,
        crate::config::session_hosts::EndpointMode::LegacyDedicated,
    );
    let before = case.case.effects();
    case.pass();
    assert_eq!(case.result(), recovery::Recovery::Retired);
    assert_eq!(case.case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.checker.calls.load(Ordering::SeqCst), 0);
    assert_eq!(case.source.reads.load(Ordering::SeqCst), 0);
    assert_eq!(case.case.effects(), before);
}
