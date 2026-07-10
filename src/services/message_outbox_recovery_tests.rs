use sqlx::PgPool;

use super::message_outbox_recovery::{RecoveryError, inspect_failed_rows, redrive_failed_rows};

async fn pool(name: &str) -> Option<PgPool> {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        name,
        "message_outbox recovery tests",
    )
    .await?;
    Some(pg_db.connect_and_migrate().await)
}

async fn seed(
    pool: &PgPool,
    id: i64,
    status: &str,
    source: &str,
    target: &str,
    content: &str,
    session_key: &str,
    dedupe_key: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO message_outbox
         (id,target,content,bot,source,status,reason_code,session_key,retry_count,error,
          claimed_at,claim_owner,next_attempt_at,sent_at,dedupe_key,dedupe_expires_at)
         VALUES($1,$2,$3,'notify',$4,$5,'catch_up_too_old',$6,5,'terminal failure',
                NOW()-INTERVAL '5 minutes','old-worker',NOW()+INTERVAL '15 minutes',
                CASE WHEN $5='sent' THEN NOW() ELSE NULL END,$7,NOW()-INTERVAL '1 minute')",
    )
    .bind(id)
    .bind(target)
    .bind(content)
    .bind(source)
    .bind(status)
    .bind(session_key)
    .bind(dedupe_key)
    .execute(pool)
    .await
    .expect("seed message_outbox recovery fixture");
}

async fn audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox_redrive_audit")
        .fetch_one(pool)
        .await
        .expect("count redrive audits")
}

async fn status(pool: &PgPool, id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("load outbox status")
}

#[tokio::test]
async fn redrive_dry_run_has_zero_mutation_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_dry_run").await else {
        return;
    };
    seed(
        &pool,
        10,
        "failed",
        "catch_up_too_old",
        "channel:10",
        &"x".repeat(400),
        "session:10",
        Some("dry-key"),
    )
    .await;
    let result = redrive_failed_rows(&pool, &[10], "dry-run-key", "verify only", true)
        .await
        .expect("dry-run redrive");
    assert_eq!(result[0].outcome, "would_redrive");
    assert_eq!(status(&pool, 10).await, "failed");
    assert_eq!(audit_count(&pool).await, 0);
    let inspection = inspect_failed_rows(&pool, &[10])
        .await
        .expect("inspect row");
    assert!(inspection[0].content_snippet.chars().count() <= 241);
    assert_eq!(inspection[0].content_hash.len(), 64);
    assert_eq!(
        inspection[0].error_snippet.as_deref(),
        Some("terminal failure")
    );
}

#[tokio::test]
async fn redrive_same_key_is_exact_once_even_after_refailure_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_exact_once").await else {
        return;
    };
    seed(
        &pool,
        20,
        "failed",
        "catch_up_too_old",
        "channel:20",
        "notice",
        "session:20",
        Some("once-key"),
    )
    .await;
    let first = redrive_failed_rows(&pool, &[20], "stable-key", "verified incident", false)
        .await
        .expect("first redrive");
    assert_eq!(first[0].outcome, "redriven");
    let row: (String, i64, Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT status,retry_count,error,claim_owner,dedupe_expires_at>NOW() FROM message_outbox WHERE id=20",
    )
    .fetch_one(&pool)
    .await
    .expect("load reset row");
    assert_eq!(row, ("pending".into(), 0, None, None, true));
    sqlx::query(
        "UPDATE message_outbox SET status='failed',retry_count=5,error='failed again' WHERE id=20",
    )
    .execute(&pool)
    .await
    .expect("simulate terminal refailure");
    let replay = redrive_failed_rows(&pool, &[20], "stable-key", "verified incident", false)
        .await
        .expect("same-key replay");
    assert_eq!(replay[0].outcome, "idempotent_replay");
    assert_eq!(status(&pool, 20).await, "failed");
    assert_eq!(audit_count(&pool).await, 1);
}

#[tokio::test]
async fn concurrent_same_key_admits_one_redrive_mutation_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_concurrent").await else {
        return;
    };
    seed(
        &pool,
        30,
        "failed",
        "catch_up_too_old",
        "channel:30",
        "notice",
        "session:30",
        Some("concurrent-key"),
    )
    .await;
    let (left, right) = tokio::join!(
        redrive_failed_rows(&pool, &[30], "same-key", "concurrent proof", false),
        redrive_failed_rows(&pool, &[30], "same-key", "concurrent proof", false),
    );
    let outcomes = [
        left.unwrap()[0].outcome.clone(),
        right.unwrap()[0].outcome.clone(),
    ];
    assert_eq!(
        outcomes.iter().filter(|name| *name == "redriven").count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|name| *name == "idempotent_replay")
            .count(),
        1
    );
    assert_eq!(audit_count(&pool).await, 1);
}

#[tokio::test]
async fn sent_or_active_semantic_sibling_blocks_redrive_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_sibling_block").await else {
        return;
    };
    seed(
        &pool,
        40,
        "failed",
        "catch_up_too_old",
        "channel:40",
        "sent notice",
        "session:40",
        Some("sent-key"),
    )
    .await;
    seed(
        &pool,
        41,
        "sent",
        "catch_up_too_old",
        "channel:40",
        "sent notice",
        "session:40",
        Some("sent-key"),
    )
    .await;
    seed(
        &pool,
        42,
        "failed",
        "catch_up_too_old",
        "channel:42",
        "active notice",
        "session:42",
        Some("active-key"),
    )
    .await;
    seed(
        &pool,
        43,
        "processing",
        "catch_up_too_old",
        "channel:42",
        "active notice",
        "session:42",
        Some("active-key"),
    )
    .await;
    let result = redrive_failed_rows(&pool, &[40, 42], "sibling-key", "sibling proof", false)
        .await
        .expect("sibling-blocked redrive");
    assert_eq!(result[0].outcome, "already_delivered");
    assert_eq!(result[1].outcome, "already_in_flight");
    assert_eq!(status(&pool, 40).await, "failed");
    assert_eq!(status(&pool, 42).await, "failed");
}

#[tokio::test]
async fn duplicate_failed_identity_selects_one_canonical_row_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_duplicate_failed").await else {
        return;
    };
    seed(
        &pool,
        50,
        "failed",
        "catch_up_too_old",
        "channel:50",
        "same notice",
        "session:50",
        Some("duplicate-key"),
    )
    .await;
    seed(
        &pool,
        51,
        "failed",
        "catch_up_too_old",
        "channel:50",
        "same notice",
        "session:50",
        Some("duplicate-key"),
    )
    .await;
    let result = redrive_failed_rows(
        &pool,
        &[51, 50],
        "duplicate-request",
        "duplicate proof",
        false,
    )
    .await
    .expect("duplicate failed redrive");
    assert_eq!(result[0].outcome, "duplicate_failed_identity");
    assert_eq!(result[0].canonical_id, Some(50));
    assert_eq!(result[1].outcome, "redriven");
    assert_eq!(status(&pool, 50).await, "pending");
    assert_eq!(status(&pool, 51).await, "failed");
}

#[tokio::test]
async fn unknown_source_preflight_blocks_all_mutation_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_unknown_source").await else {
        return;
    };
    seed(
        &pool,
        60,
        "failed",
        "unknown_historical_source",
        "channel:60",
        "notice",
        "session:60",
        None,
    )
    .await;
    let error = redrive_failed_rows(&pool, &[60], "unknown-key", "must fail closed", false)
        .await
        .expect_err("unknown source must block redrive");
    assert!(matches!(
        error,
        RecoveryError::SourceNotAllowed { id: 60, .. }
    ));
    assert_eq!(status(&pool, 60).await, "failed");
    assert_eq!(audit_count(&pool).await, 0);
}

async fn claim_worker(pool: PgPool, id: i64, owner: &'static str) -> Option<String> {
    for _ in 0..100 {
        let claimed = sqlx::query_scalar::<_, String>(
            "UPDATE message_outbox SET status='processing',claim_owner=$2,claimed_at=NOW() WHERE id=$1 AND status='pending' RETURNING claim_owner",
        )
        .bind(id).bind(owner).fetch_optional(&pool).await.expect("worker claim race");
        if claimed.is_some() {
            return claimed;
        }
        tokio::task::yield_now().await;
    }
    None
}

#[tokio::test]
async fn worker_claim_race_has_one_owner_and_no_double_claim_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_worker_race").await else {
        return;
    };
    seed(
        &pool,
        70,
        "failed",
        "catch_up_too_old",
        "channel:70",
        "notice",
        "session:70",
        Some("worker-key"),
    )
    .await;
    let (redrive, worker_a, worker_b) = tokio::join!(
        redrive_failed_rows(&pool, &[70], "worker-race-key", "worker race proof", false),
        claim_worker(pool.clone(), 70, "worker-a"),
        claim_worker(pool.clone(), 70, "worker-b"),
    );
    assert_eq!(redrive.unwrap()[0].outcome, "redriven");
    assert_eq!(
        [worker_a, worker_b]
            .iter()
            .filter(|owner| owner.is_some())
            .count(),
        1
    );
    let owner: Option<String> =
        sqlx::query_scalar("SELECT claim_owner FROM message_outbox WHERE id=70")
            .fetch_one(&pool)
            .await
            .expect("load final worker owner");
    assert!(matches!(owner.as_deref(), Some("worker-a" | "worker-b")));
}

#[tokio::test]
async fn incident_13651_13652_13653_fixtures_replay_at_most_once_pg() {
    let Some(pool) = pool("agentdesk_message_outbox_redrive_incident_4424").await else {
        return;
    };
    for (id, target, session) in [
        (
            13651,
            "channel:1479671301387059200",
            "catch_up_too_old:1479671301387059200:1524946021082337372",
        ),
        (
            13652,
            "channel:1490141485167808532",
            "catch_up_too_old:1490141485167808532:1523549852120645634",
        ),
        (
            13653,
            "channel:1479671298497183835",
            "catch_up_too_old:1479671298497183835:1525020393935605842",
        ),
    ] {
        seed(
            &pool,
            id,
            "failed",
            "catch_up_too_old",
            target,
            "catch-up notice",
            session,
            None,
        )
        .await;
    }
    let ids = [13651, 13652, 13653];
    let first = redrive_failed_rows(
        &pool,
        &ids,
        "issue-4424-catchup-notices-v1",
        "verified incident fixtures",
        false,
    )
    .await
    .unwrap();
    assert!(first.iter().all(|item| item.outcome == "redriven"));
    let replay = redrive_failed_rows(
        &pool,
        &ids,
        "issue-4424-catchup-notices-v1",
        "verified incident fixtures",
        false,
    )
    .await
    .unwrap();
    assert!(
        replay
            .iter()
            .all(|item| item.outcome == "idempotent_replay")
    );
    assert_eq!(audit_count(&pool).await, 3);
}
