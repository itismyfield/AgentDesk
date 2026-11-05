use super::*;
use crate::config::session_hosts::EndpointMode;
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::herdr_launch::policy::UserDefaultCreateRequest;
use crate::services::session_host::herdr::policy::{
    ConfigChecker, PeerSource, ProofCache, Unverified, file, peer::PeerSnapshot,
    permit::CreatePermit,
};

struct Source {
    peer: PeerSnapshot,
    serving: Arc<AtomicU32>,
}

impl PeerSource for Source {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        let mut peer = self.peer.clone();
        let pid = self.serving.load(Ordering::SeqCst);
        peer.witness.pid = pid;
        peer.witness.start = start_of(pid).identity;
        Ok(peer)
    }
}

struct Checker;
#[async_trait::async_trait]
impl ConfigChecker for Checker {
    async fn check(&self, _: &PeerSnapshot) -> Result<(), Unverified> {
        Ok(())
    }
}

async fn request(
    server: &Server,
    host: &mut SocketHerdrLaunchHost,
    home: &Path,
    config: &Path,
    cwd: &Path,
) -> UserDefaultCreateRequest {
    let image = file::read(config).unwrap().token;
    let source = Arc::new(Source {
        peer: PeerSnapshot {
            witness: ServerWitness {
                socket: server.path.clone(),
                pid: 7,
                start: start_of(7).identity,
            },
            started: SystemTime::now(),
            home: home.into(),
            config: config.into(),
            checker: config.into(),
            image,
            socket_dir: file::directory_identity(server.path.parent().unwrap()).unwrap(),
        },
        serving: server.serving.clone(),
    });
    let proof = ProofCache::default()
        .next_restart(source.clone(), &Checker)
        .await
        .unwrap();
    host.sources = vec![source];
    host.endpoints[0].0 = host.endpoints[0]
        .0
        .clone()
        .with_mode(EndpointMode::UserDefault);
    let endpoint = launch_endpoint(server);
    UserDefaultCreateRequest {
        permit: CreatePermit::new(endpoint.clone(), proof).unwrap(),
        endpoint,
        label: "AgentDesk-user-policy".into(),
        cwd: cwd.into(),
        command: "bash /run/launch.sh".into(),
    }
}

fn config(home: &Path) -> PathBuf {
    let path = home.join("config.toml");
    std::fs::write(&path, CANONICAL_CONFIG).unwrap();
    path
}

#[tokio::test]
async fn policy_fence_refuses_file_change_before_workspace_create() {
    let (dir, cwd) = workdir();
    let config = config(dir.path());
    let server = serve(created(cwd.display().to_string()));
    let mut host = launch_host(&server);
    let request = request(&server, &mut host, dir.path(), &config, &cwd).await;
    std::fs::write(&config, b"[session]\nresume_agents_on_restore=true\n").unwrap();
    let spy = SpyGuard::install(SpyState::default());
    let outcome = host.create_user_default(&request);
    assert!(
        matches!(outcome, HerdrCreateOutcome::NotSent(ref reason) if reason.contains("policy fence")),
        "changed file must refuse before create: {outcome:?}"
    );
    assert!(
        server.methods().is_empty(),
        "no create or command before final token fence"
    );
    assert!(
        spy.calls().is_empty(),
        "policy refusal cannot fall back to tmux"
    );
}

#[tokio::test]
async fn policy_fence_refuses_witness_change_before_workspace_create() {
    let (dir, cwd) = workdir();
    let config = config(dir.path());
    let server = serve(created(cwd.display().to_string()));
    let mut host = launch_host(&server);
    let request = request(&server, &mut host, dir.path(), &config, &cwd).await;
    server.serving.store(8, Ordering::SeqCst);
    let outcome = host.create_user_default(&request);
    assert!(
        matches!(outcome, HerdrCreateOutcome::NotSent(ref reason) if reason.contains("policy fence")),
        "changed witness must refuse before create: {outcome:?}"
    );
    assert!(
        server.methods().is_empty(),
        "no create after current witness changed"
    );
}

#[tokio::test]
async fn policy_fence_refuses_command_after_workspace_reply_changes_file() {
    let (dir, cwd) = workdir();
    let config = config(dir.path());
    let changed = config.clone();
    let reported = cwd.display().to_string();
    let server = serve(Box::new(move |call, _| match call["method"].as_str() {
        Some("workspace.create") => {
            std::fs::write(&changed, b"[session]\nresume_agents_on_restore=true\n").unwrap();
            json!({"type":"workspace_created","root_pane":{"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","revision":0,"cwd":reported}})
        }
        _ => json!({"type":"ok"}),
    }));
    let mut host = launch_host(&server);
    let request = request(&server, &mut host, dir.path(), &config, &cwd).await;
    let spy = SpyGuard::install(SpyState::default());
    let outcome = host.create_user_default(&request);
    assert!(
        matches!(outcome, HerdrCreateOutcome::CreatedUnconfirmed { ref pane_id, ref detail } if pane_id == "w1:p1" && detail.contains("policy fence")),
        "created pane stays uncertain and is never recreated: {outcome:?}"
    );
    assert_eq!(
        server.methods(),
        ["workspace.create"],
        "no provider command after token changed"
    );
    assert!(
        spy.calls().is_empty(),
        "uncertain created pane cannot fall back to tmux"
    );
}

#[tokio::test]
async fn policy_fence_refuses_command_after_workspace_reply_changes_witness() {
    let (dir, cwd) = workdir();
    let config = config(dir.path());
    let server = serve(created_then(cwd.display().to_string(), |serving| {
        serving.store(8, Ordering::SeqCst);
    }));
    let mut host = launch_host(&server);
    let request = request(&server, &mut host, dir.path(), &config, &cwd).await;
    let outcome = host.create_user_default(&request);
    assert!(
        matches!(outcome, HerdrCreateOutcome::CreatedUnconfirmed { ref pane_id, ref detail } if pane_id == "w1:p1" && detail.contains("policy fence")),
        "new server cannot receive provider command: {outcome:?}"
    );
    assert_eq!(
        server.methods(),
        ["workspace.create"],
        "provider command stays unsent after witness changed"
    );
}

#[tokio::test]
async fn policy_fence_unchanged_evidence_creates_and_sends_once_without_e7() {
    let (dir, cwd) = workdir();
    let config = config(dir.path());
    let server = serve(created(cwd.display().to_string()));
    let mut host = launch_host(&server);
    let request = request(&server, &mut host, dir.path(), &config, &cwd).await;
    assert_eq!(
        host.create_user_default(&request),
        HerdrCreateOutcome::Created {
            pane_id: "w1:p1".into()
        }
    );
    assert_eq!(
        server.methods(),
        ["workspace.create", "pane.send_input"],
        "UserDefault must not invoke startup-Off E7"
    );
}

#[test]
fn policy_endpoint_keys_do_not_alias_and_missing_source_is_a_refusal() {
    let server = serve(created("/tmp".into()));
    let mut host = launch_host(&server);
    let endpoint = host.endpoints[0].0.clone();
    let grant = crate::services::session_host::policy::permit::grant_for_test(
        &launch_endpoint(&server),
        "same-nonce",
        ServerWitness {
            socket: server.path.clone(),
            pid: 7,
            start: start_of(7).identity,
        },
    );
    host.remember_grant(grant);
    assert!(
        host.mutation_evidence(&endpoint, "same-nonce")
            .unwrap()
            .grant
            .is_some()
    );
    let other = HerdrEndpoint::new(
        endpoint.execution_node(),
        "other-key",
        endpoint.socket_path(),
        endpoint.herdr_session(),
    )
    .unwrap();
    assert!(
        host.mutation_evidence(&other, "same-nonce")
            .unwrap()
            .grant
            .is_none(),
        "grant_keys_include_whole_endpoint_contract"
    );
    host.sources.clear();
    assert!(
        host.source(&launch_endpoint(&server)).is_none(),
        "missing_source_is_not_an_index_panic_contract"
    );
}

#[test]
fn policy_remembered_grant_bypasses_only_its_boot_restore_failure() {
    let server = serve(created("/tmp".into()));
    let host = launch_host(&server).with_prepared_for_test(Err(Unverified::Cache));
    let configured = host.endpoints[0].0.clone();
    let endpoint = launch_endpoint(&server);
    let grant = crate::services::session_host::policy::permit::grant_for_test(
        &endpoint,
        "remembered",
        ServerWitness {
            socket: server.path.clone(),
            pid: 7,
            start: start_of(7).identity,
        },
    );
    assert!(
        matches!(
            host.mutation_evidence(&configured, "remembered"),
            Err(Unverified::Cache)
        ),
        "boot_restore_failure_initially_refuses_contract"
    );
    host.remember_grant(grant.clone());
    let evidence = host.mutation_evidence(&configured, "remembered");
    assert!(
        matches!(evidence, Ok(ref evidence) if evidence.grant == Some(grant)),
        "remembered_grant_survives_boot_error_contract"
    );
    assert!(
        matches!(
            host.mutation_evidence(&configured, "other-nonce"),
            Err(Unverified::Cache)
        ),
        "boot_error_still_blocks_unremembered_nonce_contract"
    );
    assert!(
        server.methods().is_empty(),
        "remembered_grant_evidence_reads_no_socket_contract"
    );
}

#[test]
fn policy_boot_restore_errors_are_endpoint_scoped_and_cache_poison_remains_closed() {
    use crate::services::session_host::policy::grant_store::GrantRestoration;
    let server = serve(created("/tmp".into()));
    let endpoint = launch_endpoint(&server);
    let mut other = endpoint.clone();
    other.config_key = "another-endpoint".into();
    let restored = GrantRestoration {
        by_endpoint: [(other, Err(Unverified::Cache))].into_iter().collect(),
        unknown_failure: false,
    };
    let host = launch_host(&server).with_restored_for_test(restored);
    let configured = host.endpoints[0].0.clone();
    assert!(
        matches!(host.mutation_evidence(&configured, "missing"), Ok(ref evidence) if evidence.grant.is_none()),
        "foreign_endpoint_restore_error_does_not_block_current_contract"
    );

    for poison in ["grants", "witnessed", "proof"] {
        let host = launch_host(&server).with_prepared_for_test(Err(Unverified::Cache));
        host.seed_witness_for_test(
            &configured,
            ServerWitness {
                socket: server.path.clone(),
                pid: 7,
                start: start_of(7).identity,
            },
        );
        host.remember_grant(
            crate::services::session_host::policy::permit::grant_for_test(
                &endpoint,
                "remembered",
                ServerWitness {
                    socket: server.path.clone(),
                    pid: 7,
                    start: start_of(7).identity,
                },
            ),
        );
        match poison {
            "grants" => host.poison_grants_for_test(),
            "witnessed" => host.poison_witnessed_for_test(),
            _ => host.poison_proof_for_test(),
        }
        assert!(
            matches!(
                host.mutation_evidence(&configured, "remembered"),
                Err(Unverified::Cache)
            ),
            "remembered_grant_does_not_hide_poisoned_cache_contract: {poison}"
        );
    }
    assert!(
        server.methods().is_empty(),
        "endpoint_restore_errors_probe_zero_contract"
    );
}

struct CountChecker {
    calls: std::sync::atomic::AtomicUsize,
    result: Result<(), Unverified>,
}

#[async_trait::async_trait]
impl ConfigChecker for CountChecker {
    async fn check(&self, _: &PeerSnapshot) -> Result<(), Unverified> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result
    }
}

struct PreparingSource {
    peer: PeerSnapshot,
    failure: AtomicBool,
    reads: std::sync::atomic::AtomicUsize,
}

impl PeerSource for PreparingSource {
    fn snapshot(&self) -> Result<PeerSnapshot, Unverified> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.failure.load(Ordering::SeqCst) {
            Err(Unverified::Peer)
        } else {
            Ok(self.peer.clone())
        }
    }
}

struct ExistingPolicyCase {
    home: tempfile::TempDir,
    host: SocketHerdrLaunchHost,
    source: Arc<PreparingSource>,
    checker: Arc<CountChecker>,
}

impl ExistingPolicyCase {
    fn new(server: &Server, result: Result<(), Unverified>) -> Self {
        let (home, cwd) = workdir();
        let config = config(&cwd);
        let checker_path = cwd.join("checker");
        std::fs::write(&checker_path, b"checker-image").unwrap();
        let source = Arc::new(PreparingSource {
            peer: PeerSnapshot {
                witness: ServerWitness {
                    socket: server.path.clone(),
                    pid: 7,
                    start: start_of(7).identity,
                },
                started: SystemTime::UNIX_EPOCH,
                home: cwd,
                config,
                checker: checker_path.clone(),
                image: file::image(&checker_path).unwrap(),
                socket_dir: file::directory_identity(server.path.parent().unwrap()).unwrap(),
            },
            failure: AtomicBool::new(false),
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let checker = Arc::new(CountChecker {
            calls: std::sync::atomic::AtomicUsize::new(0),
            result,
        });
        let mut host = launch_host(server)
            .with_prepared_for_test(Ok(Vec::new()))
            .with_policy_source_for_test(source.clone())
            .with_policy_checker_for_test(checker.clone());
        host.endpoints[0].0 = host.endpoints[0]
            .0
            .clone()
            .with_mode(EndpointMode::UserDefault);
        Self {
            home,
            host,
            source,
            checker,
        }
    }

    fn rewrite_config(&self, text: &[u8]) {
        std::fs::write(self.home.path().join("config.toml"), text).unwrap();
    }

    fn checks(&self) -> usize {
        self.checker.calls.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn prepare_existing_policy_remembers_failed_check_for_postdated_config() {
    let server = serve(created("/tmp".into()));
    let case = ExistingPolicyCase::new(&server, Err(Unverified::Checker));
    for _ in 0..3 {
        assert_eq!(
            case.host
                .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
                .await,
            Err(Unverified::Checker),
            "same failed observation remains unverified"
        );
    }
    assert_eq!(
        case.checks(),
        1,
        "one checker call for one witness/file token"
    );
    assert!(
        case.host
            .mutation_evidence(&case.host.endpoints[0].0, "old-nonce")
            .unwrap()
            .startup
            .is_none(),
        "a failed observation cannot produce startup evidence"
    );
}

#[tokio::test]
async fn prepare_existing_policy_retries_when_config_token_changes() {
    let server = serve(created("/tmp".into()));
    let case = ExistingPolicyCase::new(&server, Err(Unverified::Checker));
    assert_eq!(
        case.host
            .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
            .await,
        Err(Unverified::Checker)
    );
    case.rewrite_config(b"[session]\nresume_agents_on_restore=false\n# changed token\n");
    assert_eq!(
        case.host
            .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
            .await,
        Err(Unverified::Checker)
    );
    assert_eq!(
        case.checks(),
        2,
        "a new file token permits a new observation"
    );
}

#[tokio::test]
async fn prepare_existing_policy_does_not_promote_postdated_next_restart_proof() {
    let server = serve(created("/tmp".into()));
    let case = ExistingPolicyCase::new(&server, Ok(()));
    assert_eq!(
        case.host
            .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
            .await,
        Ok(())
    );
    let evidence = case
        .host
        .mutation_evidence(&case.host.endpoints[0].0, "old-nonce")
        .unwrap();
    assert_eq!(case.checks(), 1, "the current config was checked");
    assert!(
        evidence.startup.is_none(),
        "a postdated file proves no startup policy"
    );
    assert!(matches!(
        super::super::ExistingMutationPermit::issue(
            &launch_endpoint(&server),
            "old-nonce",
            case.source.peer.witness.clone(),
            &evidence,
        ),
        Err(Unverified::Identity)
    ));
}

#[tokio::test]
async fn prepare_existing_policy_keeps_exact_grant_after_config_becomes_invalid() {
    let server = serve(created("/tmp".into()));
    let mut case = ExistingPolicyCase::new(&server, Err(Unverified::Checker));
    let endpoint = launch_endpoint(&server);
    let grant = crate::services::session_host::policy::permit::grant_for_test(
        &endpoint,
        "old-nonce",
        case.source.peer.witness.clone(),
    );
    case.host = case.host.with_prepared_for_test(Ok(vec![grant.clone()]));
    case.rewrite_config(b"invalid config");
    assert_eq!(
        case.host
            .prepare_existing_policy(&endpoint, "old-nonce")
            .await,
        Ok(())
    );
    assert_eq!(
        case.checks(),
        0,
        "historical permission needs no config checker"
    );
    assert_eq!(
        case.host
            .mutation_evidence(&case.host.endpoints[0].0, "old-nonce")
            .unwrap()
            .grant,
        Some(grant),
        "the exact admitted grant remains unchanged"
    );
}

#[tokio::test]
async fn prepare_existing_policy_skips_legacy_endpoints_without_peer_reads() {
    let server = serve(created("/tmp".into()));
    let mut case = ExistingPolicyCase::new(&server, Err(Unverified::Checker));
    case.host.endpoints[0].0 = case.host.endpoints[0]
        .0
        .clone()
        .with_mode(EndpointMode::LegacyDedicated);
    case.source.failure.store(true, Ordering::SeqCst);
    assert_eq!(
        case.host
            .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
            .await,
        Ok(())
    );
    assert_eq!(case.checks(), 0, "legacy policy is not observed");
    assert_eq!(
        case.source.reads.load(Ordering::SeqCst),
        0,
        "legacy mode returns before source reads"
    );
}

#[tokio::test]
async fn prepare_existing_policy_preserves_peer_read_failure() {
    let server = serve(created("/tmp".into()));
    let case = ExistingPolicyCase::new(&server, Ok(()));
    case.source.failure.store(true, Ordering::SeqCst);
    assert_eq!(
        case.host
            .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
            .await,
        Err(Unverified::Peer)
    );
    assert_eq!(
        case.checks(),
        0,
        "an unreadable peer cannot reach the checker"
    );
}

#[tokio::test]
async fn prepare_existing_policy_preserves_restore_and_poisoned_cache_errors() {
    let server = serve(created("/tmp".into()));
    for failure in ["restore", "grants", "witnessed", "proof"] {
        let mut case = ExistingPolicyCase::new(&server, Ok(()));
        match failure {
            "restore" => {
                case.host = case.host.with_prepared_for_test(Err(Unverified::Cache));
            }
            "grants" => case.host.poison_grants_for_test(),
            "witnessed" => case.host.poison_witnessed_for_test(),
            _ => {
                case.host.seed_witness_for_test(
                    &case.host.endpoints[0].0,
                    case.source.peer.witness.clone(),
                );
                case.host.poison_proof_for_test();
            }
        }
        assert_eq!(
            case.host
                .prepare_existing_policy(&launch_endpoint(&server), "old-nonce")
                .await,
            Err(Unverified::Cache),
            "cache failure is not missing proof: {failure}"
        );
        assert_eq!(
            case.checks(),
            0,
            "cache errors cannot trigger new observations: {failure}"
        );
    }
}
