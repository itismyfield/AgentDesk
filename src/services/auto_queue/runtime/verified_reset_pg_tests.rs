use crate::config::TestEnvVarGuard;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::SharedData;
use crate::services::discord::admin_host_guard::{self, ManagedReset};
use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tui_prompt_dedupe as dedupe;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Fixture {
    pub root: tempfile::TempDir,
    pub tmux: String,
    pub channel: ChannelId,
    fake: FakeTmux,
    evidence: Vec<(std::path::PathBuf, Vec<u8>)>,
    pending_dispatch: std::cell::OnceCell<Vec<u8>>,
    _env: [TestEnvVarGuard; 2],
    _dedupe_lock: std::sync::MutexGuard<'static, ()>,
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

impl Fixture {
    pub(crate) fn new(channel: u64) -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (root, env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(Some(root.path()));
        let tmux = format!(
            "AgentDesk-codex-reset-race-{}",
            uuid::Uuid::new_v4().simple()
        );
        let fake = FakeTmux::install(&tmux);
        let mut evidence = Vec::new();
        for (extension, bytes) in [
            ("out", b"unread native prefix\n".as_slice()),
            ("jsonl", b"unread relay prefix\n".as_slice()),
            (
                crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
                br#"{"output_path":"pre-hook","last_offset":12}"#.as_slice(),
            ),
        ] {
            let path = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
                &tmux, extension,
            ));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            evidence.push((path, bytes.to_vec()));
        }
        let upload = root
            .path()
            .join("runtime/discord_uploads")
            .join(channel.to_string())
            .join("retained.txt");
        std::fs::create_dir_all(upload.parent().unwrap()).unwrap();
        std::fs::write(&upload, b"retained upload artifact").unwrap();
        evidence.push((upload, b"retained upload artifact".to_vec()));
        Self {
            root,
            tmux,
            channel: ChannelId::new(channel),
            fake,
            evidence,
            pending_dispatch: std::cell::OnceCell::new(),
            _env: env,
            _dedupe_lock: dedupe_lock,
            _env_lock: env_lock,
        }
    }

    pub(crate) async fn shared(
        &self,
        pool: &sqlx::PgPool,
    ) -> (Arc<SharedData>, Arc<HealthRegistry>, String) {
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.settings.write().await.provider = ProviderKind::Codex;
        let name = self.tmux.strip_prefix("AgentDesk-codex-").unwrap();
        let upload = self
            .root
            .path()
            .join("runtime/discord_uploads")
            .join(self.channel.get().to_string())
            .join("retained.txt");
        admin_host_guard::verified_reset_tests::seed_reset_race_session(
            &shared,
            self.channel,
            name,
            &upload,
        )
        .await;
        let key = format!(
            "codex/{}/{}:{}",
            shared.token_hash,
            crate::services::platform::hostname_short(),
            self.tmux
        );
        let channel = self.channel.get().to_string();
        let params = crate::db::dispatched_sessions::HookSessionUpsert {
            session_key: &key,
            instance_id: Some("test-node"),
            agent_id: Some("agent-1"),
            provider: "codex",
            status: "idle",
            session_info: None,
            model: None,
            tokens: None,
            cwd: None,
            active_dispatch_id: None,
            thread_channel_id: Some(&channel),
            channel_id: Some(&channel),
            claude_session_id: None,
            raw_provider_session_id: None,
            turn_start_nonce: None,
            dispatched_origin: false,
        };
        let identity = crate::db::dispatched_session_canonical_identity::CanonicalSessionIdentity {
            kind: crate::db::dispatched_session_canonical_identity::SessionIdentityKind::DiscordChannel,
            discord_token_hash: &shared.token_hash, channel_id: &channel,
        };
        crate::db::dispatched_session_canonical_identity::upsert_hook_session_with_identity_pg(
            pool,
            params,
            Some(identity),
        )
        .await
        .unwrap();
        let registry = Arc::new(HealthRegistry::new());
        registry.register("codex".into(), shared.clone()).await;
        admin_host_guard::verified_reset_tests::seed_accepted_queue(&shared, self.channel).await;
        self.pending_dispatch
            .set(
                admin_host_guard::verified_reset_tests::pending_dispatch_bytes(
                    &shared,
                    self.channel,
                ),
            )
            .unwrap();
        shared.overrides.session_reset_pending.insert(self.channel);
        shared
            .overrides
            .codex_goals_session_reset_pending
            .insert(self.channel);
        (shared, registry, key)
    }

    pub(crate) fn pin_after_legacy_verdict(&self) -> Arc<AtomicUsize> {
        let (name, root, channel) = (
            self.tmux.clone(),
            self.root.path().to_owned(),
            self.channel.get(),
        );
        let count = Arc::new(AtomicUsize::new(0));
        let pinned = count.clone();
        admin_host_guard::before_verified_clear_recheck_for_tests(channel, move || {
            admin_host_guard::verified_reset_tests::pin_verified_execution(&name, channel, &root);
            pinned.fetch_add(1, Ordering::SeqCst);
        });
        count
    }

    pub(crate) fn queue_bytes(&self, shared: &SharedData) -> Vec<u8> {
        std::fs::read(
            self.root
                .path()
                .join("runtime/discord_pending_queue/codex")
                .join(&shared.token_hash)
                .join(format!("{}.json", self.channel.get())),
        )
        .unwrap()
    }

    pub(crate) async fn assert_unchanged(&self, shared: &SharedData, queue: &[u8]) {
        assert_eq!(
            admin_host_guard::verified_reset_tests::accepted_queue_len(shared, self.channel).await,
            1
        );
        assert_eq!(self.queue_bytes(shared), queue);
        assert_eq!(
            admin_host_guard::verified_reset_tests::pending_dispatch_bytes(shared, self.channel),
            *self.pending_dispatch.get().unwrap()
        );
        assert!(
            shared
                .overrides
                .session_reset_pending
                .contains(&self.channel)
        );
        assert!(
            shared
                .overrides
                .codex_goals_session_reset_pending
                .contains(&self.channel)
        );
        let upload = self
            .root
            .path()
            .join("runtime/discord_uploads")
            .join(self.channel.get().to_string())
            .join("retained.txt");
        admin_host_guard::verified_reset_tests::assert_reset_race_session(
            shared,
            self.channel,
            &upload,
        )
        .await;
        for (path, bytes) in &self.evidence {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
        }
        let dedupe::binding_context::SpawnNonceMarker::Known(nonce) =
            dedupe::binding_context::observe_spawn_nonce_marker(&self.tmux)
        else {
            panic!("pin must persist its nonce");
        };
        let context = dedupe::binding_context::execution_context("codex", &nonce).unwrap();
        assert_eq!(context.source_policy.as_deref(), Some("verified"));
        let calls = self.fake.take_calls();
        assert!(
            calls
                .iter()
                .all(|call| !call.starts_with("kill-session") && !call.starts_with("send-keys")),
            "no kill or next input: {calls:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        dedupe::reset_state_for_tests();
        dedupe::binding_events::set_test_root(None);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn autoqueue_actual_reset_rechecks_pin_after_legacy_verdict_pg() {
    let fixture = Fixture::new(6_845_122);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_cdx) VALUES ('agent-1', 'verified reset', 'codex', $1)")
        .bind(fixture.channel.get().to_string()).execute(&pool).await.unwrap();
    let (shared, registry, _key) = fixture.shared(&pool).await;
    sqlx::query("INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map) VALUES ('agent-1', 0, $1::jsonb)")
        .bind(serde_json::json!({"0":fixture.channel.get().to_string()}).to_string()).execute(&pool).await.unwrap();
    let queue = fixture.queue_bytes(&shared);
    let pin = fixture.pin_after_legacy_verdict();
    let cleared = super::clear_slot_threads_for_slot_pg(Some(registry), &pool, "agent-1", 0)
        .await
        .unwrap();
    assert_eq!(cleared, 1, "actual preflight admitted the legacy session");
    let observed =
        super::slot_reset_host_pg_tests::runtime_clears_after_done(&[fixture.channel.get()]).await;
    assert!(
        matches!(observed.as_slice(), [(channel, Some(ManagedReset::Refused(_)))] if *channel == fixture.channel.get()),
        "apply must refuse the newly pinned execution: {observed:?}"
    );
    assert_eq!(pin.load(Ordering::SeqCst), 1);
    fixture.assert_unchanged(&shared, &queue).await;
    pool.close().await;
    db.drop().await;
}
