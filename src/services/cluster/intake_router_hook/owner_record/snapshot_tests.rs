use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::intake_router_hook::owner_record::{self, output, plan};

pub(crate) struct Fixture {
    pub db: TestPostgresDb,
    pub pool: sqlx::PgPool,
    pub channel: String,
}
impl Fixture {
    pub(crate) async fn new(channel: &str) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_id,default_execution_node_id) VALUES('custody-agent','fixture','claude',$1,'mac-a')")
            .bind(channel).execute(&pool).await.unwrap();
        let now: i64 = sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM NOW())*1000)::BIGINT")
            .fetch_one(&pool)
            .await
            .unwrap();
        for instance in ["mac-a", "mac-b"] {
            let providers = json!({"claude":{"cli_usable":true,"version":"1","failure":null,"credential_profiles":{"default":true},"authentication_verified":false,"quota_verified":false},
                "codex":{"cli_usable":true,"version":"1","failure":null,"credential_profiles":{"default":true},"authentication_verified":false,"quota_verified":false}});
            let caps = json!({"intake_worker":{"enabled":true,"providers":["claude","codex"],"features":["execution_requirements_v1","preserve_on_cancel_v1"]},
                "intake_poller":{"claude":now,"codex":now},"routing_owner":{"control":["claude","codex"],"rest":["claude","codex"]},"execution_capacity":{"accepting":false,"occupied":999,"capacity":0},
                "execution_readiness":{"schema":1,"boot_id":"fixture","observed_at_ms":now,"expires_at_ms":now+120000,"os":"macos","arch":"aarch64","runtime_profile":"worker","release":{},
                "providers":providers,"tools":{"git":true,"tmux":true},"repositories":{"owner/repo":true},"backends":["tmux","process"],"disk_free_bytes":null}});
            sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,capabilities) VALUES($1,'online',NOW(),$2)").bind(instance).bind(caps).execute(&pool).await.unwrap();
        }
        Self {
            db,
            pool,
            channel: channel.into(),
        }
    }
    pub(crate) fn context(&self) -> Context {
        Context {
            policy_channel: self.channel.clone(),
            pair: ["mac-a".into(), "mac-b".into()],
            gateway: Some("mac-a".into()),
            auth_profile: "default".into(),
            lease_seconds: 15,
        }
    }
    pub(crate) async fn observe(&self, id: &OwnerIdentity, context: &Context) -> Shared {
        let mut tx = self.pool.begin().await.unwrap();
        let observed = read_on_tx(&mut tx, id, context).await.unwrap();
        tx.commit().await.unwrap();
        observed
    }
    pub(crate) async fn home(&self, id: &OwnerIdentity, value: Option<&str>) {
        let previous = crate::db::channel_home_preferences::read(&self.pool, id)
            .await
            .unwrap();
        let mut tx = self.pool.begin().await.unwrap();
        crate::db::channel_home_preferences::write_on_tx(
            &mut tx,
            id,
            previous.map_or(0, |r| r.revision),
            uuid::Uuid::new_v4(),
            value,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    /// A stored bundle for one source message; routing must carry this exact reference.
    pub(crate) async fn bundle_ref(
        &self,
        channel: &str,
        message: &str,
    ) -> crate::services::cluster::attachment_transfer::uploads::BundleRef {
        use crate::services::cluster::attachment_transfer::{self as attachments, store};
        let identity = attachments::AttachmentMessageIdentity {
            provider: "claude".into(),
            channel_id: channel.into(),
            user_msg_id: message.into(),
        };
        let bytes = format!("bundle for {message}").into_bytes();
        let bundle = attachments::validate_attachment_bundle_v1(
            attachments::AttachmentBundleV1 {
                version: attachments::ATTACHMENT_BUNDLE_V1,
                identity: identity.clone(),
                source_attachment_count: 1,
                entries: vec![attachments::AttachmentEntryV1 {
                    filename: "routed.txt".into(),
                    sha256: attachments::attachment_sha256_hex(&bytes),
                    bytes,
                }],
            },
            &identity,
        )
        .unwrap();
        store::put(&self.pool, &bundle).await.unwrap()
    }
    pub(crate) async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[tokio::test]
async fn shared_snapshot_symmetric_pg_selection_ignores_capacity_and_gateway() {
    let f = Fixture::new("73369101").await;
    let mut checked = 0;
    for provider in ["claude", "codex"] {
        let id = OwnerIdentity::new(provider, &f.channel);
        for home in ["mac-a", "mac-b"] {
            f.home(&id, Some(home)).await;
            for gateway in ["mac-a", "mac-b"] {
                for offline in [false, true] {
                    let mut context = f.context();
                    context.gateway = Some(gateway.into());
                    let h = usize::from(home == "mac-b");
                    sqlx::query("UPDATE worker_nodes SET capabilities=jsonb_set(capabilities,'{routing_owner,control}',CASE WHEN instance_id=$1 AND $2 THEN '[]'::jsonb ELSE '[\"claude\",\"codex\"]'::jsonb END)")
                        .bind(home).bind(offline).execute(&f.pool).await.unwrap();
                    let observed = f.observe(&id, &context).await;
                    let expected = if offline {
                        &context.pair[1 - h]
                    } else {
                        &context.pair[h]
                    };
                    assert_eq!(observed.target(), Target::Selected(expected));
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 16);
    let owners: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_session_owners")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    let deliveries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        (owners, deliveries),
        (0, 0),
        "a snapshot must not admit or switch"
    );
    f.finish().await;
}

#[tokio::test]
async fn shared_snapshot_unknown_conflict_storage_and_stale_heartbeat_pg() {
    let f = Fixture::new("73369102").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    let context = f.context();
    sqlx::query("UPDATE worker_nodes SET capabilities=capabilities-'routing_owner' WHERE instance_id='mac-a'").execute(&f.pool).await.unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Held(fallback::Held::AvailabilityUnknown),
        "a node that never published routing ownership is unknown, not unavailable"
    );
    sqlx::query("UPDATE worker_nodes SET capabilities=jsonb_set(capabilities,'{routing_owner}','{\"control\":[\"claude\"],\"rest\":[\"codex\"]}') WHERE instance_id='mac-a'").execute(&f.pool).await.unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Selected("mac-b"),
        "an explicitly missing REST provider is unavailable"
    );
    sqlx::query("UPDATE worker_nodes SET capabilities=jsonb_set(capabilities,'{routing_owner,rest}','[\"claude\"]') WHERE instance_id='mac-a'").execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE worker_nodes SET capabilities=capabilities-'execution_readiness' WHERE instance_id='mac-a'").execute(&f.pool).await.unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Held(fallback::Held::AvailabilityUnknown),
        "unknown home readiness must not select the peer"
    );
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW()-INTERVAL '60 seconds' WHERE instance_id='mac-a'").execute(&f.pool).await.unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Selected("mac-b")
    );
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NULL WHERE instance_id='mac-a'")
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Held(fallback::Held::AvailabilityUnknown)
    );
    sqlx::query("INSERT INTO agents(id,name,discord_channel_id,default_execution_node_id) VALUES('conflict','conflict',$1,'mac-b')").bind(&f.channel).execute(&f.pool).await.unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Held(fallback::Held::HomeConflict)
    );
    let mut missing = context.clone();
    missing.policy_channel = "73369999".into();
    assert_eq!(
        f.observe(&OwnerIdentity::new("claude", "73369999"), &missing)
            .await
            .target(),
        Target::Held(fallback::Held::HomeUnknown)
    );
    let mut absent = context.clone();
    absent.gateway = None;
    assert_eq!(
        f.observe(&id, &absent).await.target(),
        Target::Held(fallback::Held::NoGateway)
    );
    let mut broken = f.pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE worker_nodes RENAME TO unavailable_worker_nodes")
        .execute(&mut *broken)
        .await
        .unwrap();
    let error = read_on_tx(&mut broken, &id, &context).await.err().unwrap();
    assert_eq!(
        error.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42P01"),
        "a failed shared snapshot query must return storage failure, never a target"
    );
    broken.rollback().await.unwrap();
    f.pool.close().await;
    assert!(
        f.pool.begin().await.is_err(),
        "a PG read failure cannot yield a target"
    );
    f.finish().await;
}

#[tokio::test]
async fn shared_snapshot_raw_parent_reset_and_workspace_auth_pg() {
    let f = Fixture::new("73369103").await;
    let parent = OwnerIdentity::new("claude", &f.channel);
    let id = OwnerIdentity::new("claude", "73369104");
    let mut context = f.context();
    f.home(&parent, Some("mac-b")).await;
    assert_eq!(
        f.observe(&id, &context).await.home.as_deref(),
        Some("mac-b")
    );
    f.home(&id, Some("mac-a")).await;
    assert_eq!(
        f.observe(&id, &context).await.home.as_deref(),
        Some("mac-a")
    );
    f.home(&id, None).await;
    assert_eq!(
        f.observe(&id, &context).await.home.as_deref(),
        Some("mac-b")
    );
    context.auth_profile = "missing".into();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Held(fallback::Held::NeitherAvailable)
    );
    context.auth_profile = "default".into();
    sqlx::query("UPDATE agents SET execution_requirements='{\"repositories\":[\"owner/repo\"]}'")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE worker_nodes SET capabilities=jsonb_set(capabilities,'{execution_readiness,repositories,owner/repo}','false') WHERE instance_id='mac-b'").execute(&f.pool).await.unwrap();
    assert_eq!(
        f.observe(&id, &context).await.target(),
        Target::Selected("mac-a")
    );
    f.finish().await;
}

#[tokio::test]
async fn staged_plan_rechecks_parent_revision_and_preserves_original_payload_pg() {
    let f = Fixture::new("73369105").await;
    let parent = OwnerIdentity::new("claude", &f.channel);
    let id = OwnerIdentity::new("claude", "73369106");
    let context = f.context();
    f.home(&parent, Some("mac-a")).await;
    let observed = f.observe(&id, &context).await;
    let mut payload = output::tests::payload(id.raw_channel_id(), "source-third", "gateway");
    payload.reply_context = Some("원 reply context".into());
    payload.replay_source_message_ids = Some(vec![
        "source-first".into(),
        "source-second".into(),
        "source-third".into(),
    ]);
    use crate::services::cluster::attachment_transfer::{self as attachments, store};
    let identity = attachments::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: id.raw_channel_id().into(),
        user_msg_id: "source-third".into(),
    };
    let bytes = b"original attachment survives planning";
    let bundle = attachments::validate_attachment_bundle_v1(
        attachments::AttachmentBundleV1 {
            version: attachments::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![attachments::AttachmentEntryV1 {
                filename: "original.txt".into(),
                sha256: attachments::attachment_sha256_hex(bytes),
                bytes: bytes.to_vec(),
            }],
        },
        &identity,
    )
    .unwrap();
    let reference = store::put(&f.pool, &bundle).await.unwrap();
    payload.attachment_refs = json!([reference]);
    let delivery = custody::stage(
        &f.pool,
        custody::StageRequest {
            payload: &payload,
            home_revision: 0,
            wait_generation: None,
            grace_seconds: 77,
        },
    )
    .await
    .unwrap();
    assert!(
        sqlx::query("UPDATE intake_outbox SET routing_plan='{}' WHERE id=$1")
            .bind(delivery)
            .execute(&f.pool)
            .await
            .is_err(),
        "an actual held row must reject an incomplete plan"
    );
    let original: Value = sqlx::query_scalar(
        "SELECT to_jsonb(d)-'routing_plan'-'target_instance_id'-'updated_at' FROM intake_outbox d WHERE id=$1",
    )
    .bind(delivery)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    f.home(&parent, Some("mac-a")).await;
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        plan::save_on_tx(&mut tx, &id, delivery, &observed)
            .await
            .unwrap()
            .is_none(),
        "a same-target parent revision must invalidate the old plan"
    );
    tx.rollback().await.unwrap();
    let fresh = f.observe(&id, &context).await;
    let mut tx = f.pool.begin().await.unwrap();
    let stored = plan::save_on_tx(&mut tx, &id, delivery, &fresh)
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    assert_eq!(
        plan::read_on_tx(&mut tx, &id, delivery).await.unwrap(),
        Some(stored)
    );
    tx.rollback().await.unwrap();
    let preserved: Value = sqlx::query_scalar(
        "SELECT to_jsonb(d)-'routing_plan'-'target_instance_id'-'updated_at' FROM intake_outbox d WHERE id=$1",
    )
    .bind(delivery)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(original, preserved);
    let mut no_gateway = context.clone();
    no_gateway.gateway = None;
    let absent = f.observe(&id, &no_gateway).await;
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        plan::save_on_tx(&mut tx, &id, delivery, &absent)
            .await
            .unwrap()
            .is_none()
    );
    tx.rollback().await.unwrap();
    let before = f.observe(&id, &context).await;
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, &id).await.unwrap();
    owner_record::acquire_owner_in_tx(&mut tx, &id, "mac-a", |_| true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        plan::save_on_tx(&mut tx, &id, delivery, &before)
            .await
            .unwrap()
            .is_none(),
        "an owner change must invalidate the saved observation"
    );
    tx.rollback().await.unwrap();
    f.finish().await;
}
