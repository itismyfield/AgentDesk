use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::turn_presence::activity::tests::BindingRoot;
use crate::services::platform::tmux::session_inventory::tests::ChannelProbeTrace;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(in crate::services::discord::turn_presence::activity) struct Rig {
    pub(in crate::services::discord::turn_presence::activity) shared: Arc<SharedData>,
    pub(in crate::services::discord::turn_presence::activity) channel: ChannelId,
    config: Config,
    pub(in crate::services::discord::turn_presence::activity) root: tempfile::TempDir,
    _binding: BindingRoot,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _namespace: crate::config::TestEnvVarGuard,
    _root_env: crate::config::TestEnvVarGuard,
    _db: TestPostgresDb,
    _live: crate::config_live_reload::TestLiveGuard,
}
impl Rig {
    pub(in crate::services::discord::turn_presence::activity) async fn new(
        id: u64,
        hosted: bool,
        retired: bool,
    ) -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let root_env = set("AGENTDESK_ROOT_DIR", root.path());
        let namespace = set("TMUX_TMPDIR", root.path());
        let target = [(id, "test-node")];
        let hosts = crate::config::session_hosts::force_for_test(
            Some("test-node"),
            if hosted { &target } else { &[] },
        );
        let mut config: Config = serde_yaml::from_str(&format!("server: {{}}\ncluster: {{instance_id: test-node}}\nagents:\n  - id: fixture\n    name: fixture\n    provider: claude\n    channels:\n      claude: {{id: '{id}', name: s1e-{id}, runtime: tui}}\n")).unwrap();
        config.session_hosts =
            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
        crate::config_live_reload::install(config.clone());
        let config_path = crate::runtime_layout::config_file_path(root.path());
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
        let channel = ChannelId::new(id);
        crate::services::discord::host_defer_gate::tests::map_channel(
            &shared,
            channel,
            &format!("s1e-{id}"),
        )
        .await;
        let binding = BindingRoot::enter(root.path());
        let rig = Self {
            shared,
            channel,
            config,
            root,
            _binding: binding,
            _hosts: hosts,
            _live: live,
            _namespace: namespace,
            _root_env: root_env,
            _db: db,
        };
        if retired {
            rig.seed_retired().await;
        }
        rig
    }
    fn name(&self) -> String {
        ProviderKind::Claude.build_tmux_session_name(&format!("s1e-{}", self.channel))
    }
    fn watch(&self) -> Arc<Mutex<Watch>> {
        Arc::new(Mutex::new(Watch::default()))
    }
    fn ports(&self) -> Arc<LivePorts> {
        Arc::new(LivePorts {
            root: Some(self.root.path().to_owned()),
        })
    }
    async fn seed_retired(&self) {
        use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, tests::record};
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: self.shared.token_hash.clone(),
            channel_id: self.channel.to_string(),
            logical_key: self.name(),
            owner_node: "test-node".into(),
            runtime_root: self.root.path().display().to_string(),
        };
        let boot = BootSessionHosts::from_config(&self.config).unwrap();
        let endpoint = boot.herdr_endpoint(self.channel.get()).unwrap();
        let mut execution = record(&owner, "retired", HostedState::Retired);
        execution.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: endpoint.execution_node.clone(),
            endpoint_config_key: endpoint.key.clone(),
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session.clone(),
            pane_id: "p-1".into(),
        });
        let key = adk_session::build_namespaced_session_key(
            &self.shared.token_hash,
            &ProviderKind::Claude,
            &self.name(),
        );
        inflight::seed_session_row_hashed(
            self.shared.pg_pool.as_ref().unwrap(),
            &key,
            self.channel.get(),
            &self.shared.token_hash,
            Some(serde_json::to_value(execution).unwrap()),
        )
        .await;
    }
    fn history(&self, name: &str) {
        use crate::services::tui_prompt_dedupe::binding_events as p5;
        let path = self.root.path().join("old.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("parent", &path).unwrap();
        let now = chrono::Utc::now();
        let event = p5::BindingEvent {
            seq: 1,
            channel_id: self.channel.get(),
            provider: "claude".into(),
            tmux_session: name.into(),
            execution_nonce: Some("old".into()),
            old: None,
            new: p5::BindingTarget::Source(source),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: now,
            },
            committed_at: now,
        };
        let dir = self.root.path().join(p5::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.log", self.channel)),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
    }
}
async fn publish(stamp: &Stamp, watch: &Mutex<Watch>) -> Option<()> {
    let calls = AtomicUsize::new(0);
    let result = stamp
        .publish(watch, || {
            calls.fetch_add(1, Ordering::SeqCst);
        })
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        usize::from(result.is_some()),
        "publication callback disagrees with accepted result"
    );
    result
}

#[derive(Clone, Copy, Debug)]
enum Changed {
    Marker,
    Inflight,
    Fallback,
    Exact,
    Canonical,
}
async fn changed_publication(kind: Changed, id: u64) {
    let rig = Rig::new(id, false, false).await;
    let count = ChannelProbeTrace::new(id);
    let watch = rig.watch();
    let (answer, stamp) = read(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    assert_eq!(
        answer,
        observed(Activity::Idle, "no_session"),
        "positive absence failed: {kind:?}"
    );
    assert!(count.calls() > 0, "successful probe was not observed");
    assert!(
        stamp.snapshot.is_some(),
        "Unknown error stamp is not an absence control"
    );
    assert!(publish(&stamp, &watch).await.is_some());
    match kind {
        Changed::Marker => {
            let path = marker_path(&rig.name(), "host_kind");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "process").unwrap();
        }
        Changed::Inflight => {
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &ProviderKind::Claude,
                id,
            );
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "corrupt").unwrap();
        }
        Changed::Fallback => {
            let mut config = rig.config.clone();
            let mut channel = config.agents[0].channels.get("claude").unwrap().clone();
            match &mut channel {
                crate::config::AgentChannel::Detailed(c) => c.name = Some("D".into()),
                _ => panic!("detailed fixture"),
            };
            config.agents[0].channels.insert("claude", channel);
            std::fs::write(
                crate::runtime_layout::config_file_path(rig.root.path()),
                serde_yaml::to_string(&config).unwrap(),
            )
            .unwrap();
        }
        Changed::Exact | Changed::Canonical => {
            let name = if matches!(kind, Changed::Exact) {
                rig.name()
            } else {
                ProviderKind::Claude.build_tmux_session_name("other")
            };
            let key = adk_session::build_namespaced_session_key(
                &rig.shared.token_hash,
                &ProviderKind::Claude,
                &name,
            );
            inflight::seed_session_row_hashed(
                rig.shared.pg_pool.as_ref().unwrap(),
                &key,
                id,
                &rig.shared.token_hash,
                None,
            )
            .await;
            let changed =
                Snapshot::collect(&rig.shared, &ProviderKind::Claude, rig.channel, rig.ports())
                    .await
                    .unwrap();
            let original = stamp.snapshot.as_ref().unwrap();
            assert!(
                changed.exact != original.exact || changed.canonical != original.canonical,
                "seeded row did not change the lookup used by this observation"
            );
        }
    }
    assert!(
        publish(&stamp, &watch).await.is_none(),
        "stale {kind:?} input published an old absence"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_publication_marker_change_pg() {
    changed_publication(Changed::Marker, 69_182_001).await;
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_publication_inflight_change_pg() {
    changed_publication(Changed::Inflight, 69_182_002).await;
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_publication_fallback_change_pg() {
    changed_publication(Changed::Fallback, 69_182_003).await;
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_publication_exact_change_pg() {
    changed_publication(Changed::Exact, 69_182_004).await;
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_publication_canonical_change_pg() {
    changed_publication(Changed::Canonical, 69_182_005).await;
}

struct LegacyFile(PathBuf);
impl Drop for LegacyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
#[test]
fn s1e_runtime_marker_restore_preserves_legacy_and_canonical_priority() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let name = format!("s1-runtime-{}", uuid::Uuid::new_v4());
    let legacy = LegacyFile(PathBuf::from(tmux_common::legacy_tmp_session_path(
        &name,
        tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
    )));
    let canonical = marker_path(&name, tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT);
    std::fs::create_dir_all(canonical.parent().unwrap()).unwrap();
    std::fs::write(&legacy.0, "process_backend").unwrap();
    assert_eq!(
        local_trace(&name).runtime,
        Ok(Some(RuntimeHandoffKind::ProcessBackend)),
        "legacy runtime evidence disappeared"
    );
    assert_eq!(
        tmux_common::resolve_tmux_runtime_kind_marker(&name),
        Some(RuntimeHandoffKind::ProcessBackend)
    );
    std::fs::write(&canonical, "claude_tui").unwrap();
    assert_eq!(
        local_trace(&name).runtime,
        Ok(Some(RuntimeHandoffKind::ClaudeTui))
    );
    std::fs::remove_file(&canonical).unwrap();
    std::fs::create_dir(&canonical).unwrap();
    assert!(
        local_trace(&name).runtime.is_err(),
        "canonical read failure fell through to legacy"
    );
    std::fs::remove_dir(&canonical).unwrap();
    std::fs::write(&legacy.0, "unknown-runtime").unwrap();
    assert!(local_trace(&name).runtime.is_err());
    let legacy_host = LegacyFile(PathBuf::from(tmux_common::legacy_tmp_session_path(
        &name,
        "host_kind",
    )));
    std::fs::write(&legacy_host.0, "process").unwrap();
    assert_eq!(
        local_trace(&name).marker,
        HostKindMarker::Absent,
        "host-kind must remain canonical-only"
    );
}

struct ProcessTrace(String);
impl ProcessTrace {
    fn install(name: &str) -> Self {
        crate::services::session_backend::insert_process_session(
            name,
            crate::services::session_backend::SessionHandle::TestProcess {
                pid: 424_260,
                alive: Arc::new(true.into()),
            },
        );
        Self(name.into())
    }
}
impl Drop for ProcessTrace {
    fn drop(&mut self) {
        crate::services::session_backend::remove_process_session(&self.0);
    }
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_historical_host_traces_survive_current_absence_pg() {
    for (index, hosted, retired) in [(0, false, false), (1, true, false), (2, true, true)] {
        let rig = Rig::new(69_182_100 + index, hosted, retired).await;
        let history_name = format!("s1-history-H-{}", rig.channel);
        let name = history_name.as_str();
        rig.history(name);
        let snapshot =
            Snapshot::collect(&rig.shared, &ProviderKind::Claude, rig.channel, rig.ports())
                .await
                .unwrap();
        let decide = |s: &Snapshot| {
            decide_unbound(
                s.files.candidates.clone(),
                s.scope(&rig.shared),
                &Inventory::Sessions(vec![]),
                &BTreeMap::new(),
            )
        };
        assert!(
            matches!(decide(&snapshot), Verdict::Absent(_)),
            "positive history control: hosted={hosted} retired={retired} {:?}",
            decide(&snapshot)
        );
        let runtime = marker_path(name, tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT);
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        let marker = marker_path(name, "host_kind");
        std::fs::write(&marker, "tmux").unwrap();
        std::fs::write(&runtime, "claude_tui").unwrap();
        let benign =
            Snapshot::collect(&rig.shared, &ProviderKind::Claude, rig.channel, rig.ports())
                .await
                .unwrap();
        assert!(
            matches!(decide(&benign), Verdict::Absent(_)),
            "benign historical tmux trace became a permanent hold"
        );
        std::fs::remove_file(&marker).unwrap();
        std::fs::remove_file(&runtime).unwrap();
        for trace in ["marker", "runtime", "runtime_io", "process"] {
            let marker = marker_path(name, "host_kind");
            let process = match trace {
                "marker" => {
                    std::fs::write(&marker, "process").unwrap();
                    None
                }
                "runtime" => {
                    std::fs::write(&runtime, "unknown-runtime").unwrap();
                    None
                }
                "runtime_io" => {
                    std::fs::create_dir(&runtime).unwrap();
                    None
                }
                _ => Some(ProcessTrace::install(name)),
            };
            let next =
                Snapshot::collect(&rig.shared, &ProviderKind::Claude, rig.channel, rig.ports())
                    .await
                    .unwrap();
            assert!(
                matches!(decide(&next), Verdict::Hold(_)),
                "historical {trace} discarded for hosted={hosted} retired={retired}"
            );
            if trace == "process" {
                assert!(
                    next.files.traces[name].process.is_some(),
                    "process registry witness was invisible"
                );
            }
            drop(process);
            let _ = std::fs::remove_file(marker);
            if trace == "runtime_io" {
                std::fs::remove_dir(&runtime).unwrap();
            } else {
                let _ = std::fs::remove_file(&runtime);
            }
            let clean =
                Snapshot::collect(&rig.shared, &ProviderKind::Claude, rig.channel, rig.ports())
                    .await
                    .unwrap();
            assert!(
                matches!(decide(&clean), Verdict::Absent(_)),
                "fixture left an unrelated hold after {trace}"
            );
        }
    }
}

struct Pause {
    at: usize,
    count: AtomicUsize,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
static PAUSES: std::sync::LazyLock<Mutex<BTreeMap<u64, Arc<Pause>>>> =
    std::sync::LazyLock::new(Default::default);
pub(super) async fn after_files(channel: u64) {
    let pause = lock(&PAUSES).get(&channel).cloned();
    if let Some(pause) = pause {
        if pause.count.fetch_add(1, Ordering::SeqCst) + 1 == pause.at {
            pause.entered.notify_one();
            pause.resume.notified().await;
        }
    }
}
struct PauseGuard(u64);
impl Drop for PauseGuard {
    fn drop(&mut self) {
        lock(&PAUSES).remove(&self.0);
    }
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_marker_change_during_second_snapshot_db_await_pg() {
    let rig = Rig::new(69_182_006, false, false).await;
    let count = ChannelProbeTrace::new(rig.channel.get());
    let pause = Arc::new(Pause {
        at: 2,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(rig.channel.get(), pause.clone());
    let _pause = PauseGuard(rig.channel.get());
    let (shared, channel, watch) = (rig.shared.clone(), rig.channel, rig.watch());
    let task =
        tokio::spawn(async move { read(&shared, &ProviderKind::Claude, channel, watch).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered.notified())
        .await
        .unwrap();
    assert!(count.calls() > 0);
    let marker = marker_path(&rig.name(), "host_kind");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, "process").unwrap();
    pause.resume.notify_one();
    let (answer, _) = task.await.unwrap();
    assert_eq!(
        answer.activity,
        Activity::Unknown,
        "file changed after its last read but before the final DB await"
    );
}

struct NamespaceLifetime {
    socket: PathBuf,
    fifo: PathBuf,
}
impl NamespaceLifetime {
    fn new(root: &std::path::Path, socket: PathBuf) -> (Self, String) {
        let fifo = root.join("publication-finish.fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let read = format!(
            "read adk_fixture_line < {}",
            crate::services::process::shell_escape(fifo.to_str().unwrap())
        );
        let command = format!(
            "/bin/sh -c {}",
            crate::services::process::shell_escape(&read)
        );
        (Self { socket, fifo }, command)
    }
}
impl Drop for NamespaceLifetime {
    fn drop(&mut self) {
        use std::os::unix::fs::OpenOptionsExt;
        for _ in 0..300 {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.fifo)
            {
                let _ = std::io::Write::write_all(&mut file, b"done\n");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        for _ in 0..300 {
            if std::os::unix::net::UnixStream::connect(&self.socket).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_publication_rechecks_new_related_tmux_session_pg() {
    let rig = Rig::new(69_182_009, false, false).await;
    let watch = rig.watch();
    let counter = ChannelProbeTrace::new(rig.channel.get());
    let (answer, stamp) = read(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    assert!(counter.calls() > 0);
    assert!(publish(&stamp, &watch).await.is_some());
    let socket = match &stamp.namespace.as_ref().unwrap().inventory {
        Inventory::Empty(
            crate::services::platform::tmux::session_inventory::EmptyReason::MissingSocket(path),
        ) => path.clone(),
        other => panic!("fresh private namespace expected: {other:?}"),
    };
    let (_lifetime, command) = NamespaceLifetime::new(rig.root.path(), socket.clone());
    let name = rig.name();
    let output = crate::services::platform::binary_resolver::runtime_command("tmux")
        .unwrap()
        .args([
            "-u",
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            &name,
            &command,
        ])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        socket.exists(),
        "live fixture never created its private socket"
    );
    assert!(
        publish(&stamp, &watch).await.is_none(),
        "new related live tmux reused an old empty inventory at publication"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_actual_reading_revalidates_absence_and_has_no_busy_identity_pg() {
    let rig = Rig::new(69_182_007, false, false).await;
    let trace = ChannelProbeTrace::new(rig.channel.get());
    let first = crate::services::discord::turn_presence::activity::reading_now(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
    )
    .await;
    assert_eq!(first.observed, observed(Activity::Idle, "no_session"));
    assert!(trace.calls() > 0);
    assert!(first.session().is_none() && first.source().is_none());
    assert!(first.publish_if_current(|| ()).await.is_some());
    let second = crate::services::discord::turn_presence::activity::reading_now(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
    )
    .await;
    assert_eq!(second.observed.activity, Activity::Idle);
    assert!(
        first.publish_if_current(|| ()).await.is_none(),
        "newer successful absence supersedes old publication"
    );
    assert!(second.publish_if_current(|| ()).await.is_some());
    let marker = marker_path(&rig.name(), "host_kind");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, "process").unwrap();
    assert!(
        second.publish_if_current(|| ()).await.is_none(),
        "Reading bypassed stamp host revalidation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1e_herdr_first_create_keeps_legacy_runtime_trace_pg() {
    let rig = Rig::new(69_182_008, true, false).await;
    let count = ChannelProbeTrace::new(rig.channel.get());
    let read = crate::services::discord::turn_presence::activity::activity_now(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
    )
    .await;
    assert_eq!(read, observed(Activity::Idle, "no_session"));
    assert!(count.calls() > 0);
    let name = rig.name();
    let legacy = LegacyFile(PathBuf::from(tmux_common::legacy_tmp_session_path(
        &name,
        tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
    )));
    assert!(
        !legacy.0.exists(),
        "fixture name collided with another legacy marker"
    );
    std::fs::write(&legacy.0, "claude_tui").unwrap();
    let read = crate::services::discord::turn_presence::activity::activity_now(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
    )
    .await;
    assert_eq!(
        read.activity,
        Activity::Unknown,
        "first-create discarded a legacy execution trace"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1e_successful_unbound_reads_complete_in_reverse_order_pg() {
    let rig = Rig::new(69_182_010, false, false).await;
    let counter = ChannelProbeTrace::new(rig.channel.get());
    let pause = Arc::new(Pause {
        at: 2,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(rig.channel.get(), pause.clone());
    let _pause = PauseGuard(rig.channel.get());
    let (shared, channel) = (rig.shared.clone(), rig.channel);
    let first = tokio::spawn(async move {
        crate::services::discord::turn_presence::activity::reading_now(
            &shared,
            &ProviderKind::Claude,
            channel,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered.notified())
        .await
        .unwrap();
    assert!(
        counter.calls() > 0,
        "first observation never reached its successful probe"
    );
    let second = crate::services::discord::turn_presence::activity::reading_now(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
    )
    .await;
    assert_eq!(second.observed, observed(Activity::Idle, "no_session"));
    assert!(second.publish_if_current(|| ()).await.is_some());
    pause.resume.notify_one();
    let first = first.await.unwrap();
    assert_eq!(first.observed, observed(Activity::Unknown, "superseded"));
    let calls = AtomicUsize::new(0);
    assert!(
        first
            .publish_if_current(|| {
                calls.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(second.publish_if_current(|| ()).await.is_some());
}
#[tokio::test(flavor = "current_thread")]
async fn s1e_unbound_publication_rejects_native_seq_and_namespace_changes_pg() {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let rig = Rig::new(69_182_011, false, false).await;
    let watch = rig.watch();
    let (answer, stamp) = read(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    assert!(publish(&stamp, &watch).await.is_some());
    let other = tempfile::tempdir().unwrap();
    let namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        other.path(),
    );
    assert!(publish(&stamp, &watch).await.is_none());
    drop(namespace);
    assert!(publish(&stamp, &watch).await.is_some());
    let path = rig.root.path().join("pending.jsonl");
    let proposal = p5::Proposal {
        channel_id: rig.channel.get(),
        provider: "claude",
        tmux_session: "s1-pending-native",
        session_id: Some("pending"),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Observed,
        hook: None,
    };
    assert_eq!(
        p5::record_pending(&proposal).unwrap(),
        p5::PendingRecord::Recorded
    );
    assert!(
        publish(&stamp, &watch).await.is_none(),
        "native append did not invalidate the old sequence witness"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1e_marker_change_during_publication_db_await_pg() {
    let rig = Rig::new(69_182_012, false, false).await;
    let watch = rig.watch();
    let (answer, stamp) = read(
        &rig.shared,
        &ProviderKind::Claude,
        rig.channel,
        watch.clone(),
    )
    .await;
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    assert!(publish(&stamp, &watch).await.is_some());
    let pause = Arc::new(Pause {
        at: 1,
        count: AtomicUsize::new(0),
        entered: Default::default(),
        resume: Default::default(),
    });
    lock(&PAUSES).insert(rig.channel.get(), pause.clone());
    let _pause = PauseGuard(rig.channel.get());
    let calls = Arc::new(AtomicUsize::new(0));
    let published = calls.clone();
    let task = tokio::spawn(async move {
        stamp
            .publish(&watch, || published.fetch_add(1, Ordering::SeqCst))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.entered.notified())
        .await
        .unwrap();
    let marker = marker_path(&rig.name(), "host_kind");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, "process").unwrap();
    pause.resume.notify_one();
    assert!(
        task.await.unwrap().is_none(),
        "publication used files read before its last DB await"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "stale publication callback ran"
    );
}
