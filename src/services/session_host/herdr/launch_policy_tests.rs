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
