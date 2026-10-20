//! Restored binding-only state reads durable identity without starting or clearing a session.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::turn_presence::activity::tests::BindingRoot;
use crate::services::platform::tmux::session_inventory::tests::{
    ChannelCommand, ChannelProbeTrace,
};
use crate::services::tui_prompt_dedupe::binding_events as p5;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RecoveryFixture {
    shared: Arc<SharedData>,
    channel: ChannelId,
    old_name: String,
    canonical_name: String,
    key: String,
    row_id: i64,
    inventory: PathBuf,
    binding: p5::BindingEvent,
    _command: ChannelCommand,
    probes: ChannelProbeTrace,
    _binding_root: BindingRoot,
    _hosts: crate::config::session_hosts::ForcedSessionHosts,
    _env: Vec<crate::config::TestEnvVarGuard>,
    root: tempfile::TempDir,
    _db: TestPostgresDb,
    _live: crate::config_live_reload::TestLiveGuard,
}

impl RecoveryFixture {
    async fn connect_and_migrate_fixture(id: u64) -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let config_path = crate::runtime_layout::config_file_path(root.path());
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let env = vec![
            set("AGENTDESK_ROOT_DIR", root.path()),
            set("TMPDIR", root.path()),
            set("AGENTDESK_CONFIG", &config_path),
            set("CODEX_HOME", &root.path().join("codex-home")),
            set("CLAUDE_CONFIG_DIR", &root.path().join("claude-home")),
        ];
        let hosts = crate::config::session_hosts::force_for_test(Some("test-node"), &[]);
        let mut config: Config = serde_yaml::from_str(&format!(
            "server: {{}}\ncluster: {{instance_id: test-node}}\nagents:\n  - id: stale-recovery\n    name: Stale recovery\n    provider: claude\n    channels:\n      claude: {{id: '{id}', runtime: tui}}\n"
        ))
        .unwrap();
        config.session_hosts =
            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
        crate::config_live_reload::install(config.clone());
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
        let data = Arc::get_mut(&mut shared).unwrap();
        data.provider = ProviderKind::Claude;
        data.token_hash = "s1-restored-token".into();
        let channel = ChannelId::new(id);
        assert!(shared.core.lock().await.sessions.is_empty());
        assert!(shared.tmux_watchers.channel_binding(&channel).is_none());
        assert!(
            registrations(&config)
                .unwrap()
                .iter()
                .filter(|binding| binding.channel_id == id)
                .all(|binding| binding.fallback_name.is_none())
        );
        let old_name = ProviderKind::Claude.build_tmux_session_name(&format!("s1-old-{id}"));
        let canonical_name =
            ProviderKind::Claude.build_tmux_session_name(&format!("s1-canonical-{id}"));
        let key = adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Claude,
            &canonical_name,
        );
        let pool = shared.pg_pool.as_ref().unwrap();
        inflight::seed_session_row_hashed(pool, &key, id, &shared.token_hash, None).await;
        sqlx::query("UPDATE sessions SET status = 'idle', active_turn_nonce = 'stable-terminal-nonce' WHERE session_key = $1")
            .bind(&key).execute(pool).await.unwrap();
        let row_id = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
            .bind(&key)
            .fetch_one(pool)
            .await
            .unwrap();
        let binding_root = BindingRoot::enter(root.path());
        let transcript = root.path().join("historical-transcript.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        let source = crate::services::tui_o::shadow::binding_reader::source_id_for(
            "historical",
            &transcript,
        )
        .unwrap();
        let now = chrono::Utc::now();
        let binding = p5::BindingEvent {
            seq: 1,
            channel_id: id,
            provider: "claude".into(),
            tmux_session: old_name.clone(),
            execution_nonce: Some("historical-execution".into()),
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
        let binding_dir = root.path().join(p5::BINDING_EVENTS_DIR);
        std::fs::create_dir_all(&binding_dir).unwrap();
        std::fs::write(
            binding_dir.join(format!("{id}.log")),
            format!("{}\n", serde_json::to_string(&binding).unwrap()),
        )
        .unwrap();
        let inventory = root.path().join("inventory.txt");
        std::fs::write(&inventory, "").unwrap();
        let executable = root.path().join("inventory-reader");
        let quoted = inventory.to_str().unwrap().replace('\'', "'\\''");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nexec /bin/cat '{quoted}'\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = ChannelCommand::new(id, executable);
        let probes = ChannelProbeTrace::new(id);
        Self {
            shared,
            channel,
            old_name,
            canonical_name,
            key,
            row_id,
            inventory,
            binding,
            _command: command,
            probes,
            _binding_root: binding_root,
            _hosts: hosts,
            _env: env,
            root,
            _db: db,
            _live: live,
        }
    }

    async fn read(&self) -> (Observed, Stamp, Arc<Mutex<Watch>>) {
        let watch = Arc::new(Mutex::new(Watch::default()));
        let (answer, stamp) = read(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            watch.clone(),
        )
        .await;
        (answer, stamp, watch)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_restored_stale_binding_uses_canonical_target_and_keeps_old_candidate_pg() {
    let fixture = RecoveryFixture::connect_and_migrate_fixture(69_185_101).await;
    let marker_dir = marker_path(&fixture.canonical_name, "spawn_nonce")
        .parent()
        .unwrap()
        .to_owned();
    assert!(marker_dir.starts_with(fixture.root.path()));
    assert!(!marker_dir.exists());
    let binding_bytes = std::fs::read(fixture.binding_path()).unwrap();
    let (answer, stamp, watch) = fixture.read().await;
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    let snapshot = stamp
        .snapshot
        .as_ref()
        .expect("successful absence snapshot");
    assert!(snapshot.basis.is_none());
    let candidates = snapshot.files.candidates.as_ref().unwrap();
    assert_eq!(candidates.current.as_ref(), Some(&fixture.canonical_name));
    for name in [&fixture.old_name, &fixture.canonical_name] {
        assert!(
            candidates
                .names
                .iter()
                .any(|candidate| &candidate.name == name)
        );
    }
    assert!(!candidates.inflight);
    assert!(
        fixture.probes.calls() > 0,
        "fake inventory was not consumed"
    );
    let callbacks = AtomicUsize::new(0);
    assert!(
        stamp
            .publish(&watch, || callbacks.fetch_add(1, Ordering::SeqCst))
            .await
            .is_some()
    );
    assert_eq!(callbacks.load(Ordering::SeqCst), 1);
    let retained: Option<String> = sqlx::query_scalar(
        "SELECT active_turn_nonce FROM sessions WHERE id = $1 AND status = 'idle'",
    )
    .bind(fixture.row_id)
    .fetch_one(fixture.shared.pg_pool.as_ref().unwrap())
    .await
    .unwrap();
    assert_eq!(retained.as_deref(), Some("stable-terminal-nonce"));
    assert!(fixture.shared.core.lock().await.sessions.is_empty());
    assert_eq!(
        std::fs::read(fixture.binding_path()).unwrap(),
        binding_bytes
    );
    assert!(
        !marker_dir.exists(),
        "absence reads created marker directories"
    );
    let mut same_target = fixture.binding.clone();
    same_target.tmux_session = fixture.canonical_name.clone();
    fixture.write_binding(&same_target, false);
    let (same_stamp, _) = fixture.positive().await;
    assert!(
        same_stamp
            .snapshot
            .as_ref()
            .unwrap()
            .files
            .candidates
            .as_ref()
            .unwrap()
            .names
            .iter()
            .all(|candidate| candidate.name == fixture.canonical_name),
        "binding equal to the canonical target must remain recoverable"
    );
}

impl RecoveryFixture {
    fn pool(&self) -> &sqlx::PgPool {
        self.shared.pg_pool.as_ref().unwrap()
    }
    fn binding_path(&self) -> PathBuf {
        self.root
            .path()
            .join(p5::BINDING_EVENTS_DIR)
            .join(format!("{}.log", self.channel))
    }
    fn write_binding(&self, event: &p5::BindingEvent, append: bool) {
        let mut text = if append {
            std::fs::read_to_string(self.binding_path()).unwrap()
        } else {
            String::new()
        };
        text.push_str(&serde_json::to_string(event).unwrap());
        text.push('\n');
        std::fs::write(self.binding_path(), text).unwrap();
    }
    fn inventory(&self, names: &[&str]) {
        let mut text = names.join("\n");
        if !names.is_empty() {
            text.push('\n');
        }
        std::fs::write(&self.inventory, text).unwrap();
    }
    fn marker(&self, name: &str, extension: &str, bytes: &[u8]) -> PathBuf {
        let path = marker_path(name, extension);
        assert!(path.starts_with(self.root.path()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
    fn install_config(&self, config: Config) {
        let path = crate::runtime_layout::config_file_path(self.root.path());
        std::fs::write(path, serde_yaml::to_string(&config).unwrap()).unwrap();
        crate::config_live_reload::install(config);
    }
    async fn positive(&self) -> (Stamp, Arc<Mutex<Watch>>) {
        let (answer, stamp, watch) = self.read().await;
        assert_eq!(answer, observed(Activity::Idle, "no_session"));
        let calls = AtomicUsize::new(0);
        assert!(
            stamp
                .publish(&watch, || calls.fetch_add(1, Ordering::SeqCst))
                .await
                .is_some()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        (stamp, watch)
    }
    async fn hold(&self, reason: &'static str) {
        let (answer, _, _) = self.read().await;
        assert_eq!(answer, observed(Activity::Unknown, reason));
    }
    fn inflight_path(&self) -> PathBuf {
        inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            self.channel.get(),
        )
    }
    fn inflight(&self, nonce: &str) {
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            self.channel.get(),
            None,
            200,
            901,
            902,
            "persisted pending input".into(),
            None,
            Some(self.canonical_name.clone()),
            None,
            None,
            0,
        );
        row.turn_nonce = Some(nonce.into());
        let path = self.inflight_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&row).unwrap()).unwrap();
    }
    async fn queue_sentinel(&self) {
        use crate::services::turn_orchestrator::{
            Intervention, InterventionMode, QueuePersistenceContext,
        };
        use poise::serenity_prelude::{MessageId, UserId};
        let id = MessageId::new(self.channel.get() + 5_000);
        let item = Intervention {
            author_id: UserId::new(200),
            author_is_bot: false,
            message_id: id,
            queued_generation: runtime_store::process_generation(),
            source_message_ids: vec![id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "must remain queued".into(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let persistence =
            QueuePersistenceContext::new(&ProviderKind::Claude, &self.shared.token_hash, None);
        assert!(
            self.shared
                .mailbox(self.channel)
                .enqueue(item, persistence)
                .await
                .enqueued
        );
    }
    async fn queue_ids(&self) -> (Vec<u64>, Vec<u64>) {
        let memory = self
            .shared
            .mailbox(self.channel)
            .snapshot()
            .await
            .intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .collect();
        let disk = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &ProviderKind::Claude,
            &self.shared.token_hash,
            self.channel,
        )
        .0
        .iter()
        .map(|item| item.message_id.get())
        .collect();
        (memory, disk)
    }
    async fn reject_without_effects(&self, stamp: &Stamp, watch: &Mutex<Watch>) {
        let expected = vec![self.channel.get() + 5_000];
        let queue = self.queue_ids().await;
        assert_eq!(queue, (expected.clone(), expected));
        let marker = marker_path(&self.canonical_name, "spawn_nonce");
        let bytes = std::fs::read(&marker).unwrap();
        let calls = AtomicUsize::new(0);
        assert!(
            stamp
                .publish(watch, || calls.fetch_add(1, Ordering::SeqCst))
                .await
                .is_none(),
            "changed evidence published the saved Idle"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(self.queue_ids().await, queue);
        assert_eq!(std::fs::read(marker).unwrap(), bytes);
    }
    async fn publication_control(&self) -> (Stamp, Arc<Mutex<Watch>>) {
        self.queue_sentinel().await;
        self.marker(&self.canonical_name, "spawn_nonce", b"stable-spawn-nonce");
        self.positive().await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_either_historical_or_canonical_live_candidate_holds_pg() {
    let fixture = RecoveryFixture::connect_and_migrate_fixture(69_185_102).await;
    let foreign = ChannelId::new(fixture.channel.get() + 99);
    fixture
        .shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(&fixture.old_name, foreign);
    assert_eq!(
        fixture
            .shared
            .tmux_watchers
            .owner_channel_for_tmux_session(&fixture.old_name),
        Some(foreign)
    );
    fixture.positive().await;
    for name in [&fixture.old_name, &fixture.canonical_name] {
        fixture.inventory(&[name]);
        fixture.hold("watcher_unbound").await;
        let nonce = fixture.marker(name, "spawn_nonce", b" \n");
        fixture.hold("watcher_unbound").await;
        std::fs::remove_file(nonce).unwrap();
        fixture.inventory(&[]);
        fixture.positive().await;
    }
    assert!(
        std::fs::read_to_string(fixture.binding_path())
            .unwrap()
            .contains(&fixture.old_name)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1_canonical_missing_namespace_or_tuple_mismatch_keeps_hold_pg() {
    let cases = [
        "missing",
        "legacy",
        "malformed",
        "key-token",
        "key-provider",
        "key-host",
        "row-token",
        "row-provider",
        "row-channel",
        "row-kind",
        "conflict",
        "read-error",
    ];
    for (offset, case) in cases.into_iter().enumerate() {
        let fixture =
            RecoveryFixture::connect_and_migrate_fixture(69_185_120 + offset as u64).await;
        fixture.positive().await;
        let key = match case {
            "legacy" => Some(format!(
                "{}:{}",
                crate::services::platform::hostname_short(),
                fixture.canonical_name
            )),
            "malformed" => Some("claude/malformed-primary".into()),
            "key-token" => Some(adk_session::build_namespaced_session_key(
                "other-token",
                &ProviderKind::Claude,
                &fixture.canonical_name,
            )),
            "key-provider" => Some(adk_session::build_namespaced_session_key(
                &fixture.shared.token_hash,
                &ProviderKind::Codex,
                &fixture.canonical_name,
            )),
            "key-host" => Some(format!(
                "claude/{}/{}-foreign:{}",
                fixture.shared.token_hash,
                crate::services::platform::hostname_short(),
                fixture.canonical_name
            )),
            _ => None,
        };
        if let Some(key) = key {
            sqlx::query("UPDATE sessions SET session_key = $2 WHERE id = $1")
                .bind(fixture.row_id)
                .bind(key)
                .execute(fixture.pool())
                .await
                .unwrap();
            if matches!(case, "legacy" | "key-token" | "key-provider" | "key-host") {
                sqlx::query(
                    "INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)",
                )
                .bind(&fixture.key)
                .bind(fixture.row_id)
                .execute(fixture.pool())
                .await
                .unwrap();
                let exact = load_hosted_execution_pg(
                    fixture.pool(),
                    HostedLookupKey::SessionKey(&fixture.key),
                )
                .await;
                let channel = fixture.channel.to_string();
                let canonical = load_hosted_execution_pg(
                    fixture.pool(),
                    HostedLookupKey::Canonical {
                        provider: "claude",
                        identity: CanonicalSessionIdentity {
                            kind: SessionIdentityKind::DiscordChannel,
                            discord_token_hash: &fixture.shared.token_hash,
                            channel_id: &channel,
                        },
                    },
                )
                .await;
                assert!(matches!(exact, HostedLookup::Found(_)));
                assert_eq!(exact, canonical, "alias convergence control: {case}");
            }
        } else {
            match case {
                "missing" => {
                    sqlx::query("DELETE FROM sessions WHERE id = $1")
                        .bind(fixture.row_id)
                        .execute(fixture.pool())
                        .await
                        .unwrap();
                }
                "row-token" => {
                    sqlx::query(
                        "UPDATE sessions SET discord_token_hash = 'other-token' WHERE id = $1",
                    )
                    .bind(fixture.row_id)
                    .execute(fixture.pool())
                    .await
                    .unwrap();
                }
                "row-provider" => {
                    sqlx::query("UPDATE sessions SET provider = 'codex' WHERE id = $1")
                        .bind(fixture.row_id)
                        .execute(fixture.pool())
                        .await
                        .unwrap();
                }
                "row-channel" => {
                    sqlx::query("UPDATE sessions SET channel_id = '999' WHERE id = $1")
                        .bind(fixture.row_id)
                        .execute(fixture.pool())
                        .await
                        .unwrap();
                }
                "row-kind" => {
                    sqlx::query(
                        "UPDATE sessions SET identity_kind = 'scheduled_snapshot' WHERE id = $1",
                    )
                    .bind(fixture.row_id)
                    .execute(fixture.pool())
                    .await
                    .unwrap();
                }
                "conflict" => {
                    sqlx::query("DROP INDEX sessions_canonical_discord_identity_uidx")
                        .execute(fixture.pool())
                        .await
                        .unwrap();
                    let other = adk_session::build_namespaced_session_key(
                        &fixture.shared.token_hash,
                        &ProviderKind::Claude,
                        "AgentDesk-claude-s1-conflicting",
                    );
                    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3)")
                        .bind(other).bind(&fixture.shared.token_hash).bind(fixture.channel.to_string()).execute(fixture.pool()).await.unwrap();
                }
                "read-error" => {
                    sqlx::query("ALTER TABLE sessions RENAME COLUMN active_turn_nonce TO hidden_active_turn_nonce").execute(fixture.pool()).await.unwrap();
                }
                _ => unreachable!(),
            }
        }
        fixture.hold("session_identity_unresolved").await;
        eprintln!("S1_CANONICAL_HOLD case={case}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_hosted_records_and_nonlocal_registration_are_not_recovered_pg() {
    use crate::db::dispatched_sessions::hosted_execution::tests::{record, wire};
    for (offset, case) in [
        "herdr",
        "json-null",
        "unknown",
        "unregistered",
        "process-runtime",
        "herdr-config",
    ]
    .into_iter()
    .enumerate()
    {
        let fixture =
            RecoveryFixture::connect_and_migrate_fixture(69_185_140 + offset as u64).await;
        fixture.positive().await;
        let mut forced = None;
        match case {
            "herdr" | "json-null" | "unknown" => {
                let owner = HostedOwner {
                    provider: "claude".into(),
                    discord_token_hash: fixture.shared.token_hash.clone(),
                    channel_id: fixture.channel.to_string(),
                    logical_key: fixture.canonical_name.clone(),
                    owner_node: "test-node".into(),
                    runtime_root: fixture.root.path().display().to_string(),
                };
                let raw = match case {
                    "herdr" => wire(&record(&owner, "past-hosted-nonce", HostedState::Retired)),
                    "json-null" => serde_json::Value::Null,
                    _ => serde_json::json!({"schema":99}),
                };
                sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE id = $1")
                    .bind(fixture.row_id)
                    .bind(raw)
                    .execute(fixture.pool())
                    .await
                    .unwrap();
            }
            "unregistered" => {
                let mut config = (*crate::config_live_reload::current().unwrap()).clone();
                config.agents.clear();
                fixture.install_config(config);
            }
            "process-runtime" => {
                let mut config = (*crate::config_live_reload::current().unwrap()).clone();
                let mut channel = config.agents[0].channels.get("claude").unwrap().clone();
                let crate::config::AgentChannel::Detailed(binding) = &mut channel else {
                    panic!("detailed local registration")
                };
                binding.runtime = Some("pipe".into());
                config.agents[0].channels.insert("claude", channel);
                fixture.install_config(config);
            }
            "herdr-config" => {
                forced = Some(crate::config::session_hosts::force_for_test(
                    Some("test-node"),
                    &[(fixture.channel.get(), "test-node")],
                ));
                let mut config = (*crate::config_live_reload::current().unwrap()).clone();
                config.session_hosts =
                    crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
                fixture.install_config(config);
            }
            _ => unreachable!(),
        }
        fixture.hold("session_identity_unresolved").await;
        drop(forced);
        eprintln!("S1_HOST_HOLD case={case}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_historical_and_canonical_host_runtime_nonce_traces_are_required_pg() {
    let fixture = RecoveryFixture::connect_and_migrate_fixture(69_185_150).await;
    fixture.positive().await;
    for name in [&fixture.old_name, &fixture.canonical_name] {
        for (extension, bytes, reason) in [
            ("host_kind", &b"process"[..], "session_identity_unresolved"),
            ("host_kind", &b"herdr"[..], "session_identity_unresolved"),
            (
                "host_kind",
                &b"unknown-host"[..],
                "session_identity_unresolved",
            ),
            (
                tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
                &b"process_backend"[..],
                "session_identity_unresolved",
            ),
            (
                tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
                &b"unknown-runtime"[..],
                "session_probe_failed",
            ),
            ("spawn_nonce", &b" \n"[..], "session_probe_failed"),
            ("spawn_nonce", &[0xff][..], "session_probe_failed"),
        ] {
            let path = fixture.marker(name, extension, bytes);
            fixture.hold(reason).await;
            std::fs::remove_file(path).unwrap();
            fixture.positive().await;
        }
        let path = marker_path(name, "host_kind");
        std::fs::create_dir(&path).unwrap();
        fixture.hold("session_probe_failed").await;
        std::fs::remove_dir(path).unwrap();
        fixture.positive().await;
    }

    let nonce = fixture.marker(&fixture.canonical_name, "spawn_nonce", b" \n");
    crate::services::discord::host_defer_gate::tests::map_channel(
        &fixture.shared,
        fixture.channel,
        &format!("s1-canonical-{}", fixture.channel),
    )
    .await;
    fixture.positive().await;
    fixture
        .shared
        .core
        .lock()
        .await
        .sessions
        .remove(&fixture.channel);
    fixture.hold("session_probe_failed").await;
    std::fs::remove_file(nonce).unwrap();
    fixture.positive().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1_inflight_unreadable_log_or_inventory_never_proves_absence_pg() {
    let fixture = RecoveryFixture::connect_and_migrate_fixture(69_185_151).await;
    fixture.positive().await;
    fixture.inflight("existing-turn-nonce");
    fixture.hold("session_identity_unresolved").await;
    std::fs::remove_file(fixture.inflight_path()).unwrap();
    fixture.positive().await;
    std::fs::create_dir(fixture.inflight_path()).unwrap();
    fixture.hold("session_probe_failed").await;
    std::fs::remove_dir(fixture.inflight_path()).unwrap();
    fixture.positive().await;
    std::fs::write(fixture.binding_path(), "corrupt-binding\n").unwrap();
    fixture.hold("session_probe_failed").await;
    fixture.write_binding(&fixture.binding, false);
    fixture.positive().await;
    std::fs::write(&fixture.inventory, "invalid.inventory:name\n").unwrap();
    fixture.hold("session_probe_failed").await;
    fixture.inventory(&[]);
    fixture.positive().await;
    std::fs::remove_file(fixture.binding_path()).unwrap();
    fixture.hold("session_identity_unresolved").await;
}

async fn collide_exact_alias(fixture: &RecoveryFixture) {
    let other_key = adk_session::build_namespaced_session_key(
        &fixture.shared.token_hash,
        &ProviderKind::Claude,
        "AgentDesk-claude-s1-other-row",
    );
    inflight::seed_session_row_hashed(
        fixture.pool(),
        &other_key,
        fixture.channel.get() + 1,
        &fixture.shared.token_hash,
        None,
    )
    .await;
    let other_id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
        .bind(other_key)
        .fetch_one(fixture.pool())
        .await
        .unwrap();
    // Corrupt only this throwaway database to exercise the read-only divergence guard.
    sqlx::query("DROP TRIGGER trg_session_key_aliases_locator_namespace ON session_key_aliases")
        .execute(fixture.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO session_key_aliases (session_key, session_id) VALUES ($1, $2)")
        .bind(&fixture.key)
        .bind(other_id)
        .execute(fixture.pool())
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn s1_exact_alias_pointing_to_another_row_keeps_hold_pg() {
    let fixture = RecoveryFixture::connect_and_migrate_fixture(69_185_152).await;
    fixture.positive().await;
    collide_exact_alias(&fixture).await;
    fixture.hold("session_identity_unresolved").await;
}

#[tokio::test(flavor = "current_thread")]
async fn s1_saved_idle_rejects_canonical_row_status_nonce_key_and_alias_changes_pg() {
    for (offset, case) in ["status", "nonce", "key", "row", "alias"]
        .into_iter()
        .enumerate()
    {
        let fixture =
            RecoveryFixture::connect_and_migrate_fixture(69_185_160 + offset as u64).await;
        let (stamp, watch) = fixture.publication_control().await;
        match case {
            "status" => {
                sqlx::query("UPDATE sessions SET status = 'disconnected' WHERE id = $1")
                    .bind(fixture.row_id)
                    .execute(fixture.pool())
                    .await
                    .unwrap();
            }
            "nonce" => {
                sqlx::query("UPDATE sessions SET active_turn_nonce = 'another-stable-terminal-nonce' WHERE id = $1").bind(fixture.row_id).execute(fixture.pool()).await.unwrap();
            }
            "key" => {
                let key = adk_session::build_namespaced_session_key(
                    &fixture.shared.token_hash,
                    &ProviderKind::Claude,
                    "AgentDesk-claude-s1-replacement-target",
                );
                sqlx::query("UPDATE sessions SET session_key = $2 WHERE id = $1")
                    .bind(fixture.row_id)
                    .bind(key)
                    .execute(fixture.pool())
                    .await
                    .unwrap();
            }
            "row" => {
                sqlx::query("DELETE FROM sessions WHERE id = $1")
                    .bind(fixture.row_id)
                    .execute(fixture.pool())
                    .await
                    .unwrap();
                inflight::seed_session_row_hashed(
                    fixture.pool(),
                    &fixture.key,
                    fixture.channel.get(),
                    &fixture.shared.token_hash,
                    None,
                )
                .await;
                sqlx::query("UPDATE sessions SET active_turn_nonce = 'stable-terminal-nonce' WHERE session_key = $1").bind(&fixture.key).execute(fixture.pool()).await.unwrap();
            }
            "alias" => collide_exact_alias(&fixture).await,
            _ => unreachable!(),
        }
        fixture.reject_without_effects(&stamp, &watch).await;
        if case == "alias" {
            fixture.hold("session_identity_unresolved").await;
        } else {
            fixture.positive().await;
        }
        eprintln!("S1_DB_PUBLICATION case={case} old_callbacks=0");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_saved_idle_rejects_same_seq_binding_nonce_payload_and_seq_changes_pg() {
    for (offset, case) in ["nonce", "payload", "seq"].into_iter().enumerate() {
        let fixture =
            RecoveryFixture::connect_and_migrate_fixture(69_185_170 + offset as u64).await;
        let (stamp, watch) = fixture.publication_control().await;
        let mut binding = fixture.binding.clone();
        match case {
            "nonce" => binding.execution_nonce = Some("changed-same-seq-nonce".into()),
            "payload" => {
                binding.new = p5::BindingTarget::Pending {
                    payload_session_id: "another-native-session".into(),
                    payload_transcript_path: Some(
                        fixture
                            .root
                            .path()
                            .join("another-transcript.jsonl")
                            .display()
                            .to_string(),
                    ),
                }
            }
            "seq" => binding.seq += 1,
            _ => unreachable!(),
        }
        fixture.write_binding(&binding, case == "seq");
        fixture.reject_without_effects(&stamp, &watch).await;
        fixture.positive().await;
        eprintln!("S1_BINDING_PUBLICATION case={case} old_callbacks=0");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_saved_idle_rejects_historical_and_recovered_candidate_trace_changes_pg() {
    for (offset, case) in [
        "canonical-nonce",
        "historical-nonce",
        "host",
        "runtime",
        "inflight",
    ]
    .into_iter()
    .enumerate()
    {
        let fixture =
            RecoveryFixture::connect_and_migrate_fixture(69_185_180 + offset as u64).await;
        let (stamp, watch) = fixture.publication_control().await;
        match case {
            "canonical-nonce" => {
                fixture.marker(
                    &fixture.canonical_name,
                    "spawn_nonce",
                    b"changed-opaque-nonce",
                );
            }
            "historical-nonce" => {
                fixture.marker(
                    &fixture.old_name,
                    "spawn_nonce",
                    b"changed-historical-nonce",
                );
            }
            "host" => {
                fixture.marker(&fixture.old_name, "host_kind", b"tmux");
            }
            "runtime" => {
                fixture.marker(
                    &fixture.old_name,
                    tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
                    b"claude_tui",
                );
            }
            "inflight" => fixture.inflight("new-inflight-nonce"),
            _ => unreachable!(),
        }
        fixture.reject_without_effects(&stamp, &watch).await;
        if case == "inflight" {
            fixture.hold("session_identity_unresolved").await;
        } else {
            fixture.positive().await;
        }
        eprintln!("S1_FILE_PUBLICATION case={case} old_callbacks=0");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_saved_inflight_hold_also_rejects_nonce_only_rewrite_pg() {
    let fixture = RecoveryFixture::connect_and_migrate_fixture(69_185_190).await;
    fixture.queue_sentinel().await;
    fixture.marker(
        &fixture.canonical_name,
        "spawn_nonce",
        b"stable-spawn-nonce",
    );
    fixture.inflight("before-inflight-nonce");
    let (answer, stamp, watch) = fixture.read().await;
    assert_eq!(
        answer,
        observed(Activity::Unknown, "session_identity_unresolved")
    );
    let calls = AtomicUsize::new(0);
    assert!(
        stamp
            .publish(&watch, || calls.fetch_add(1, Ordering::SeqCst))
            .await
            .is_some()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let path = fixture.inflight_path();
    let mut row: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    row["turn_nonce"] = serde_json::json!("after-inflight-nonce");
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    fixture.reject_without_effects(&stamp, &watch).await;
    fixture.hold("session_identity_unresolved").await;
}
