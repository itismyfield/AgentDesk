use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::owner_fence_pg_tests::snapshot;
use serde_json::json;
use std::time::Duration;

fn root() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    let dir = tempfile::tempdir().unwrap();
    let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        dir.path(),
    );
    (dir, guard)
}

async fn setup(pool: &PgPool, channel: u64, stamped: bool) -> (IntakeOutboxRow, WaitIntent) {
    setup_sources(
        pool,
        channel,
        stamped,
        vec![channel.to_string(), (channel + 1).to_string()],
    )
    .await
}

async fn setup_sources(
    pool: &PgPool,
    channel: u64,
    stamped: bool,
    sources: Vec<String>,
) -> (IntakeOutboxRow, WaitIntent) {
    sqlx::query("INSERT INTO agents(id,name,provider) VALUES('fence-agent','Fence','claude') ON CONFLICT DO NOTHING")
        .execute(pool).await.unwrap();
    let id:i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,
        channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,claim_owner,
        claimed_at,owner_instance_id,owner_generation,admission_kind,replay_source_message_ids)
        VALUES('fence-worker','leader',$1,$1,'100','hello','standard','fence-agent','claude','spawned',
        'token',NOW()-INTERVAL '1 hour',$2,$3,'forwarded',$4) RETURNING id")
        .bind(channel.to_string()).bind(stamped.then_some("fence-worker")).bind(stamped.then_some(0_i64))
        .bind(&sources).fetch_one(pool).await.unwrap();
    if stamped {
        sqlx::query("INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status)
            VALUES('claude',$1,'fence-worker',0,'active')")
            .bind(channel.to_string()).execute(pool).await.unwrap();
    }
    sqlx::query(
        "UPDATE intake_outbox SET accepted_at=NOW(),spawned_at=NOW(),
        attachment_refs=$2,replay_source_message_ids=$3,reply_context='original reply'
        WHERE id=$1",
    )
    .bind(id)
    .bind(json!([{"bundle":"original-upload","name":"source.txt"}]))
    .bind(sources)
    .execute(pool)
    .await
    .unwrap();
    let key = format!("claude/migration-token/fence-worker:AgentDesk-claude-migration-{channel}");
    let canonical:i64 = sqlx::query_scalar("INSERT INTO sessions (session_key,provider,status,identity_kind,
        discord_token_hash,channel_id,instance_id,cwd,claude_session_id,raw_provider_session_id)
        VALUES ($1,'claude','idle','discord_channel','migration-token',$2,'fence-worker','/tmp/old-cwd',$3,$3) RETURNING id")
        .bind(&key).bind(channel.to_string()).bind("11111111-1111-4111-8111-111111111111").fetch_one(pool).await.unwrap();
    let authority = Authority {
        provider: "claude".into(),
        token_hash: "migration-token".into(),
        channel_id: channel,
        local_instance: "fence-worker".into(),
        runtime_root: crate::config::runtime_root()
            .unwrap()
            .canonicalize()
            .unwrap(),
        logical_key: format!("AgentDesk-claude-migration-{channel}"),
        session_key: key,
    };
    let intent = capture_intent(pool, &authority, canonical, 7, "original-migration-intent")
        .await
        .unwrap();
    let row = sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (row, intent)
}

async fn pending_row(pool: &PgPool, id: i64) {
    sqlx::query(
        "UPDATE intake_outbox SET status='pending',claim_owner=NULL,claimed_at=NULL,
        accepted_at=NULL,spawned_at=NULL,dispatched_at=NULL WHERE id=$1",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

fn payload_image(image: &Value) -> Value {
    let mut payload = image.clone();
    let object = payload.as_object_mut().unwrap();
    for key in [
        "status",
        "claim_owner",
        "claimed_at",
        "accepted_at",
        "spawned_at",
        "last_error",
        "updated_at",
    ] {
        object.remove(key);
    }
    payload
}

#[tokio::test]
async fn migration_wait_rearms_exact_before_effects_without_settling_payload_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030101, true).await;
    let before = snapshot(&pool, row.id).await;
    let result = park_before_effects(&pool, BeforeEffects::for_test(&row), &intent)
        .await
        .unwrap();
    assert_eq!(
        result,
        ParkOutcome::Parked,
        "exact BeforeEffects must park the original row without execution settlement"
    );
    let after = snapshot(&pool, row.id).await;
    assert_eq!(after["status"], "pending");
    assert_eq!(
        payload_image(&after),
        payload_image(&before),
        "migration wait must preserve the original ID, attempt, payload, attachments and source set"
    );
    for key in ["claim_owner", "claimed_at", "accepted_at", "spawned_at"] {
        assert!(
            after[key].is_null(),
            "park must clear only its current execution claim: {key}"
        );
    }
    assert!(after["last_error"].as_str().unwrap().starts_with(PREFIX));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert_eq!(
        fence.waiting().requests().len(),
        1,
        "the original row must be independently reverified after the rearm commit"
    );
    assert!(fence.blockers().is_empty());
    let waiting = &fence.waiting().requests()[0];
    assert_eq!(waiting.row_id, row.id);
    assert_eq!(waiting.source_ids, vec!["75030101", "75030102"]);
    assert!(matches!(
        waiting.owner,
        WaitOwner::Stamped { generation: 0, .. }
    ));
    fence.release().await.unwrap();
    let repeated = park_before_effects(&pool, BeforeEffects::for_test(&row), &intent)
        .await
        .unwrap();
    assert_eq!(repeated, ParkOutcome::AlreadyParked);
    assert_eq!(
        snapshot(&pool, row.id).await,
        after,
        "an exact acknowledged row cannot acquire another attempt or marker"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_ack_unknown_revalidates_the_original_pending_with_one_connection_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030103, false).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        with_lost_commit_ack(park_before_effects(
            &pool,
            BeforeEffects::for_test(&row),
            &intent,
        )),
    )
    .await
    .expect("commit readback must release its old connection before reacquiring")
    .unwrap();
    assert_eq!(
        outcome,
        ParkOutcome::AlreadyParked,
        "a lost commit ACK is progress only after an exact pending marker readback"
    );
    let reopened = capture_intent(
        &pool,
        &intent.authority,
        intent.selector.canonical_row,
        7,
        "original-migration-intent",
    )
    .await
    .unwrap();
    let fence = fenced_waiting(&pool, &reopened).await.unwrap();
    assert_eq!(
        fence.waiting().requests().len(),
        1,
        "memory-free restart must reconstruct the same durable wait reference"
    );
    assert!(matches!(
        fence.waiting().requests()[0].owner,
        WaitOwner::LegacyLocal { .. }
    ));
    assert!(fence.blockers().is_empty());
    fence.release().await.unwrap();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_classifies_every_open_or_unknown_responsibility_and_preserves_rows_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, change) in [
        "claim_owner='foreign'",
        "accepted_at=NOW()",
        "spawned_at=NOW()",
        "dispatched_at=NOW()",
        "completed_at=NOW()",
        "replay_source_message_ids=ARRAY[user_msg_id,'0']",
        "home_epoch=99",
        "last_error='herdr-consent-custody:v1:{}'",
        "owner_generation=NULL",
        "status='accepted'",
        "status='unknown'",
        "status='failed_post_accept'",
        "target_instance_id='foreign'",
        "replay_disposition='started_unclassified'",
        "last_error='unknown-receipt'",
    ]
    .iter()
    .enumerate()
    {
        let channel = 75030200 + index as u64;
        let invalid_source = change.starts_with("replay_source_message_ids=");
        let (row, intent) = if invalid_source {
            setup_sources(&pool, channel, true, vec![channel.to_string(), "0".into()]).await
        } else {
            setup(&pool, channel, true).await
        };
        pending_row(&pool, row.id).await;
        if !invalid_source {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE intake_outbox SET {change} WHERE id=$1"
            )))
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let before = snapshot(&pool, row.id).await;
        let fence = fenced_waiting(&pool, &intent).await.unwrap();
        assert!(
            fence.waiting().requests().is_empty(),
            "unsafe responsibility cannot be exempted as waiting: {change}"
        );
        assert_eq!(
            fence.blockers().len(),
            1,
            "all observed open or unknown responsibility must be classified: {change}"
        );
        fence.release().await.unwrap();
        assert_eq!(
            snapshot(&pool, row.id).await,
            before,
            "classification cannot erase a blocker: {change}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_valid_legacy_local_is_not_a_wildcard_and_foreign_stamps_never_rearm_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, intent) = setup(&pool, 75030300, false).await;
    pending_row(&pool, row.id).await;
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert_eq!(
        fence.waiting().requests().len(),
        1,
        "an exact legacy local pending row must make progress without inventing generation zero"
    );
    assert!(matches!(
        fence.waiting().requests()[0].owner,
        WaitOwner::LegacyLocal { .. }
    ));
    fence.release().await.unwrap();
    sqlx::query("INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status)
        VALUES('claude','75030300','foreign',0,'active')").execute(&pool).await.unwrap();
    assert!(
        fenced_waiting(&pool, &intent).await.is_err(),
        "a previously unstamped row cannot bypass a conflicting stamped owner"
    );
    let (stamped, stamp_intent) = setup(&pool, 75030301, true).await;
    sqlx::query("UPDATE intake_outbox SET owner_generation=owner_generation+1 WHERE id=$1")
        .bind(stamped.id)
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool, stamped.id).await;
    let result = park_before_effects(&pool, BeforeEffects::for_test(&stamped), &stamp_intent)
        .await
        .unwrap();
    assert!(
        matches!(result, ParkOutcome::Blocked(_)),
        "an exact proof cannot authorize a changed owner generation"
    );
    assert_eq!(snapshot(&pool, stamped.id).await, before);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_selector_readset_and_marked_payload_are_rechecked_after_restart_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, change) in [
        "raw_provider_session_id='22222222-2222-4222-8222-222222222222'",
        "cwd='/tmp/replaced-project'",
        "claude_session_id_recorded_at=NOW()+INTERVAL '1 second'",
        "instance_id='foreign'",
    ]
    .iter()
    .enumerate()
    {
        let (row, intent) = setup(&pool, 75030400 + index as u64, true).await;
        let before = snapshot(&pool, row.id).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE sessions SET {change} WHERE id=$1"
        )))
        .bind(intent.selector.canonical_row)
        .execute(&pool)
        .await
        .unwrap();
        let result = park_before_effects(&pool, BeforeEffects::for_test(&row), &intent).await;
        assert!(
            matches!(result, Ok(ParkOutcome::Blocked(_)) | Err(_)),
            "a stale selector cannot park input: {change}"
        );
        assert_eq!(snapshot(&pool, row.id).await, before);
    }
    let (row, intent) = setup(&pool, 75030410, true).await;
    assert_eq!(
        park_before_effects(&pool, BeforeEffects::for_test(&row), &intent)
            .await
            .unwrap(),
        ParkOutcome::Parked
    );
    sqlx::query("UPDATE intake_outbox SET attachment_refs='[]'::JSONB WHERE id=$1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool, row.id).await;
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert!(
        fence.waiting().requests().is_empty(),
        "a persisted marker cannot approve changed attachment custody"
    );
    assert_eq!(fence.blockers().len(), 1);
    fence.release().await.unwrap();
    assert_eq!(snapshot(&pool, row.id).await, before);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_own_fence_holds_the_db_subset_without_pool_reentry_or_updates_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030500, false).await;
    pending_row(&pool, row.id).await;
    let before = snapshot(&pool, row.id).await;
    let fence = tokio::time::timeout(Duration::from_secs(3), fenced_waiting(&pool, &intent))
        .await
        .expect("the fenced read may use only its owner-gated transaction connection")
        .unwrap();
    assert_eq!(fence.waiting().requests().len(), 1);
    assert!(fence.blockers().is_empty());
    assert_eq!(
        pool.num_idle(),
        0,
        "the returned proof must retain its DB fence"
    );
    fence.release().await.unwrap();
    assert_eq!(
        snapshot(&pool, row.id).await,
        before,
        "a DB wait-set proof never mutates the original pending row"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_accepts_only_exact_registered_not_started_receipts_pg() {
    use crate::db::replay_disposition::write::{CanonicalInput, register_or_reuse};
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    for variant in 0..8 {
        let channel = 75030600 + variant;
        let (row, intent) = setup(&pool, channel, false).await;
        pending_row(&pool, row.id).await;
        let mut input = CanonicalInput::discord(
            "claude",
            channel,
            channel,
            [channel, channel + 1],
            &row.user_text,
        );
        input.owner_id = row.request_owner_id.clone();
        input.agent_id = row.agent_id.clone();
        input.attachments = row.attachment_refs.clone();
        input.reply_context = row.reply_context.clone();
        match variant {
            1 => input.attachments = json!([]),
            2 => input.owner_id = "foreign-user".into(),
            3 => input.original_text = "a different request".into(),
            4 => input.reply_context = None,
            5 => input.agent_id = "foreign-agent".into(),
            6 => input.request_key = "foreign-request-key".into(),
            7 => input.provenance = "unrecognized",
            _ => {}
        }
        let receipt = register_or_reuse(&pool, &input, "fence-worker")
            .await
            .unwrap();
        assert_eq!(
            receipt.disposition,
            crate::db::replay_disposition::receipt::Disposition::RegisteredNotStarted
        );
        let before = snapshot(&pool, row.id).await;
        let fence = fenced_waiting(&pool, &intent).await.unwrap();
        if variant == 0 {
            assert_eq!(
                fence.waiting().requests().len(),
                1,
                "an exact registered request must not deadlock behind its own waiting input"
            );
            assert!(fence.blockers().is_empty());
        } else {
            assert!(
                fence.waiting().requests().is_empty(),
                "registered spelling cannot replace full preserved request identity: variant={variant}"
            );
            assert_eq!(fence.blockers().len(), 1);
        }
        fence.release().await.unwrap();
        assert_eq!(snapshot(&pool, row.id).await, before);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_a_pending_attempt_cannot_hide_older_or_absorbed_submitted_sources_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, status) in ["failed_pre_accept", "unknown", "failed_post_accept", "done"]
        .iter()
        .enumerate()
    {
        let channel = 75030700 + index as u64;
        let (row, intent) = setup(&pool, channel, false).await;
        pending_row(&pool, row.id).await;
        sqlx::query("UPDATE intake_outbox SET attempt_no=2 WHERE id=$1")
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        let parent:i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,
            channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,attempt_no,
            accepted_at,spawned_at) SELECT target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
            request_owner_id,user_text,turn_kind,agent_id,provider,$2,1,
            CASE WHEN $2 IN ('done','failed_post_accept') THEN NOW() ELSE NULL END,
            CASE WHEN $2 IN ('done','failed_post_accept') THEN NOW() ELSE NULL END FROM intake_outbox WHERE id=$1 RETURNING id")
            .bind(row.id).bind(status).fetch_one(&pool).await.unwrap();
        sqlx::query("UPDATE intake_outbox SET parent_outbox_id=$2 WHERE id=$1")
            .bind(row.id)
            .bind(parent)
            .execute(&pool)
            .await
            .unwrap();
        let before = snapshot(&pool, row.id).await;
        let fence = fenced_waiting(&pool, &intent).await.unwrap();
        if *status == "failed_pre_accept" {
            assert_eq!(
                fence.waiting().requests().len(),
                1,
                "a proven failed preaccept ancestor must permit the same pending retry"
            );
            assert!(fence.blockers().is_empty());
        } else {
            assert!(
                fence.waiting().requests().is_empty(),
                "pending is not evidence that its previous attempt never reached the provider: {status}"
            );
            assert!(
                fence
                    .blockers
                    .iter()
                    .any(|b| b.row_id == row.id && b.reason == BlockReason::Family)
            );
        }
        fence.release().await.unwrap();
        assert_eq!(snapshot(&pool, row.id).await, before);
    }
    let channel = 75030710;
    let (row, intent) = setup(&pool, channel, false).await;
    pending_row(&pool, row.id).await;
    sqlx::query("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
        request_owner_id,user_text,turn_kind,agent_id,provider,status,accepted_at,spawned_at)
        SELECT target_instance_id,forwarded_by_instance_id,channel_id,$2,request_owner_id,'prior absorbed source',
        turn_kind,agent_id,provider,'done',NOW(),NOW() FROM intake_outbox WHERE id=$1")
        .bind(row.id).bind((channel+1).to_string()).execute(&pool).await.unwrap();
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert!(
        fence.waiting().requests().is_empty(),
        "the representative ID cannot hide an already-submitted absorbed source"
    );
    assert!(
        fence
            .blockers
            .iter()
            .any(|b| b.row_id == row.id && b.reason == BlockReason::Family)
    );
    fence.release().await.unwrap();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_lost_commit_ack_cannot_approve_a_reclaimed_row_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030800, true).await;
    let before = snapshot(&pool, row.id).await;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        with_claim_before_readback(park_before_effects(
            &pool,
            BeforeEffects::for_test(&row),
            &intent,
        )),
    )
    .await
    .expect(
        "the committed connection must be released before the competing real claim and readback",
    )
    .unwrap();
    let after = snapshot(&pool, row.id).await;
    assert_eq!(
        after["status"], "claimed",
        "the negative control must actually reach the competing claim"
    );
    assert_eq!(after["claim_owner"], "migration-readback-competitor");
    assert!(
        matches!(result, ParkOutcome::Unknown(_)),
        "a lost ACK cannot approve a row whose claim changed before exact readback"
    );
    assert_eq!(
        payload_image(&after),
        payload_image(&before),
        "unknown commit custody must leave the original payload and the new claimant intact"
    );
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert!(fence.waiting().requests().is_empty());
    assert_eq!(fence.blockers().len(), 1);
    fence.release().await.unwrap();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_rejects_changed_claim_payload_and_foreign_custody_before_rearm_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, change) in [
        "expected.user_text='stale payload'",
        "claim_owner='successor'",
        "last_error='herdr-consent-custody:v1:{}'",
        "completed_at=NOW()",
        "home_epoch=4",
        "owner_generation=owner_generation+1",
        "accepted_at=NULL",
        "spawned_at=NULL",
    ]
    .iter()
    .enumerate()
    {
        let (mut row, intent) = setup(&pool, 75030900 + index as u64, true).await;
        if change.starts_with("expected.user_text=") {
            // The real worker separately proves issuance; this checks DB defense against stale test evidence.
            row.user_text = "stale payload".into();
        } else {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE intake_outbox SET {change} WHERE id=$1"
            )))
            .bind(row.id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let before = snapshot(&pool, row.id).await;
        let result = park_before_effects(&pool, BeforeEffects::for_test(&row), &intent).await;
        assert!(
            matches!(result, Ok(ParkOutcome::Blocked(_)) | Err(_)),
            "a pre-effects proof cannot replace changed request custody: {change}"
        );
        assert_eq!(
            snapshot(&pool, row.id).await,
            before,
            "a rejected pre-effects proof must leave the current custodian intact: {change}"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_wait_owner_gate_and_share_fence_block_real_peer_dml_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75031000, false).await;
    pending_row(&pool, row.id).await;
    let peer = crate::db::postgres::connect_test_pool_with_max_connections(
        &db.database_url,
        "migration-wait peer",
        1,
    )
    .await
    .unwrap();
    let mut connection = peer.acquire().await.unwrap();
    let peer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let mut fence = fenced_waiting(&pool, &intent).await.unwrap();
    let owner_key = OwnerIdentity::new("claude", "75031000").advisory_key();
    let owner_held:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory'
        AND pid=pg_backend_pid() AND granted AND objsubid=1
        AND classid::BIGINT=(($1::BIGINT >> 32) & 4294967295) AND objid::BIGINT=($1::BIGINT & 4294967295))")
        .bind(owner_key).fetch_one(&mut *fence.transaction).await.unwrap();
    let id = row.id;
    let mut writer = tokio::spawn(async move {
        sqlx::query("UPDATE intake_outbox SET last_error='peer changed after fence' WHERE id=$1")
            .bind(id)
            .execute(&mut *connection)
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut observation_error = None;
    let blocked = loop {
        let blocked: bool = match sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1))>0")
            .bind(peer_pid)
            .fetch_one(&mut *fence.transaction)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                observation_error = Some(error);
                break false;
            }
        };
        if blocked || writer.is_finished() || tokio::time::Instant::now() >= deadline {
            break blocked;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let observed =
        sqlx::query_scalar::<_, Option<String>>("SELECT last_error FROM intake_outbox WHERE id=$1")
            .bind(row.id)
            .fetch_one(&mut *fence.transaction)
            .await;
    let released = fence.release().await;
    let completed = match tokio::time::timeout(Duration::from_secs(3), &mut writer).await {
        Ok(joined) => Some(joined),
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            None
        }
    };
    peer.close().await;
    pool.close().await;
    db.drop().await;
    released.expect("the SHARE fence must release before assertions");
    assert!(
        observation_error.is_none(),
        "PG lock observation failed: {observation_error:?}"
    );
    let observed = observed.expect("read the original guarded payload");
    let completed = completed
        .expect("release must unblock and join peer DML")
        .unwrap()
        .unwrap();
    assert!(
        owner_held,
        "the exact owner advisory key must remain held in the wait-set transaction"
    );
    assert!(
        blocked,
        "peer DML must reach the actual SHARE lock before publication can trust the DB subset"
    );
    assert_eq!(
        observed, None,
        "the guarded readset must remain unchanged until its SHARE fence releases"
    );
    assert_eq!(
        completed.rows_affected(),
        1,
        "the peer must progress only after the DB proof releases its guard"
    );
}

#[tokio::test]
async fn migration_wait_persisted_marker_cannot_approve_a_changed_fifo_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, intent) = setup(&pool, 75031100, false).await;
    assert_eq!(
        park_before_effects(&pool, BeforeEffects::for_test(&row), &intent)
            .await
            .unwrap(),
        ParkOutcome::Parked,
        "the regression must first publish an exact same-row migration wait marker"
    );
    let original = snapshot(&pool, row.id).await;
    sqlx::query(
        "UPDATE intake_outbox SET created_at=created_at+INTERVAL '1 millisecond' WHERE id=$1",
    )
    .bind(row.id)
    .execute(&pool)
    .await
    .unwrap();
    let changed = snapshot(&pool, row.id).await;
    assert_ne!(
        changed["created_at"], original["created_at"],
        "the negative control must actually change the original FIFO timestamp"
    );
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let waiting = fence.waiting().requests().len();
    let blockers = fence.blockers().len();
    fence.release().await.unwrap();
    let retained = snapshot(&pool, row.id).await;
    pool.close().await;
    db.drop().await;
    assert_eq!(
        waiting, 0,
        "a persisted migration marker cannot approve a changed original FIFO timestamp"
    );
    assert_eq!(blockers, 1);
    assert_eq!(
        retained, changed,
        "the changed FIFO remains with its original row for reconciliation"
    );
}
