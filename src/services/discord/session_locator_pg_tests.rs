use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::{
    ChannelId,
    health::HealthRegistry,
    herdr_terminate::{test_queue, test_turn},
};
use crate::services::platform::tmux::session_inventory::tests::ChannelNamespace;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use std::{
    process::{Command, Output},
    sync::Arc,
};

struct SessionCleanup {
    program: std::path::PathBuf,
    names: [String; 2],
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = Command::new(&self.program)
                .args(["kill-session", "-t", &format!("={name}")])
                .output();
        }
    }
}

pub(crate) struct PromotionRig {
    pub(crate) pool: PgPool,
    pub(crate) shared: Arc<SharedData>,
    pub(crate) registry: Arc<HealthRegistry>,
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) old: String,
    pub(crate) channel: ChannelId,
    pub(crate) id: i64,
    _sessions: SessionCleanup,
    pub(crate) root: tempfile::TempDir,
    db: TestPostgresDb,
    program: std::path::PathBuf,
    _namespace: ChannelNamespace,
    _config: crate::config::TestEnvVarGuard,
    _path: crate::config::TestEnvVarGuard,
    _root: crate::config::TestEnvVarGuard,
    _env: crate::config::test_env_lock::SharedTestEnvLockGuard,
}
impl PromotionRig {
    pub(crate) async fn new(channel: u64) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().unwrap();
        let root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &root.path().join("unused-config"),
        );
        let binary = crate::services::platform::binary_resolver::runtime_command("tmux").unwrap();
        let socket = root.path().join("isolated.sock");
        let program = root.path().join("tmux");
        let quote = crate::services::process::shell_escape;
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nexec {} -S {} -f /dev/null \"$@\"\n",
                quote(&binary.get_program().to_string_lossy()),
                quote(&socket.to_string_lossy())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(root.path());
        let namespace = ChannelNamespace::new(channel, socket);
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
        let old_name = format!("s2-old-{channel}");
        let name = format!("스마트홈-cc-{channel}");
        let old = adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Claude,
            &old_name,
        );
        let key = adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Claude,
            &name,
        );
        let id = sqlx::query_scalar("INSERT INTO sessions(session_key, provider, identity_kind, discord_token_hash, channel_id, active_turn_nonce, status) VALUES ($1, 'claude', 'discord_channel', $2, $3, 'old', 'idle') RETURNING id")
            .bind(&old).bind(&shared.token_hash).bind(channel.to_string()).fetch_one(&pool).await.unwrap();
        let rig = Self {
            pool,
            shared,
            registry,
            key,
            name: name.clone(),
            old,
            channel: ChannelId::new(channel),
            id,
            _sessions: SessionCleanup {
                program: program.clone(),
                names: [old_name.clone(), name.clone()],
            },
            root,
            db,
            program,
            _namespace: namespace,
            _config: config,
            _path: path,
            _root: root_guard,
            _env: env,
        };
        assert!(
            rig.raw(&["new-session", "-d", "-s", &old_name, "cat"])
                .status
                .success()
        );
        assert!(!super::unix::observe_old(id, &rig.old, channel));
        assert!(
            rig.raw(&["kill-session", "-t", &format!("={old_name}")])
                .status
                .success()
        );
        assert!(super::unix::observe_old(id, &rig.old, channel));
        let scope = RegistrationScope::enter(
            Some(&rig.pool),
            &rig.shared.token_hash,
            &ProviderKind::Claude,
            channel,
            Some(&rig.name),
            Some("new"),
            Some(false),
        )
        .unwrap();
        let prepared = PreparedIncarnation::create(BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: rig.name.clone(),
            channel_id: Some(channel),
            owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        })
        .unwrap();
        let marker = prepared
            .tmux_identity()
            .unwrap()
            .environment_assignment()
            .unwrap();
        let program = rig.root.path().join("synthetic-provider.py");
        std::fs::write(&program, r#"import json, os, pathlib
root = pathlib.Path(__file__).parent
fifo = root / "input.fifo"
os.mkfifo(fifo)
fd = os.open(fifo, os.O_RDWR)
(root / "provider.ready").write_text("ready")
with os.fdopen(fd) as stream:
    for line in stream:
        value = json.loads(line)
        if value.get("type") == "control_request" and value.get("request", {}).get("subtype") == "interrupt":
            (root / "interrupt.received").write_text("received")
            break
"#).unwrap();
        assert!(
            rig.raw(&[
                "new-session",
                "-d",
                "-s",
                &rig.name,
                "-e",
                &marker,
                "python3",
                program.to_str().unwrap(),
                "tmux-wrapper"
            ])
            .status
            .success()
        );
        crate::services::tmux_common::write_tmux_owner_marker(&rig.name).unwrap();
        crate::services::tmux_common::host_marker::record_tmux_host_marker(&rig.name);
        prepared
            .finish_spawn(crate::services::discord::stamp_spawn_markers(
                &rig.name,
                Some(&prepared),
            ))
            .unwrap();
        drop(scope);
        let stored: (String, Option<String>) =
            sqlx::query_as("SELECT session_key, active_turn_nonce FROM sessions WHERE id = $1")
                .bind(id)
                .fetch_one(&rig.pool)
                .await
                .unwrap();
        assert_eq!(stored, (rig.key.clone(), Some("new".into())));
        assert!(rig.present());
        rig
    }
    pub(crate) fn raw(&self, args: &[&str]) -> Output {
        Command::new(&self.program)
            .args(args)
            .env_remove("AGENTDESK_IDENTITY")
            .output()
            .unwrap()
    }
    pub(crate) fn present(&self) -> bool {
        self.raw(&["has-session", "-t", &format!("={}", self.name)])
            .status
            .success()
    }
    fn pane_pid(&self) -> u32 {
        let pane = self.raw(&[
            "display-message",
            "-p",
            "-t",
            &format!("={}:", self.name),
            "#{pane_pid}",
        ]);
        assert!(pane.status.success());
        std::str::from_utf8(&pane.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
    pub(crate) async fn queue(&self) -> Vec<u8> {
        let token =
            test_turn::start(&self.shared, self.channel, self.channel.get() + 1, "new").await;
        token.bind_claude_tmux_session(&self.name);
        let transcript = self.root.path().join("streaming.jsonl");
        std::fs::write(&transcript, "{\"type\":\"assistant\"}\n").unwrap();
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &self.name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind:
                    crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper,
                output_path: transcript.display().to_string(),
                relay_output_path: None,
                input_fifo_path: Some(self.root.path().join("input.fifo").display().to_string()),
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !self.root.path().join("provider.ready").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "synthetic wrapper ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        token.store_child_pid(self.pane_pid());
        let mut row = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Claude,
            self.channel.get(),
        )
        .unwrap();
        row.tmux_session_name = Some(self.name.clone());
        row.session_key = Some(self.key.clone());
        row.runtime_kind =
            Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper);
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        test_queue::queue_original(&self.shared, self.channel).await;
        self.queue_bytes()
    }
    fn queue_bytes(&self) -> Vec<u8> {
        std::fs::read(
            crate::services::discord::runtime_store::discord_pending_queue_root()
                .unwrap()
                .join("claude")
                .join(&self.shared.token_hash)
                .join(format!("{}.json", self.channel.get())),
        )
        .unwrap()
    }
    pub(crate) async fn assert_queue(&self, before: &[u8]) {
        assert_eq!(self.queue_bytes(), before);
        assert_eq!(
            test_queue::queued_texts(&self.shared, self.channel).await,
            [test_queue::ORIGINAL]
        );
    }
    pub(crate) async fn finish(self) {
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.name);
        let _ = self.raw(&["kill-session", "-t", &format!("={}", self.name)]);
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[tokio::test]
async fn key_promotion_created_incarnation_and_candidate_seams_pg() {
    let rig = PromotionRig::new(81190301).await;
    let keys = candidates(
        Some(&rig.pool),
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        "unrelated-preferred-name",
        Some(rig.channel.get()),
    )
    .unwrap();
    assert_eq!(keys[0], rig.key);
    let alias: i64 =
        sqlx::query_scalar("SELECT session_id FROM session_key_aliases WHERE session_key=$1")
            .bind(&rig.old)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    assert_eq!(alias, rig.id);
    let legacy = adk_session::build_legacy_session_key(&rig.name);
    sqlx::query("UPDATE sessions SET session_key=$1 WHERE id=$2")
        .bind(&legacy)
        .bind(rig.id)
        .execute(&rig.pool)
        .await
        .unwrap();
    let scope = RegistrationScope::enter(
        Some(&rig.pool),
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        rig.channel.get(),
        Some(&rig.name),
        Some("new"),
        Some(false),
    )
    .unwrap();
    assert!(
        rig.present(),
        "alternate wire format must keep the live execution"
    );
    assert_eq!(
        candidates(
            Some(&rig.pool),
            &rig.shared.token_hash,
            &ProviderKind::Claude,
            "another-preference",
            Some(rig.channel.get())
        )
        .unwrap()[0],
        legacy
    );
    drop(scope);
    rig.finish().await;
}

#[tokio::test]
async fn key_promotion_current_primary_force_kill_has_physical_effect_pg() {
    let rig = PromotionRig::new(81190302).await;
    let before = rig.queue().await;
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    config.policies.dir = rig.root.path().join("policies");
    std::fs::create_dir_all(&config.policies.dir).unwrap();
    let broadcast = crate::server::ws::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: Some(rig.pool.clone()),
        engine: crate::engine::PolicyEngine::new(&config).unwrap(),
        config: Arc::new(config),
        batch_buffer: crate::server::ws::spawn_batch_flusher(broadcast.clone()),
        broadcast_tx: broadcast,
        health_registry: Some(rig.registry.clone()),
        cluster_instance_id: None,
    };
    let result = crate::services::dispatched_sessions::force_kill_session_impl_with_reason(
        &state,
        &rig.key,
        false,
        "manual: isolated current primary",
    )
    .await;
    assert_eq!(result.0, axum::http::StatusCode::OK, "{:?}", result.1);
    assert!(
        !rig.present(),
        "current primary must kill the actual new session"
    );
    rig.assert_queue(&before).await;
    rig.finish().await;
}

#[tokio::test]
async fn key_promotion_current_primary_queue_cancel_preserves_input_pg() {
    let rig = PromotionRig::new(81190303).await;
    let before = rig.queue().await;
    sqlx::query("INSERT INTO task_dispatches(id,status) VALUES ('s2-dispatch','dispatched')")
        .execute(&rig.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET active_dispatch_id='s2-dispatch', status='turn_active', thread_channel_id=channel_id WHERE id=$1")
        .bind(rig.id)
        .execute(&rig.pool)
        .await
        .unwrap();
    crate::services::queue::QueueService::new(Some(rig.pool.clone()))
        .cancel_dispatch(Some(&rig.registry), "s2-dispatch")
        .await
        .unwrap();
    assert!(
        rig.root.path().join("interrupt.received").exists(),
        "the wrapper must receive the real interrupt"
    );
    assert!(
        !rig.present(),
        "queue cancel must end the new physical session"
    );
    rig.assert_queue(&before).await;
    rig.finish().await;
}

#[tokio::test]
async fn key_promotion_current_primary_thread_gc_keeps_live_thread_pg() {
    let rig = PromotionRig::new(81190304).await;
    sqlx::query("UPDATE sessions SET status='idle', thread_channel_id='81190304', last_heartbeat=NOW()-INTERVAL '2 hours' WHERE id=$1").bind(rig.id).execute(&rig.pool).await.unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let name = rig.name.clone();
    let current = rig.key.clone();
    let deleted = crate::db::dispatched_sessions::gc_stale_thread_sessions_with_probe_pg(
        &rig.pool,
        move |key| {
            assert_eq!(key, current);
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let presence = crate::services::platform::tmux::session_presence(&name);
            async move { presence }
        },
    )
    .await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "must probe the new actual name"
    );
    assert!(deleted.is_empty());
    assert!(rig.present());
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE id=$1 AND thread_channel_id='81190304'",
    )
    .bind(rig.id)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "protected thread row survives");
    rig.finish().await;
}

#[tokio::test]
async fn key_promotion_live_and_foreign_namespace_keep_primary_and_execution_pg() {
    let mut rig = PromotionRig::new(81190306).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id=$1")
            .bind(rig.id)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    let pid = rig.pane_pid();
    assert!(!super::unix::observe_old(
        rig.id,
        &rig.key,
        rig.channel.get()
    ));
    let preferred = adk_session::build_namespaced_session_key(
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        "other-preferred",
    );
    assert_eq!(
        channel_key(
            &rig.shared,
            &ProviderKind::Claude,
            rig.channel.get(),
            preferred.clone()
        )
        .await,
        Some(rig.key.clone())
    );
    assert!(
        RegistrationScope::enter(
            Some(&rig.pool),
            &rig.shared.token_hash,
            &ProviderKind::Claude,
            rig.channel.get(),
            Some("other-preferred"),
            Some("other"),
            Some(false)
        )
        .is_err()
    );
    use std::os::unix::fs::PermissionsExt;
    let failed_probe = rig.root.path().join("failed-inventory");
    std::fs::write(&failed_probe, "#!/bin/sh\nexit 2\n").unwrap();
    std::fs::set_permissions(&failed_probe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failed = crate::services::platform::tmux::session_inventory::tests::ChannelCommand::new(
        rig.channel.get(),
        failed_probe,
    );
    assert_eq!(
        channel_key(
            &rig.shared,
            &ProviderKind::Claude,
            rig.channel.get(),
            preferred.clone()
        )
        .await,
        Some(rig.key.clone())
    );
    assert_eq!(pid, rig.pane_pid());
    drop(failed);
    let other_socket = rig.root.path().join("another.sock");
    let displaced = std::mem::replace(
        &mut rig._namespace,
        ChannelNamespace::new(81190399, other_socket.clone()),
    );
    drop(displaced);
    let other = ChannelNamespace::new(rig.channel.get(), other_socket);
    assert_eq!(
        channel_key(
            &rig.shared,
            &ProviderKind::Claude,
            rig.channel.get(),
            preferred
        )
        .await,
        Some(rig.key.clone())
    );
    drop(other);
    assert_eq!(pid, rig.pane_pid());
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id=$1")
            .bind(rig.id)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    rig.finish().await;
}

#[tokio::test]
async fn key_promotion_synthetic_scope_does_not_publish_canonical_registration_pg() {
    let rig = PromotionRig::new(81190307).await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id=$1")
            .bind(rig.id)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    let scope = RegistrationScope::enter(
        Some(&rig.pool),
        &rig.shared.token_hash,
        &ProviderKind::Claude,
        rig.channel.get(),
        Some("synthetic-isolated"),
        Some("synthetic"),
        None,
    )
    .unwrap();
    let context = BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: "synthetic-isolated".into(),
        channel_id: Some(rig.channel.get()),
        owner_runtime_root: rig.root.path().display().to_string(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "scheduled".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    };
    created(&context).unwrap();
    drop(scope);
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id=$1")
            .bind(rig.id)
            .fetch_one(&rig.pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    assert!(rig.present());
    rig.finish().await;
}
