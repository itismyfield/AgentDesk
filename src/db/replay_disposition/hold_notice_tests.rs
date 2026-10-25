use super::{NoticeRead, NoticeReadError, NoticeRouting, read};

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{
    self, BeginFrom, CanonicalInput, EffectProjection, ExactAttempt,
};
use sqlx::{AssertSqlSafe, PgPool};

const ROUTING: NoticeRouting<'static> = NoticeRouting {
    bot: "notify",
    session_key: Some("claude/token/node:AgentDesk-claude-shared"),
};

async fn fixture() -> (TestPostgresDb, PgPool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    (db, pool)
}

async fn start(pool: &PgPool, source: &str) -> ExactAttempt {
    let input = CanonicalInput {
        request_key: format!("notice:{source}"),
        provider: "claude".into(),
        channel: "123456789".into(),
        sources: vec![source.into()],
        original_text: format!("original {source}"),
        owner_id: "owner".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([]),
        reply_context: Some("original reply".into()),
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &input, "node-a")
        .await
        .unwrap();
    let binding = serde_json::json!({"original": "binding"});
    write::begin(
        pool,
        receipt.id,
        BeginFrom::Registered {
            nonce: receipt.episode_nonce.as_deref().unwrap(),
        },
        &EffectProjection {
            effect_target: "provider_start",
            input_hash: "prepared",
            binding: &binding,
        },
        "incarnation-a",
    )
    .await
    .unwrap()
    .into_parts()
    .0
}

async fn snapshot(pool: &PgPool, id: i64) -> (String, serde_json::Value, i64) {
    sqlx::query_as("SELECT replay_disposition, replay_preserved, (SELECT COUNT(*) FROM message_outbox) FROM intake_outbox WHERE id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

async fn reject(pool: &PgPool, table: &str) {
    let sql = match table {
        "message_outbox" => "CREATE FUNCTION reject_notice() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected enqueue failure'; END $$;
            CREATE TRIGGER reject_notice BEFORE INSERT ON message_outbox FOR EACH ROW EXECUTE FUNCTION reject_notice()",
        "intake_outbox" => "CREATE FUNCTION reject_notice() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
            IF NEW.replay_preserved ? 'stale_replay_notice' THEN RAISE EXCEPTION 'injected reference failure'; END IF;
            RETURN NEW; END $$;
            CREATE TRIGGER reject_notice BEFORE UPDATE ON intake_outbox FOR EACH ROW EXECUTE FUNCTION reject_notice()",
        _ => panic!("unsupported fault table"),
    };
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn same_receipt_repeat_and_reopen_keep_one_permanent_notice_pg() {
    const RESTART: &str = "AGENTDESK_TEST_REPLAY_NOTICE_RESTART";
    if let Ok(raw) = std::env::var(RESTART) {
        let (url, id, expected): (String, i64, super::HoldNotice) =
            serde_json::from_str(&raw).unwrap();
        let pool = PgPool::connect(&url).await.unwrap();
        assert_eq!(
            read(&pool, id).await.unwrap(),
            NoticeRead::Notified(expected)
        );
        assert_eq!(snapshot(&pool, id).await.2, 1);
        pool.close().await;
        println!("REPLAY_NOTICE_RESTART_READ_OK");
        return;
    }
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "101").await;
    let preserved = serde_json::json!({"partial_body":"captured", "cursor":8});
    let first = write::withhold_and_notify(&pool, &attempt, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    let again = write::withhold_and_notify(&pool, &attempt, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    assert_eq!(first, again);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            concat!(
                module_path!(),
                "::same_receipt_repeat_and_reopen_keep_one_permanent_notice_pg"
            )
            .split_once("::")
            .unwrap()
            .1,
            "--nocapture",
        ])
        .env(
            RESTART,
            serde_json::to_string(&(&db.database_url, attempt.receipt_id(), &first)).unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "restart reader: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("REPLAY_NOTICE_RESTART_READ_OK"));
    println!(
        "REPLAY_NOTICE_RESTART_READ_OK receipt={}",
        attempt.receipt_id()
    );
    let reopened = sqlx::PgPool::connect(&db.database_url).await.unwrap();
    assert_eq!(
        read(&reopened, attempt.receipt_id()).await.unwrap(),
        NoticeRead::Notified(first.clone())
    );
    let after = write::withhold_and_notify(&reopened, &attempt, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    assert_eq!(first, after);
    let (state, stored, count) = snapshot(&pool, attempt.receipt_id()).await;
    assert_eq!((state.as_str(), count), ("withheld", 1));
    assert_eq!(stored["partial_body"], "captured");
    assert_eq!(stored["projection"]["binding"]["original"], "binding");
    let row: (
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT dedupe_key, session_key, dedupe_expires_at FROM message_outbox WHERE id = $1",
    )
    .bind(first.outbox_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            format!("stale_replay_withheld:{}", attempt.receipt_id()),
            ROUTING.session_key.map(str::to_string),
            None
        )
    );
    reopened.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn distinct_receipts_in_one_session_have_two_notice_ids_pg() {
    let (db, pool) = fixture().await;
    let a = start(&pool, "201").await;
    let b = start(&pool, "202").await;
    let preserved = serde_json::json!({"partial_body":"same"});
    let a = write::withhold_and_notify(&pool, &a, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    let b = write::withhold_and_notify(&pool, &b, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    assert_ne!(
        a.outbox_id, b.outbox_id,
        "a shared routing session is not a shared event"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn enqueue_or_reference_failure_rolls_back_hold_and_notice_pg() {
    for table in ["message_outbox", "intake_outbox"] {
        let (db, pool) = fixture().await;
        let attempt = start(&pool, "301").await;
        let before = snapshot(&pool, attempt.receipt_id()).await;
        reject(&pool, table).await;
        let result = write::withhold_and_notify(
            &pool,
            &attempt,
            "activity",
            &serde_json::json!({"partial_body":"captured"}),
            ROUTING,
        )
        .await;
        assert!(result.is_err(), "{table}: fault must be reached");
        assert_eq!(
            snapshot(&pool, attempt.receipt_id()).await,
            before,
            "{table}: both writes must roll back"
        );
        sqlx::raw_sql(AssertSqlSafe(match table {
            "message_outbox" => "DROP TRIGGER reject_notice ON message_outbox",
            _ => "DROP TRIGGER reject_notice ON intake_outbox",
        }))
        .execute(&pool)
        .await
        .unwrap();
        let result = write::withhold_and_notify(
            &pool,
            &attempt,
            "activity",
            &serde_json::json!({"partial_body":"captured"}),
            ROUTING,
        )
        .await
        .unwrap();
        assert!(
            result.outbox_id > 0,
            "retry after rollback must publish one event"
        );
        assert_eq!(snapshot(&pool, attempt.receipt_id()).await.2, 1);
        pool.close().await;
        db.drop().await;
    }
}

#[tokio::test]
async fn concurrent_hold_writers_share_one_event_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "401").await;
    let preserved = serde_json::json!({"partial_body":"one"});
    let (a, b) = tokio::join!(
        write::withhold_and_notify(&pool, &attempt, "activity", &preserved, ROUTING),
        write::withhold_and_notify(&pool, &attempt, "activity", &preserved, ROUTING),
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await.2, 1);
    let sources = crate::db::replay_disposition::unblocked_sources(
        &pool,
        "claude",
        "123456789",
        &["401".into()],
    )
    .await
    .unwrap();
    assert!(
        sources.is_empty(),
        "notification does not weaken the replay fence"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn legacy_hold_without_notice_is_readable_and_backfilled_once_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "501").await;
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::NotNotified
    );
    let preserved =
        serde_json::json!({"partial_body":"legacy captured", "legacy":{"unknown":"kept"}});
    write::withhold(&pool, &attempt, "legacy reason", &preserved)
        .await
        .unwrap();
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::NotNotified
    );
    let notice = write::withhold_and_notify(&pool, &attempt, "legacy reason", &preserved, ROUTING)
        .await
        .unwrap();
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::Notified(notice)
    );
    let (_, stored, count) = snapshot(&pool, attempt.receipt_id()).await;
    assert_eq!(count, 1);
    assert_eq!(stored["partial_body"], "legacy captured");
    assert_eq!(stored["legacy"]["unknown"], "kept");
    let reason: String =
        sqlx::query_scalar("SELECT replay_hold_reason FROM intake_outbox WHERE id=$1")
            .bind(attempt.receipt_id())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason, "legacy reason");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn terminal_outbox_gc_never_remints_the_receipt_notice_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "601").await;
    let preserved = serde_json::json!({"partial_body":"captured"});
    let first = write::withhold_and_notify(&pool, &attempt, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE message_outbox SET status='sent',created_at=NOW()-INTERVAL '31 days' WHERE id=$1",
    )
    .bind(first.outbox_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        crate::services::message_outbox::gc_stale_outbox_rows(&pool)
            .await
            .unwrap(),
        (0, 0, 0)
    );
    sqlx::query("UPDATE message_outbox SET status='failed' WHERE id=$1")
        .bind(first.outbox_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        crate::services::message_outbox::gc_stale_outbox_rows(&pool)
            .await
            .unwrap(),
        (0, 1, 0)
    );
    let after = write::withhold_and_notify(&pool, &attempt, "activity", &preserved, ROUTING)
        .await
        .unwrap();
    assert_eq!(
        first, after,
        "a missing delivery row does not authorize a new event"
    );
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await.2, 0);
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::Notified(first)
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn commit_failure_leaves_the_started_request_and_no_event_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "701").await;
    let before = snapshot(&pool, attempt.receipt_id()).await;
    sqlx::raw_sql(AssertSqlSafe("CREATE FUNCTION reject_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        RAISE EXCEPTION 'injected commit failure'; END $$;
        CREATE CONSTRAINT TRIGGER reject_commit AFTER UPDATE ON intake_outbox DEFERRABLE INITIALLY DEFERRED
        FOR EACH ROW WHEN (NEW.replay_preserved ? 'stale_replay_notice') EXECUTE FUNCTION reject_commit()"))
        .execute(&pool).await.unwrap();
    let result =
        write::withhold_and_notify(&pool, &attempt, "activity", &serde_json::json!({}), ROUTING)
            .await;
    assert!(
        matches!(result, Err(write::WriteError::AckUnknown(_))),
        "{result:?}"
    );
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await, before);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn dormant_withhold_still_produces_no_notification_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "801").await;
    write::withhold(
        &pool,
        &attempt,
        "activity",
        &serde_json::json!({"partial_body":"captured"}),
    )
    .await
    .unwrap();
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await.2, 0);
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::NotNotified
    );
    pool.close().await;
    db.drop().await;
}

#[test]
fn malformed_notice_never_reads_as_an_unnotified_legacy_row() {
    for value in [
        serde_json::json!([]),
        serde_json::json!({"stale_replay_notice":null}),
        serde_json::json!({"stale_replay_notice":{"dedupe_key":"stale_replay_withheld:2","outbox_id":1}}),
        serde_json::json!({"stale_replay_notice":{"dedupe_key":"stale_replay_withheld:1","outbox_id":0}}),
    ] {
        assert!(super::from_preserved(1, Some(&value)).is_err(), "{value}");
    }
}

#[test]
fn withheld_notice_is_non_turn_provenance_only_for_its_exact_source_and_reason() {
    use crate::services::message_outbox::is_non_turn_operational_alert;
    assert!(is_non_turn_operational_alert(
        "lifecycle_notifier",
        Some("stale_replay_withheld")
    ));
    assert!(!is_non_turn_operational_alert(
        "lifecycle_notifier",
        Some("runtime_started")
    ));
    assert!(!is_non_turn_operational_alert(
        "headless_turn",
        Some("stale_replay_withheld")
    ));
}

#[tokio::test]
async fn p2_notice_routing_normalizes_missing_empty_and_padded_session_pg() {
    let (db, pool) = fixture().await;
    for (n, raw, expected) in [
        (0, None, "channel:123456789"),
        (1, Some("   "), "channel:123456789"),
        (2, Some(" route "), "route"),
    ] {
        let attempt = start(&pool, &format!("p2-route-{n}")).await;
        let notice = write::withhold_and_notify(
            &pool,
            &attempt,
            "activity",
            &serde_json::json!({}),
            NoticeRouting {
                bot: "notify",
                session_key: raw,
            },
        )
        .await
        .unwrap();
        let stored: Option<String> =
            sqlx::query_scalar("SELECT session_key FROM message_outbox WHERE id=$1")
                .bind(notice.outbox_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.as_deref(), Some(expected));
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p2_existing_hold_rejects_changed_reason_or_preserved_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "p2-backfill").await;
    let preserved = serde_json::json!({"partial_body":"kept"});
    write::withhold(&pool, &attempt, "original", &preserved)
        .await
        .unwrap();
    let before = snapshot(&pool, attempt.receipt_id()).await;
    for (reason, body) in [
        ("changed", preserved.clone()),
        ("original", serde_json::json!({"partial_body":"new"})),
    ] {
        let result = write::withhold_and_notify(&pool, &attempt, reason, &body, ROUTING).await;
        assert!(
            matches!(result, Err(write::WriteError::CasMiss)),
            "must not silently discard caller data: {result:?}"
        );
        assert_eq!(snapshot(&pool, attempt.receipt_id()).await, before);
    }
    write::withhold_and_notify(&pool, &attempt, "original", &preserved, ROUTING)
        .await
        .unwrap();
    let after = snapshot(&pool, attempt.receipt_id()).await;
    assert_eq!(after.2, 1);
    assert!(matches!(
        write::withhold_and_notify(&pool, &attempt, "changed", &preserved, ROUTING).await,
        Err(write::WriteError::CasMiss)
    ));
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await, after);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p2_missing_receipt_is_not_an_unnotified_row_pg() {
    let (db, pool) = fixture().await;
    assert!(matches!(
        super::read(&pool, i64::MAX).await,
        Ok(NoticeRead::Missing)
    ));
    let attempt = start(&pool, "p2-present").await;
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::NotNotified
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p2_notice_payload_pins_the_first_hold_nonce_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "p2-nonce").await;
    let first =
        write::withhold_and_notify(&pool, &attempt, "activity", &serde_json::json!({}), ROUTING)
            .await
            .unwrap();
    let (_, stored, _) = snapshot(&pool, attempt.receipt_id()).await;
    assert_eq!(stored[super::FIELD]["hold_nonce"], attempt.nonce());
    assert_eq!(
        serde_json::to_value(&first).unwrap()["hold_nonce"],
        attempt.nonce()
    );
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::Notified(first)
    );
    for (label, pinned) in [("old", None), ("wrong", Some("other-nonce"))] {
        let older = start(&pool, &format!("p2-payload-{label}")).await;
        let mut notice =
            serde_json::json!({"dedupe_key": super::key(older.receipt_id()), "outbox_id": 900});
        if let Some(nonce) = pinned {
            notice["hold_nonce"] = nonce.into();
        }
        write::withhold(
            &pool,
            &older,
            "activity",
            &serde_json::json!({"stale_replay_notice":notice}),
        )
        .await
        .unwrap();
        let readback = read(&pool, older.receipt_id()).await;
        if pinned.is_some() {
            assert!(matches!(readback, Err(NoticeReadError::Malformed)));
        } else {
            let NoticeRead::Notified(notice) = readback.unwrap() else {
                panic!("old payload must remain notified");
            };
            assert_eq!(notice.hold_nonce, None);
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p3_missing_notice_receipt_is_distinct_from_malformed_pg() {
    let (db, pool) = fixture().await;
    assert_eq!(
        super::read(&pool, i64::MAX).await.unwrap(),
        NoticeRead::Missing,
        "missing receipt is a typed read result"
    );
    let attempt = start(&pool, "p3-malformed").await;
    sqlx::query("UPDATE intake_outbox SET replay_preserved=replay_preserved || '{\"stale_replay_notice\":null}'::jsonb WHERE id=$1")
        .bind(attempt.receipt_id())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        matches!(
            super::read(&pool, attempt.receipt_id()).await,
            Err(NoticeReadError::Malformed)
        ),
        "malformed is never no notice"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn recovery_hold_notice_serializes_with_the_live_hold_writer_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "recovery-race").await;
    let body = serde_json::json!({"partial_body":"captured"});
    let (recovered, live) = tokio::join!(
        write::recover_hold_notice(
            &pool,
            attempt.receipt_id(),
            "claude",
            "123456789",
            Some(&body),
            ROUTING
        ),
        write::withhold_and_notify(&pool, &attempt, "activity", &body, ROUTING),
    );
    let notice = recovered.unwrap();
    match live {
        Ok(live) => assert_eq!(notice, live),
        Err(write::WriteError::CasMiss) => {}
        Err(error) => panic!("unexpected live writer failure: {error}"),
    }
    assert_eq!(
        read(&pool, attempt.receipt_id()).await.unwrap(),
        NoticeRead::Notified(notice.clone())
    );
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await.2, 1);
    assert!(matches!(
        write::recover_hold_notice(
            &pool,
            attempt.receipt_id(),
            "codex",
            "123456789",
            None,
            ROUTING
        )
        .await,
        Err(write::WriteError::CasMiss)
    ));
    assert!(matches!(
        write::recover_hold_notice(&pool, attempt.receipt_id(), "claude", "999", None, ROUTING)
            .await,
        Err(write::WriteError::CasMiss)
    ));
    let tuple: (String, String) = sqlx::query_as(
        "SELECT replay_episode_nonce,replay_owner_incarnation FROM intake_outbox WHERE id=$1",
    )
    .bind(attempt.receipt_id())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tuple, (attempt.nonce().into(), "incarnation-a".into()));
    let again = write::recover_hold_notice(
        &pool,
        attempt.receipt_id(),
        "claude",
        "123456789",
        None,
        ROUTING,
    )
    .await
    .unwrap();
    assert_eq!(again, notice);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn recovery_enqueue_failure_keeps_original_tuple_and_delivery_custody_pg() {
    let (db, pool) = fixture().await;
    let attempt = start(&pool, "recovery-rollback").await;
    let before = snapshot(&pool, attempt.receipt_id()).await;
    reject(&pool, "message_outbox").await;
    assert!(
        write::recover_hold_notice(
            &pool,
            attempt.receipt_id(),
            "claude",
            "123456789",
            None,
            ROUTING
        )
        .await
        .is_err()
    );
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await, before);
    sqlx::raw_sql(AssertSqlSafe(
        "DROP TRIGGER reject_notice ON message_outbox",
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        write::recover_hold_notice(
            &pool,
            attempt.receipt_id(),
            "claude",
            "123456789",
            None,
            ROUTING
        )
        .await
        .is_ok()
    );
    assert_eq!(snapshot(&pool, attempt.receipt_id()).await.2, 1);
    pool.close().await;
    db.drop().await;
}
