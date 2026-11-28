use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::turn_presence::activity::tests::BindingRoot;

pub(in crate::services::discord) struct Rig {
    pub(in crate::services::discord) shared: Arc<SharedData>,
    pub(in crate::services::discord) channel: ChannelId,
    pub(in crate::services::discord) config: Config,
    pub(in crate::services::discord) root: tempfile::TempDir,
    _binding: BindingRoot,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _namespace: crate::config::TestEnvVarGuard,
    _root_env: crate::config::TestEnvVarGuard,
    _db: TestPostgresDb,
    _live: crate::config_live_reload::TestLiveGuard,
}
impl Rig {
    pub(in crate::services::discord) fn name(&self) -> String {
        ProviderKind::Claude.build_tmux_session_name(&format!("s1e-{}", self.channel))
    }
    pub(in crate::services::discord) fn watch(&self) -> Arc<Mutex<Watch>> {
        Arc::new(Mutex::new(Watch::default()))
    }
    pub(in crate::services::discord) fn ports(&self) -> Arc<LivePorts> {
        Arc::new(LivePorts {
            root: Some(self.root.path().to_owned()),
        })
    }
    async fn seed_retired(&self) {
        use crate::db::dispatched_sessions::hosted_execution::{
            HostedLocation, test_support::record,
        };
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: self.shared.token_hash.clone(),
            channel_id: self.channel.to_string(),
            logical_key: self.name(),
            owner_node: "test-node".into(),
            runtime_root: self.root.path().display().to_string(),
        };
        let boot =
            crate::config::session_hosts::BootSessionHosts::from_config(&self.config).unwrap();
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
    pub(in crate::services::discord) fn history(&self, name: &str) {
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

impl Rig {
    pub(in crate::services::discord) async fn new(id: u64, hosted: bool, retired: bool) -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        Self::with_live_guard(id, hosted, retired, live).await
    }
    pub(in crate::services::discord) async fn with_live_guard(
        id: u64,
        hosted: bool,
        retired: bool,
        live: crate::config_live_reload::TestLiveGuard,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let root_env = set("AGENTDESK_ROOT_DIR", root.path());
        // tmux reports canonical socket paths, including macOS's /var -> /private/var alias.
        let namespace = set("TMUX_TMPDIR", &root.path().canonicalize().unwrap());
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
}
