//! Output-origin fixtures other custody tests share: a staged delivery whose origin is sealed.
use super::*;
use crate::db::{auto_queue::test_support::TestPostgresDb, intake_outbox as outbox};
use crate::services::cluster::intake_router_hook::owner_record::{
    custody, input_start, transfer_owner_in_tx,
};
use serde_json::json;

pub(crate) fn payload(channel: &str, message: &str, gateway: &str) -> outbox::InsertPendingPayload {
    outbox::InsertPendingPayload {
        target_instance_id: gateway.into(),
        forwarded_by_instance_id: gateway.into(),
        required_labels: json!([]),
        execution_requirements: json!({}),
        attachment_refs: json!([]),
        channel_id: channel.into(),
        user_msg_id: message.into(),
        replay_source_message_ids: Some(vec![message.into()]),
        request_owner_id: "100".into(),
        request_owner_name: Some("operator".into()),
        user_text: format!("original {message}"),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "custody-agent".into(),
        provider: "claude".into(),
    }
}
struct RuntimeRoot {
    _root: tempfile::TempDir,
    _config: crate::config::TestEnvVarGuard,
    previous: Option<std::ffi::OsString>,
    _live: crate::config_live_reload::TestLiveGuard,
}
impl RuntimeRoot {
    fn new() -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(lock);
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.yaml");
        std::fs::write(&config_path, "server: {}\n").unwrap();
        let config_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CONFIG",
            &config_path,
        );
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        // SAFETY: the process-wide test environment lock is held until Drop restores the root.
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
        }
        let mut config = crate::config::Config::default();
        config.data.dir = root.path().join("data");
        crate::config_live_reload::install(config);
        Self {
            _live: live,
            _root: root,
            _config: config_guard,
            previous,
        }
    }
}
impl Drop for RuntimeRoot {
    fn drop(&mut self) {
        // SAFETY: the environment lock still belongs to this fixture.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

pub(crate) struct FixtureDb {
    pub(crate) _db: TestPostgresDb,
    _root: RuntimeRoot,
}

impl FixtureDb {
    pub(crate) async fn single_pool(&self) -> PgPool {
        self._db.connect_and_migrate_with_max_connections(1).await
    }
}

pub(crate) async fn fixture(channel: &str) -> (FixtureDb, PgPool, Origin) {
    let root = RuntimeRoot::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider) VALUES('custody-agent','custody','claude')")
        .execute(&pool)
        .await
        .unwrap();
    let p = payload(channel, "90001", "gateway");
    let delivery = custody::stage(
        &pool,
        custody::StageRequest {
            payload: &p,
            home_revision: 0,
            wait_generation: None,
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    assert!(
        custody::promote(
            &pool,
            delivery,
            "executor",
            None,
            custody::SwitchReadiness::Wait
        )
        .await
        .unwrap()
    );
    let row =
        outbox::claim_pending_for_target_except(&pool, "executor", "claude", "worker-token", &[])
            .await
            .unwrap()
            .unwrap();
    assert_eq!(row.id, delivery);
    assert!(
        outbox::mark_accepted(&pool, delivery, "worker-token")
            .await
            .unwrap()
    );
    assert!(
        outbox::mark_spawned(&pool, delivery, "worker-token")
            .await
            .unwrap()
    );
    let start = input_start::prepare(
        &pool,
        input_start::StartRequest {
            delivery_id: delivery,
            executor: "executor",
            claim_owner: "worker-token",
            incarnation: "origin-runtime",
            frame: "input",
            native_binding: &json!({"native":"original","nonce":"fixed"}),
        },
    )
    .await
    .unwrap();
    let origin = start.output_origin();
    start
        .submit(
            "input",
            &json!({"native":"original","nonce":"fixed"}),
            |_| async {},
        )
        .await
        .unwrap();
    (
        FixtureDb {
            _db: db,
            _root: root,
        },
        pool,
        origin,
    )
}

pub(crate) async fn revoke(pool: &PgPool, origin: &Origin) {
    let mut tx = pool.begin().await.unwrap();
    let id = OwnerIdentity::new(&origin.provider, &origin.channel);
    custody::lock(&mut tx, &id).await.unwrap();
    let result = transfer_owner_in_tx(
        &mut tx,
        &id,
        &origin.executor,
        origin.generation,
        "returned-home",
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        super::super::TransferOutcome::Transferred { .. }
    ));
    tx.commit().await.unwrap();
}
