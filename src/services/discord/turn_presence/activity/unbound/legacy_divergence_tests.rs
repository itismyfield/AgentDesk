use super::review_tests::Rig;
use super::*;
use crate::db::dispatched_session_canonical_identity::upsert_legacy_hook_session_pg;
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::settled_owner_observation;
use crate::services::platform::tmux::session_inventory::tests::ChannelCommand;
use std::path::PathBuf;

pub(super) struct LegacyFixture {
    pub(super) rig: Rig,
    pub(super) exact_name: String,
    pub(super) canonical_name: String,
    pub(super) exact_key: String,
    pub(super) canonical_key: String,
    pub(super) inventory: PathBuf,
    _command: ChannelCommand,
}

impl LegacyFixture {
    pub(super) async fn connect_and_migrate(id: u64) -> Self {
        let rig = Rig::new(id, false, false).await;
        let provider = ProviderKind::Claude;
        let exact_name = provider.build_tmux_session_name(&format!("s1e-{id}"));
        let canonical_name = provider.build_tmux_session_name(&format!("before-clear-{id}"));
        let key = |name: &str| {
            adk_session::build_namespaced_session_key(&rig.shared.token_hash, &provider, name)
        };
        let (exact_key, canonical_key) = (key(&exact_name), key(&canonical_name));
        let pool = rig.shared.pg_pool.as_ref().unwrap();
        inflight::seed_session_row_hashed(pool, &canonical_key, id, &rig.shared.token_hash, None)
            .await;
        let channel = id.to_string();
        let shell = HookSessionUpsert {
            session_key: &exact_key,
            instance_id: Some("test-node"),
            agent_id: None,
            provider: "claude",
            status: "idle",
            session_info: None,
            model: None,
            tokens: Some(0),
            cwd: None,
            active_dispatch_id: None,
            thread_channel_id: None,
            channel_id: Some(&channel),
            claude_session_id: None,
            raw_provider_session_id: None,
            turn_start_nonce: None,
            dispatched_origin: false,
        };
        upsert_legacy_hook_session_pg(pool, shell).await.unwrap();
        let inventory = rig.root.path().join("inventory.txt");
        std::fs::write(&inventory, "").unwrap();
        let executable = rig.root.path().join("inventory-reader");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nexec /bin/cat {}\n",
                crate::services::process::shell_escape(inventory.to_str().unwrap())
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = ChannelCommand::new(id, executable);
        Self {
            rig,
            exact_name,
            canonical_name,
            exact_key,
            canonical_key,
            inventory,
            _command: command,
        }
    }

    fn pool(&self) -> &sqlx::PgPool {
        self.rig.shared.pg_pool.as_ref().unwrap()
    }

    async fn reading(&self) -> (Observed, Stamp, Arc<Mutex<Watch>>) {
        let watch = Arc::new(Mutex::new(Watch::default()));
        let (answer, stamp) = settled_owner_observation(
            "LEGACY_DIVERGENCE superseded_retries",
            || {
                read(
                    &self.rig.shared,
                    &ProviderKind::Claude,
                    self.rig.channel,
                    watch.clone(),
                )
            },
            |sample| sample.0.reason == "superseded",
        )
        .await;
        (answer, stamp, watch)
    }

    pub(super) async fn observation(&self) -> Observed {
        self.reading().await.0
    }

    async fn promotion_blocked(&self) -> bool {
        crate::services::discord::router::hosted_tui_promote_readiness_blocked(
            &self.rig.shared,
            &ProviderKind::Claude,
            self.rig.channel,
        )
        .await
    }

    async fn set_status(&self, key: &str, status: &str) {
        sqlx::query("UPDATE sessions SET status = $2 WHERE session_key = $1")
            .bind(key)
            .bind(status)
            .execute(self.pool())
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cleared_local_legacy_rows_become_idle_pg() {
    let fixture = LegacyFixture::connect_and_migrate(69_186_101).await;
    let _confirmed =
        crate::services::tui_o::turn_mode::TestConfirmation::new(fixture.rig.channel.get());
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM sessions s")
            .fetch_one(fixture.pool())
            .await
            .unwrap();
    let (answer, stamp, watch) = fixture.reading().await;
    let snapshot = stamp.snapshot.as_ref().unwrap();
    assert_ne!(
        snapshot.exact, snapshot.canonical,
        "fixture must reproduce divergent rows"
    );
    for row in [&snapshot.exact, &snapshot.canonical] {
        assert!(matches!(row, HostedLookup::Found(found) if found.record == HostedRecord::Legacy));
    }
    assert_eq!(answer, observed(Activity::Idle, "no_session"));
    assert!(stamp.publish(&watch, || ()).await.is_some());
    assert!(
        !crate::services::discord::router::hosted_tui_promote_readiness_blocked(
            &fixture.rig.shared,
            &ProviderKind::Claude,
            fixture.rig.channel,
        )
        .await,
        "read-only promotion still held the cleared channel"
    );
    assert!(
        crate::services::discord::host_defer_gate::channel_session_deferred(
            &fixture.rig.shared,
            &ProviderKind::Claude,
            fixture.rig.channel.get(),
            &fixture.exact_name,
        )
        .await,
        "the absence witness must not grant teardown authority"
    );
    fixture
        .rig
        .shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&fixture.rig.channel)
        .unwrap()
        .channel_name = None;
    assert!(
        !fixture.promotion_blocked().await,
        "registered fallback still held a nameless core"
    );
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(s) ORDER BY id) FROM sessions s")
            .fetch_one(fixture.pool())
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "absence observation must not reconcile or clear rows"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn divergent_rows_keep_live_inflight_remote_and_active_turns_held_pg() {
    let fixture = LegacyFixture::connect_and_migrate(69_186_102).await;
    let _confirmed =
        crate::services::tui_o::turn_mode::TestConfirmation::new(fixture.rig.channel.get());
    assert!(!fixture.promotion_blocked().await);
    assert_eq!(fixture.observation().await.activity, Activity::Idle);
    for names in [
        format!("{}\n", fixture.exact_name),
        format!("{}\n", fixture.canonical_name),
        format!("{}\n{}\n", fixture.exact_name, fixture.canonical_name),
    ] {
        std::fs::write(&fixture.inventory, names).unwrap();
        assert_eq!(
            fixture.observation().await.activity,
            Activity::Unknown,
            "live candidate bypassed Hold"
        );
        assert!(
            fixture.promotion_blocked().await,
            "live candidate bypassed promotion Hold"
        );
    }
    std::fs::write(&fixture.inventory, "").unwrap();
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        fixture.rig.channel.get(),
    );
    let row = InflightTurnState::new(
        ProviderKind::Claude,
        fixture.rig.channel.get(),
        None,
        200,
        fixture.rig.channel.get() + 1,
        fixture.rig.channel.get() + 2,
        "inflight input".into(),
        None,
        Some(fixture.canonical_name.clone()),
        None,
        None,
        0,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    assert_eq!(
        fixture.observation().await.activity,
        Activity::Unknown,
        "inflight bypassed Hold"
    );
    assert!(
        fixture.promotion_blocked().await,
        "inflight bypassed promotion Hold"
    );
    std::fs::remove_file(path).unwrap();
    for key in [&fixture.exact_key, &fixture.canonical_key] {
        fixture.set_status(key, "turn_active").await;
        assert_eq!(
            fixture.observation().await.activity,
            Activity::Unknown,
            "active row bypassed Hold"
        );
        assert!(
            fixture.promotion_blocked().await,
            "active row bypassed promotion Hold"
        );
        fixture.set_status(key, "idle").await;
    }
    let remote = format!(
        "claude/{}/other-node:{}",
        fixture.rig.shared.token_hash, fixture.canonical_name
    );
    sqlx::query("UPDATE sessions SET session_key = $2 WHERE session_key = $1")
        .bind(&fixture.canonical_key)
        .bind(remote)
        .execute(fixture.pool())
        .await
        .unwrap();
    assert_eq!(
        fixture.observation().await.activity,
        Activity::Unknown,
        "remote canonical row bypassed Hold"
    );
    assert!(
        fixture.promotion_blocked().await,
        "remote row bypassed promotion Hold"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn divergent_row_absence_revalidates_successor_and_canonical_trace_pg() {
    let fixture = LegacyFixture::connect_and_migrate(69_186_103).await;
    let (answer, stamp, watch) = fixture.reading().await;
    assert_eq!(answer.activity, Activity::Idle);
    assert!(stamp.publish(&watch, || ()).await.is_some());
    fixture
        .set_status(&fixture.canonical_key, "turn_active")
        .await;
    assert!(
        stamp.publish(&watch, || ()).await.is_none(),
        "successor row reused old Idle"
    );
    fixture.set_status(&fixture.canonical_key, "idle").await;
    let (_, stamp, watch) = fixture.reading().await;
    std::fs::write(&fixture.inventory, format!("{}\n", fixture.canonical_name)).unwrap();
    assert!(
        stamp.publish(&watch, || ()).await.is_none(),
        "canonical session appeared across publication"
    );
    std::fs::write(&fixture.inventory, "").unwrap();
    let (_, stamp, watch) = fixture.reading().await;
    let marker = marker_path(&fixture.canonical_name, "host_kind");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(marker, "process").unwrap();
    assert!(
        stamp.publish(&watch, || ()).await.is_none(),
        "canonical process trace reused old Idle"
    );
    assert_eq!(fixture.observation().await.activity, Activity::Unknown);
}

#[path = "legacy_divergence_entry_tests.rs"]
mod entry_tests;
