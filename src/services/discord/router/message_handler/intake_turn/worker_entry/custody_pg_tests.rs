use super::*;
use crate::db::{auto_queue::test_support::TestPostgresDb, intake_outbox as outbox};
use crate::services::cluster::intake_router_hook::owner_record::{custody, input_start};
use crate::services::discord::router::intake_dispatch::{
    IntakeAdmission, IntakeOrigin, IntakeSubmission, admit_text_intake,
};
use custody::SwitchReadiness::{Unavailable, Wait};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn payload(channel: &str, message: &str, gateway: &str) -> outbox::InsertPendingPayload {
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
async fn promote_fixture(
    pool: &sqlx::PgPool,
    delivery: i64,
    target: &str,
    expected: Option<(&str, i64)>,
    readiness: custody::SwitchReadiness,
) -> Result<bool, sqlx::Error> {
    if let Some((owner, generation)) = expected
        && readiness != Wait
    {
        use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, cleanup};
        let (provider, channel): (String, String) =
            sqlx::query_as("SELECT provider,channel_id FROM intake_outbox WHERE id=$1")
                .bind(delivery)
                .fetch_one(pool)
                .await?;
        sqlx::query(
            "UPDATE intake_outbox SET target_instance_id=$2 WHERE id=$1 AND routing_state='held'",
        )
        .bind(delivery)
        .bind(target)
        .execute(pool)
        .await?;
        cleanup::request(
            pool,
            &OwnerIdentity::new(&provider, &channel),
            (owner, generation),
            delivery,
            target,
            12,
        )
        .await?;
        sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3").bind(provider).bind(channel).bind(generation).execute(pool).await?;
    }
    custody::promote(pool, delivery, target, expected, readiness).await
}

async fn stage(
    pool: &sqlx::PgPool,
    p: &outbox::InsertPendingPayload,
    generation: Option<i64>,
    grace: i32,
) -> i64 {
    custody::stage(
        pool,
        custody::StageRequest {
            payload: p,
            home_revision: 0,
            wait_generation: generation,
            grace_seconds: grace,
        },
    )
    .await
    .unwrap()
}
async fn claim(pool: &sqlx::PgPool, target: &str) -> outbox::IntakeOutboxRow {
    outbox::claim_pending_for_target_except(pool, target, "claude", "worker-token", &[])
        .await
        .unwrap()
        .expect("actual claim must accept routed local as well as forwarded")
}
async fn seed(pool: &sqlx::PgPool) {
    sqlx::query("INSERT INTO agents(id,name,provider) VALUES('custody-agent','custody','claude')")
        .execute(pool)
        .await
        .unwrap();
}
fn request(row: &outbox::IntakeOutboxRow) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: Some(row.id),
        channel_id: ChannelId::new(row.channel_id.parse().unwrap()),
        user_msg_id: MessageId::new(row.user_msg_id.parse().unwrap()),
        source_message_ids: vec![MessageId::new(row.user_msg_id.parse().unwrap())],
        busy_followup_retry_user_msg_id: MessageId::new(row.user_msg_id.parse().unwrap()),
        request_owner: UserId::new(100),
        request_owner_name: "operator".into(),
        user_text: row.user_text.clone(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    }
}
async fn prepare(
    pool: &sqlx::PgPool,
    id: i64,
    executor: &str,
    frame: &str,
) -> input_start::OwnerStartPermit {
    let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    if status == "accepted" {
        assert!(
            outbox::mark_spawned(pool, id, "worker-token")
                .await
                .unwrap()
        );
    }
    input_start::prepare(
        pool,
        input_start::StartRequest {
            delivery_id: id,
            executor,
            claim_owner: "worker-token",
            incarnation: "runtime-one",
            frame,
            native_binding: &json!({"native":"exact-session","driver":"tui"}),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn custody_pg_old_open_accepted_spawned_local_claim_and_old_effect_zero_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    for (index, (old, new, status)) in [
        ("desk-A", "desk-B", "accepted"),
        ("desk-B", "desk-A", "spawned"),
        ("desk-A", "desk-A", "spawned"),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = (733610 + index).to_string();
        let m1 = payload(&channel, "101", old);
        let first = stage(&pool, &m1, None, 0).await;
        assert!(
            promote_fixture(&pool, first, old, None, Wait)
                .await
                .unwrap()
        );
        let row = claim(&pool, old).await;
        assert!(
            outbox::mark_accepted(&pool, row.id, "worker-token")
                .await
                .unwrap()
        );
        if status == "spawned" {
            assert!(
                outbox::mark_spawned(&pool, row.id, "worker-token")
                    .await
                    .unwrap()
            );
        }
        let stale = if status == "spawned" {
            Some(prepare(&pool, row.id, old, "m1").await)
        } else {
            assert!(
                input_start::prepare(
                    &pool,
                    input_start::StartRequest {
                        delivery_id: row.id,
                        executor: old,
                        claim_owner: "worker-token",
                        incarnation: "runtime-one",
                        frame: "m1",
                        native_binding: &json!({"native":"exact-session","driver":"tui"}),
                    }
                )
                .await
                .is_err(),
                "accepted alone does not permit raw input"
            );
            None
        };
        let m2 = payload(&channel, "102", new);
        let second = stage(&pool, &m2, Some(0), 300).await;
        assert!(
            !promote_fixture(&pool, second, new, Some((old, 0)), Unavailable)
                .await
                .unwrap(),
            "deadline cannot be skipped"
        );
        sqlx::query(
            "UPDATE intake_outbox SET routing_not_before=NOW()-INTERVAL '1 second' WHERE id=$1",
        )
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            promote_fixture(&pool, second, new, Some((old, 0)), Unavailable)
                .await
                .unwrap()
        );
        let count = AtomicUsize::new(0);
        if let Some(stale) = stale {
            assert!(
                stale
                    .submit(
                        "m1",
                        &json!({"native":"exact-session","driver":"tui"}),
                        |_| async {
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
        if old != new {
            assert!(
                outbox::claim_pending_for_target_except(&pool, old, "claude", "old-restart", &[])
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let row = claim(&pool, new).await;
        assert_eq!(
            (row.id, row.user_text.as_str()),
            (second, m2.user_text.as_str())
        );
        assert!(
            outbox::mark_accepted(&pool, row.id, "worker-token")
                .await
                .unwrap()
        );
        assert!(
            outbox::mark_spawned(&pool, row.id, "worker-token")
                .await
                .unwrap()
        );
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        assert!(
            matches!(
                worker_replay_admission(&shared, &request(&row)).await,
                Some(IntakeExecution::Deferred)
            ),
            "unconnected physical adapter stays held"
        );
        let entry_http = Arc::new(serenity::http::Http::new("fixture"));
        assert!(matches!(
            execute_intake_turn_core(&entry_http, &shared, "fixture", request(&row), vec![])
                .await
                .unwrap(),
            IntakeExecution::Deferred
        ));
        let mut intake = request(&row);
        intake.intake_outbox_id = None;
        let submission = IntakeSubmission {
            provider: crate::services::provider::ProviderKind::Claude,
            request: intake,
            origin: IntakeOrigin::LiveMessage,
            preserve_on_cancel: false,
            owner: None,
            has_nonportable_uploads: false,
            attachments: vec![],
            preloaded_uploads: vec![],
            voice_announcement: None,
        };
        let http = Arc::new(serenity::http::Http::new("fixture"));
        let deps = IntakeDeps {
            http: &http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared: &shared,
            token: "fixture",
        };
        assert!(
            matches!(admit_text_intake(&deps,&submission).await,IntakeAdmission::Forwarded{outbox_id,..} if outbox_id==second)
        );
        let frame = row.user_text.clone();
        let fresh = prepare(&pool, row.id, new, &frame).await;
        fresh
            .submit(
                &frame,
                &json!({"native":"exact-session","driver":"tui"}),
                |_| async {
                    assert_eq!(frame, m2.user_text);
                    count.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
            .unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "actual permitted callback, not receipt persistence"
        );
        let cleanup:bool=sqlx::query_scalar("SELECT routing_cleanup_deferred FROM intake_session_owners WHERE raw_channel_id=$1 AND generation=0").bind(&channel).fetch_one(&pool).await.unwrap();
        assert!(cleanup);
        let old_state:(String,String)=sqlx::query_as("SELECT d.routing_state,r.replay_disposition FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.id=$1").bind(first).fetch_one(&pool).await.unwrap();
        assert_eq!(old_state, ("superseded".into(), "withheld".into()));
        assert_eq!(
            stage(&pool, &m1, Some(0), 0).await,
            first,
            "old source never gets a new admission"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn custody_pg_start_guard_serializes_switch_and_rejects_old_native_and_frame_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    use crate::services::cluster::intake_router_hook::owner_record::{
        OwnerIdentity, transfer_owner_in_tx,
    };
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    for (index, case) in ["owner", "binding", "frame", "lock"]
        .into_iter()
        .enumerate()
    {
        let channel = (733630 + index).to_string();
        let p = payload(&channel, "201", "gateway");
        let id = stage(&pool, &p, None, 0).await;
        assert!(
            promote_fixture(&pool, id, "executor", None, Wait)
                .await
                .unwrap()
        );
        let row = claim(&pool, "executor").await;
        assert!(
            outbox::mark_accepted(&pool, row.id, "worker-token")
                .await
                .unwrap()
        );
        let permit = prepare(&pool, id, "executor", "prepared-frame").await;
        let count = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&count);
        let binding = json!({"native":"exact-session","driver":"tui"});
        if case == "owner" {
            let mut tx = pool.begin().await.unwrap();
            let identity = OwnerIdentity::new("claude", &channel);
            custody::lock(&mut tx, &identity).await.unwrap();
            transfer_owner_in_tx(&mut tx, &identity, "executor", 0, "next")
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        if case == "lock" {
            let (entered, inside) = tokio::sync::oneshot::channel();
            let (release, wait) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                permit
                    .submit("prepared-frame", &binding, |_| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        entered.send(()).unwrap();
                        wait.await.unwrap();
                    })
                    .await
            });
            inside.await.unwrap();
            let mut tx = pool.begin().await.unwrap();
            let available: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(OwnerIdentity::new("claude", &channel).advisory_key())
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert!(!available, "guard stays held through raw effect future");
            tx.rollback().await.unwrap();
            release.send(()).unwrap();
            task.await.unwrap().unwrap();
            assert_eq!(count.load(Ordering::SeqCst), 1);
        } else {
            let frame = if case == "frame" {
                "changed-frame"
            } else {
                "prepared-frame"
            };
            let binding = if case == "binding" {
                json!({"native":"replacement-session"})
            } else {
                binding
            };
            assert!(
                permit
                    .submit(frame, &binding, |_| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
                    .is_err()
            );
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn custody_pg_bundle_pin_put_load_cleanup_and_head_revision_cas_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    use crate::services::cluster::attachment_transfer::{self as attachments, store};
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let identity = attachments::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "733650".into(),
        user_msg_id: "301".into(),
    };
    let bytes = b"attachment crosses Macs";
    let bundle = attachments::validate_attachment_bundle_v1(
        attachments::AttachmentBundleV1 {
            version: attachments::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![attachments::AttachmentEntryV1 {
                filename: "portable.txt".into(),
                sha256: attachments::attachment_sha256_hex(bytes),
                bytes: bytes.to_vec(),
            }],
        },
        &identity,
    )
    .unwrap();
    let reference = store::put(&pool, &bundle).await.unwrap();
    let mut p = payload("733650", "301", "gateway");
    p.attachment_refs = json!([reference]);
    let original_root = tempfile::tempdir().unwrap();
    let queued = original_root.path().join("queued-input.json");
    std::fs::write(
        &queued,
        serde_json::to_vec(&json!({"body":p.user_text,"refs":p.attachment_refs})).unwrap(),
    )
    .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&queued).unwrap()).unwrap();
    p.user_text = saved["body"].as_str().unwrap().to_owned();
    p.attachment_refs = saved["refs"].clone();
    let id = stage(&pool, &p, None, 0).await;
    sqlx::query("UPDATE intake_attachment_bundles SET expires_at=NOW()-INTERVAL '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(store::cleanup(&pool).await.unwrap(), 0);
    assert_eq!(
        store::put(&pool, &bundle).await.unwrap(),
        reference,
        "put expiry sweep preserves pinned bytes"
    );
    assert_eq!(
        store::load(&pool, &reference)
            .await
            .unwrap()
            .as_bundle()
            .entries[0]
            .bytes,
        bytes
    );
    let later = payload("733650", "302", "gateway");
    let second = stage(&pool, &later, None, 0).await;
    assert!(
        !promote_fixture(&pool, second, "executor", None, Wait)
            .await
            .unwrap(),
        "oldest held first"
    );
    let old_path = original_root.path().to_path_buf();
    drop(original_root);
    assert!(
        !old_path.exists(),
        "old runtime and local queue are unavailable"
    );
    let restarted = db.connect_and_migrate().await;
    assert!(
        promote_fixture(&restarted, id, "executor", None, Wait)
            .await
            .unwrap()
    );
    let row = claim(&restarted, "executor").await;
    assert_eq!(row.id, id);
    let refs: Vec<attachments::uploads::BundleRef> =
        serde_json::from_value(row.attachment_refs).unwrap();
    assert_eq!(
        store::load(&restarted, &refs[0])
            .await
            .unwrap()
            .as_bundle()
            .entries[0]
            .bytes,
        bytes
    );
    assert!(
        outbox::mark_accepted(&restarted, id, "worker-token")
            .await
            .unwrap()
    );
    assert!(
        outbox::mark_spawned(&restarted, id, "worker-token")
            .await
            .unwrap()
    );
    let prepared = store::load(&restarted, &refs[0]).await.unwrap();
    let local = attachments::materialize::prepare(
        &[attachments::uploads::Upload::Bundle(refs[0].clone())],
        Some(&restarted),
    )
    .await
    .unwrap();
    let path = local.records[0]
        .split_once(" → ")
        .unwrap()
        .1
        .rsplit_once(" (")
        .unwrap()
        .0;
    let frame = format!("{}\n{}", row.user_text, local.records[0]);
    let permit = prepare(&restarted, id, "executor", &frame).await;
    let calls = AtomicUsize::new(0);
    permit
        .submit(
            &frame,
            &json!({"native":"exact-session","driver":"tui"}),
            |_| async {
                assert!(frame.starts_with(&p.user_text));
                assert_eq!(std::fs::read(path).unwrap(), bytes);
                assert_eq!(prepared.as_bundle().entries[0].bytes, bytes);
                assert_eq!(
                    prepared.as_bundle().entries[0].sha256,
                    attachments::attachment_sha256_hex(bytes)
                );
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let revised = payload("733651", "303", "gateway");
    let stale_revision = stage(&restarted, &revised, None, 0).await;
    let mut tx = restarted.begin().await.unwrap();
    crate::db::channel_home_preferences::write_on_tx(
        &mut tx,
        &crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new(
            "claude", "733651",
        ),
        0,
        uuid::Uuid::new_v4(),
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        !promote_fixture(&restarted, stale_revision, "executor", None, Wait)
            .await
            .unwrap(),
        "home revision is the only failed predicate on this ownerless channel"
    );
    restarted.close().await;
    pool.close().await;
    db.drop().await;
}

struct RuntimeRoot {
    _live: crate::config_live_reload::TestLiveGuard,
    root: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}
impl RuntimeRoot {
    fn new() -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(lock);
        let root = tempfile::tempdir().unwrap();
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
            root,
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

#[tokio::test]
async fn custody_pg_preexisting_unreceipted_open_delivery_is_preserved_not_replayed_pg() {
    use crate::services::cluster::intake_router_hook::owner_record::{
        OwnerIdentity, acquire_owner_in_tx,
    };
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    for (index, (status, released)) in [("accepted", false), ("spawned", false), ("spawned", true)]
        .into_iter()
        .enumerate()
    {
        let channel = (733670 + index).to_string();
        let identity = OwnerIdentity::new("claude", &channel);
        let mut tx = pool.begin().await.unwrap();
        custody::lock(&mut tx, &identity).await.unwrap();
        acquire_owner_in_tx(&mut tx, &identity, "old", |_| true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let old:i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,claim_owner) VALUES('old','gateway',$1,'401','100','old input','foreground','custody-agent','claude',$2,'gone-control') RETURNING id")
            .bind(&channel).bind(status).fetch_one(&pool).await.unwrap();
        if released {
            sqlx::query("UPDATE intake_session_owners SET status='released' WHERE provider='claude' AND raw_channel_id=$1").bind(&channel).execute(&pool).await.unwrap();
        }
        let p = payload(&channel, "402", "gateway");
        let fresh = stage(&pool, &p, Some(0), 0).await;
        assert!(
            promote_fixture(&pool, fresh, "home", Some(("old", 0)), Unavailable)
                .await
                .unwrap()
        );
        let row = claim(&pool, "home").await;
        assert_eq!(row.id, fresh);
        assert!(
            outbox::mark_accepted(&pool, fresh, "worker-token")
                .await
                .unwrap()
        );
        let permit = prepare(&pool, fresh, "home", "fresh").await;
        let count = AtomicUsize::new(0);
        permit
            .submit(
                "fresh",
                &json!({"native":"exact-session","driver":"tui"}),
                |_| async {
                    count.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let preserved: (String, String) =
            sqlx::query_as("SELECT status,replay_disposition FROM intake_outbox WHERE id=$1")
                .bind(old)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(preserved, ("unknown".into(), "withheld".into()));
        assert!(
            outbox::claim_pending_for_target_except(&pool, "old", "claude", "old-restart", &[])
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            sqlx::query("UPDATE intake_outbox SET status='pending' WHERE id=$1")
                .bind(old)
                .execute(&pool)
                .await
                .is_err(),
            "old source cannot rearm even with old control restored"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn custody_pg_duplicate_stage_ack_and_mixed_source_read_fail_closed_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let p = payload("733690", "501", "gateway");
    let (left, right) = tokio::join!(stage(&pool, &p, None, 0), stage(&pool, &p, None, 0));
    assert_eq!(left, right);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM intake_outbox WHERE routing_receipt_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let mut input =
        crate::services::discord::router::intake_dispatch::replay_admission::discord_input(
            "claude",
            733690,
            501,
            [501],
            &p.user_text,
        );
    assert!(
        custody::lookup_source(&pool, &input)
            .await
            .unwrap()
            .is_some()
    );
    input.sources.push("502".into());
    assert!(
        custody::lookup_source(&pool, &input).await.is_err(),
        "PG custody never acknowledges an absorbed source it does not own"
    );
    let row: outbox::IntakeOutboxRow = sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(left)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut mixed = request(&row);
    mixed.intake_outbox_id = None;
    mixed.source_message_ids.push(MessageId::new(502));
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::http::Http::new("fixture"));
    let deps = IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &shared,
        token: "fixture",
    };
    let submission = IntakeSubmission {
        provider: crate::services::provider::ProviderKind::Claude,
        request: mixed,
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: false,
        owner: None,
        has_nonportable_uploads: false,
        attachments: vec![],
        preloaded_uploads: vec![],
        voice_announcement: None,
    };
    assert!(matches!(
        admit_text_intake(&deps, &submission).await,
        IntakeAdmission::Blocked { .. }
    ));
    input.sources.pop();
    input.original_text.push_str(" changed");
    assert!(custody::lookup_source(&pool, &input).await.is_err());
    pool.close().await;
    assert!(custody::lookup_source(&pool, &input).await.is_err());
    db.drop().await;
}

async fn lifecycle_case(released: bool) {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let first = stage(&pool, &payload("733699", "601", "gateway"), None, 0).await;
    assert!(
        promote_fixture(&pool, first, "executor", None, Wait)
            .await
            .unwrap()
    );
    let old = claim(&pool, "executor").await;
    assert!(
        outbox::mark_accepted(&pool, old.id, "worker-token")
            .await
            .unwrap()
    );
    if released {
        sqlx::query(
            "UPDATE intake_session_owners SET status='released' WHERE raw_channel_id='733699'",
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    let second = stage(&pool, &payload("733699", "602", "gateway"), Some(0), 0).await;
    assert!(
        promote_fixture(
            &pool,
            second,
            "executor",
            Some(("executor", 0)),
            Unavailable
        )
        .await
        .unwrap()
    );
    let row = claim(&pool, "executor").await;
    assert_eq!(
        (row.id, row.owner_generation),
        (second, Some(1)),
        "new incarnation renews authority even on the same stable node"
    );
    assert!(
        outbox::mark_accepted(&pool, row.id, "worker-token")
            .await
            .unwrap()
    );
    let permit = prepare(&pool, row.id, "executor", "new incarnation").await;
    let calls = AtomicUsize::new(0);
    permit
        .submit(
            "new incarnation",
            &json!({"native":"exact-session","driver":"tui"}),
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let old_state: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id=$1")
        .bind(first)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(old_state, "unknown");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn custody_pg_same_instance_restart_renews_generation_before_new_source_pg() {
    lifecycle_case(false).await;
}
#[tokio::test]
async fn custody_pg_released_owner_reacquires_without_old_open_delivery_deadlock_pg() {
    lifecycle_case(true).await;
}

#[tokio::test]
async fn custody_pg_refresh_preserves_sources_and_deadline_after_owner_change_pg() {
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let first = stage(&pool, &payload("733698", "701", "gateway"), None, 0).await;
    let mut tx = pool.begin().await.unwrap();
    assert!(
        !custody::refresh_stage_on_tx(&mut tx, first, "first", 99, None, 0)
            .await
            .unwrap(),
        "fresh home revision is required"
    );
    tx.rollback().await.unwrap();
    assert!(
        promote_fixture(&pool, first, "first", None, Wait)
            .await
            .unwrap()
    );
    let old = claim(&pool, "first").await;
    assert!(
        outbox::mark_accepted(&pool, old.id, "worker-token")
            .await
            .unwrap()
    );
    prepare(&pool, old.id, "first", "m1")
        .await
        .submit(
            "m1",
            &json!({"native":"exact-session","driver":"tui"}),
            |_| async {},
        )
        .await
        .unwrap();
    assert!(
        outbox::mark_done(&pool, old.id, "worker-token")
            .await
            .unwrap()
    );
    let second = stage(&pool, &payload("733698", "702", "gateway"), Some(0), 0).await;
    use crate::services::cluster::attachment_transfer::{self as attachments, store};
    let identity = attachments::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "733698".into(),
        user_msg_id: "703".into(),
    };
    let bytes = b"rebase preserves this actual attachment";
    let bundle = attachments::validate_attachment_bundle_v1(
        attachments::AttachmentBundleV1 {
            version: attachments::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![attachments::AttachmentEntryV1 {
                filename: "rebase.txt".into(),
                bytes: bytes.to_vec(),
                sha256: attachments::attachment_sha256_hex(bytes),
            }],
        },
        &identity,
    )
    .unwrap();
    let reference = store::put(&pool, &bundle).await.unwrap();
    let mut third_payload = payload("733698", "703", "gateway");
    third_payload.attachment_refs = json!([reference]);
    third_payload.reply_context =
        Some(json!({"message_id":"699","content":"original reply"}).to_string());
    third_payload.replay_source_message_ids = Some(vec!["703".into(), "704".into()]);
    let third = stage(&pool, &third_payload, Some(0), 0).await;
    let original_sql = "SELECT jsonb_build_array(routing_receipt_id,provider,channel_id,user_msg_id,user_text,replay_source_message_ids,attachment_refs,reply_context) FROM intake_outbox WHERE id=$1";
    let original: serde_json::Value = sqlx::query_scalar(original_sql)
        .bind(third)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        promote_fixture(&pool, second, "second", Some(("first", 0)), Unavailable)
            .await
            .unwrap()
    );
    let active = claim(&pool, "second").await;
    assert!(
        outbox::mark_accepted(&pool, active.id, "worker-token")
            .await
            .unwrap()
    );
    prepare(&pool, active.id, "second", "m2")
        .await
        .submit(
            "m2",
            &json!({"native":"exact-session","driver":"tui"}),
            |_| async {},
        )
        .await
        .unwrap();
    assert!(
        outbox::mark_done(&pool, active.id, "worker-token")
            .await
            .unwrap()
    );
    let rebased: Option<i64> =
        sqlx::query_scalar("SELECT routing_wait_owner_generation FROM intake_outbox WHERE id=$1")
            .bind(third)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        rebased,
        Some(1),
        "remaining held inputs follow the new owner"
    );
    assert!(
        !promote_fixture(&pool, third, "second", Some(("first", 0)), Wait)
            .await
            .unwrap(),
        "a stale owner cannot admit the rebased input"
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        custody::refresh_stage_on_tx(&mut tx, third, "second", 0, Some(("second", 1)), 0)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let deadline: String =
        sqlx::query_scalar("SELECT routing_not_before::TEXT FROM intake_outbox WHERE id=$1")
            .bind(third)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        custody::refresh_stage_on_tx(&mut tx, third, "second", 0, Some(("second", 1)), 300)
            .await
            .unwrap()
    );
    assert!(
        !custody::refresh_stage_on_tx(&mut tx, first, "second", 0, Some(("second", 1)), 0)
            .await
            .unwrap(),
        "admitted sources cannot be retagged"
    );
    tx.commit().await.unwrap();
    let unchanged: String =
        sqlx::query_scalar("SELECT routing_not_before::TEXT FROM intake_outbox WHERE id=$1")
            .bind(third)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deadline, unchanged);
    assert!(
        promote_fixture(&pool, third, "second", Some(("second", 1)), Wait)
            .await
            .unwrap()
    );
    let row = claim(&pool, "second").await;
    assert_eq!(row.id, third);
    assert!(
        outbox::mark_accepted(&pool, row.id, "worker-token")
            .await
            .unwrap()
    );
    let calls = AtomicUsize::new(0);
    prepare(&pool, row.id, "second", "m3")
        .await
        .submit(
            "m3",
            &json!({"native":"exact-session","driver":"tui"}),
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let preserved: serde_json::Value = sqlx::query_scalar(original_sql)
        .bind(third)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        original, preserved,
        "routing changes preserve the raw input"
    );
    assert_eq!(
        store::load(&pool, &reference)
            .await
            .unwrap()
            .as_bundle()
            .entries[0]
            .bytes,
        bytes
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM intake_outbox WHERE routing_receipt_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 3, "refresh reuses the three original admissions");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn custody_pg_existing_withheld_result_survives_owner_loss_pg() {
    use crate::services::cluster::intake_router_hook::owner_record::{
        OwnerIdentity, acquire_owner_in_tx,
    };
    let runtime = RuntimeRoot::new();
    assert!(runtime.root.path().exists());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let id = OwnerIdentity::new("claude", "733697");
    let mut tx = pool.begin().await.unwrap();
    custody::lock(&mut tx, &id).await.unwrap();
    acquire_owner_in_tx(&mut tx, &id, "old", |_| true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let preserved = json!({"reason":"keep-existing-reason","piece":"unsent answer"});
    let old:i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_disposition,replay_source_message_ids,replay_hold_reason,replay_preserved) VALUES('old','gateway','733697','801','100','old input','foreground','custody-agent','claude','spawned','withheld',ARRAY['801'],'partial_failure',$1) RETURNING id").bind(&preserved).fetch_one(&pool).await.unwrap();
    let fresh = stage(&pool, &payload("733697", "802", "gateway"), Some(0), 0).await;
    assert!(
        promote_fixture(&pool, fresh, "home", Some(("old", 0)), Unavailable)
            .await
            .unwrap()
    );
    let saved: (String, serde_json::Value, String) = sqlx::query_as(
        "SELECT status,replay_preserved,replay_hold_reason FROM intake_outbox WHERE id=$1",
    )
    .bind(old)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        saved,
        ("unknown".into(), preserved, "partial_failure".into())
    );
    let row = claim(&pool, "home").await;
    assert_eq!(row.id, fresh);
    assert!(
        outbox::mark_accepted(&pool, row.id, "worker-token")
            .await
            .unwrap()
    );
    let calls = AtomicUsize::new(0);
    prepare(&pool, row.id, "home", "fresh source")
        .await
        .submit(
            "fresh source",
            &json!({"native":"exact-session","driver":"tui"}),
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn owner_cleanup_unclaimed_legacy_pending_keeps_fifo_and_executes_once_pg() {
    let _runtime = RuntimeRoot::new();
    use crate::services::cluster::intake_router_hook::owner_record::{
        OwnerIdentity, acquire_owner_in_tx,
    };
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    for (index, (old, target)) in [("mac-a", "mac-b"), ("mac-b", "mac-a")]
        .into_iter()
        .enumerate()
    {
        let channel = (73369410 + index).to_string();
        let identity = OwnerIdentity::new("claude", &channel);
        let mut tx = pool.begin().await.unwrap();
        acquire_owner_in_tx(&mut tx, &identity, old, |_| true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let original = payload(&channel, "701", old);
        let old_id = outbox::insert_pending(&pool, &original, 1, None)
            .await
            .unwrap();
        let newer = payload(&channel, "702", "gateway");
        let later = stage(&pool, &newer, Some(0), 0).await;
        assert!(
            !promote_fixture(&pool, later, target, Some((old, 0)), Unavailable)
                .await
                .unwrap(),
            "older unstarted input must remain first"
        );
        let preserved: (String, String, serde_json::Value) = sqlx::query_as(
            "SELECT status,user_text,attachment_refs FROM intake_outbox WHERE id=$1",
        )
        .bind(old_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            preserved,
            (
                "unknown".into(),
                original.user_text.clone(),
                original.attachment_refs.clone()
            )
        );
        assert!(
            outbox::claim_pending_for_target_except(&pool, old, "claude", "old-control", &[])
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            promote_fixture(&pool, old_id, target, Some((target, 1)), Wait)
                .await
                .unwrap()
        );
        let row = claim(&pool, target).await;
        assert_eq!(row.id, old_id);
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let http = Arc::new(serenity::Http::new("fixture"));
        assert!(matches!(
            execute_intake_turn_core(&http, &shared, "fixture", request(&row), vec![])
                .await
                .unwrap(),
            IntakeExecution::Deferred
        ));
        assert!(
            outbox::mark_accepted(&pool, old_id, "worker-token")
                .await
                .unwrap()
        );
        let counter = AtomicUsize::new(0);
        let permit = prepare(&pool, old_id, target, &row.user_text).await;
        permit
            .submit(
                &row.user_text,
                &json!({"native":"exact-session","driver":"tui"}),
                |_| async {
                    counter.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
            .unwrap();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "actual first effect must run once after handoff"
        );
        assert!(
            outbox::claim_pending_for_target_except(&pool, old, "claude", "old-control", &[])
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !promote_fixture(&pool, later, target, Some((target, 1)), Wait)
                .await
                .unwrap(),
            "newer source must wait while older delivery is open"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn owner_cleanup_bundle_live_pin_and_withheld_seven_day_retention_pg() {
    use crate::services::cluster::attachment_transfer::{self as attachments, store};
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed(&pool).await;
    let identity = attachments::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: "73369420".into(),
        user_msg_id: "711".into(),
    };
    let bytes = b"preserved attachment";
    let bundle = attachments::validate_attachment_bundle_v1(
        attachments::AttachmentBundleV1 {
            version: attachments::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![attachments::AttachmentEntryV1 {
                filename: "input.txt".into(),
                bytes: bytes.to_vec(),
                sha256: attachments::attachment_sha256_hex(bytes),
            }],
        },
        &identity,
    )
    .unwrap();
    let reference = store::put(&pool, &bundle).await.unwrap();
    let mut input = payload(&identity.channel_id, &identity.user_msg_id, "gateway");
    input.attachment_refs = json!([reference]);
    let first = stage(&pool, &input, None, 0).await;
    sqlx::query(
        "UPDATE intake_attachment_bundles SET expires_at=NOW()-INTERVAL '1 day' WHERE id=$1",
    )
    .bind(reference.bundle_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(store::cleanup(&pool).await.unwrap(), 0);
    assert_eq!(
        store::load(&pool, &reference)
            .await
            .unwrap()
            .as_bundle()
            .entries[0]
            .bytes,
        bytes
    );
    store::put(&pool, &bundle).await.unwrap();
    assert!(
        promote_fixture(&pool, first, "mac-a", None, Wait)
            .await
            .unwrap()
    );
    let row = claim(&pool, "mac-a").await;
    assert!(
        outbox::mark_accepted(&pool, row.id, "worker-token")
            .await
            .unwrap()
    );
    let second = stage(
        &pool,
        &payload(&identity.channel_id, "712", "gateway"),
        Some(0),
        0,
    )
    .await;
    assert!(
        promote_fixture(&pool, second, "mac-b", Some(("mac-a", 0)), Unavailable)
            .await
            .unwrap()
    );
    if crate::services::cluster::intake_router_hook::owner_record::cleanup::tests::mutant("pin") {
        sqlx::query("CREATE OR REPLACE FUNCTION intake_routing_bundle_pinned(bundle UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS $$ SELECT EXISTS(SELECT 1 FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.routing_state IS NOT NULL AND r.replay_disposition<>'classified_normal' AND d.attachment_refs @> jsonb_build_array(jsonb_build_object('bundle_id',bundle::TEXT))) $$").execute(&pool).await.unwrap();
    }
    let retained: bool = sqlx::query_scalar(
        "SELECT expires_at>=NOW()+INTERVAL '6 days' FROM intake_attachment_bundles WHERE id=$1",
    )
    .bind(reference.bundle_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(retained);
    assert_eq!(store::cleanup(&pool).await.unwrap(), 0);
    assert_eq!(
        store::load(&pool, &reference)
            .await
            .unwrap()
            .as_bundle()
            .entries[0]
            .bytes,
        bytes
    );
    sqlx::query(
        "UPDATE intake_attachment_bundles SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1",
    )
    .bind(reference.bundle_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        store::cleanup(&pool).await.unwrap(),
        1,
        "withheld custody must not pin bytes permanently"
    );
    assert!(store::load(&pool, &reference).await.is_err());
    pool.close().await;
}
