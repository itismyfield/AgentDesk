use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    db: DispatchPostgresTestDb,
    pool: PgPool,
    peer: PgPool,
}

impl Fixture {
    async fn create() -> Self {
        let db = DispatchPostgresTestDb::create("imsg_reply", "imsg reply S0").await;
        let pool = db.connect_and_migrate_with_max_connections(2).await;
        let tables: (bool, bool) = sqlx::query_as(
            "SELECT to_regclass('imsg_reply_obligations') IS NOT NULL,
                    to_regclass('imsg_reply_actions') IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tables, (true, true), "S0 migration must actually execute");
        let peer = db.connect_and_migrate().await;
        Self { db, pool, peer }
    }

    async fn close(self) {
        self.pool.close().await;
        self.peer.close().await;
        self.db.drop().await;
    }
}

#[derive(Default)]
struct FakeSender {
    starts: AtomicUsize,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl FakeSender {
    fn start(&self) {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
    }

    fn finish(&self) {
        assert!(self.active.fetch_sub(1, Ordering::SeqCst) > 0);
    }

    fn assert(&self, starts: usize) {
        assert_eq!(self.starts.load(Ordering::SeqCst), starts);
        assert!(self.maximum.load(Ordering::SeqCst) <= 1);
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
    }
}

async fn seed(pool: &PgPool, origin: &str, admission: &str, expired: bool) {
    sqlx::query(
        "INSERT INTO imsg_reply_obligations
         (origin_id,agent_id,channel_id,person_id,request_hash,opened_at,expires_at,
          admission,admission_kind,episode_key,terminal_seen_at)
         SELECT $1,'agent','channel',1,'hash',t,t+INTERVAL '24 hours',
                $2,'started','episode',NOW()
         FROM (SELECT NOW()-CASE WHEN $3 THEN INTERVAL '25 hours'
                                ELSE INTERVAL '0 hours' END AS t) AS clock",
    )
    .bind(origin)
    .bind(admission)
    .bind(expired)
    .execute(pool)
    .await
    .unwrap();
}

fn action<'a>(origin: &'a str, purpose: Purpose, key: &'a str) -> ClaimAction<'a> {
    ClaimAction {
        id: Uuid::new_v4(),
        origin_id: origin,
        purpose,
        action_key: key,
        reply_episode_key: Some("episode"),
    }
}

async fn ack(pool: &PgPool, origin: &str, id: Uuid, outcome: AckOutcome) -> bool {
    ack_action(
        pool,
        &ActionAck {
            origin_id: origin,
            id,
            receipt: matches!(outcome, AckOutcome::Sent).then(|| json!({"message_id":"receipt"})),
            outcome,
        },
    )
    .await
    .unwrap()
}

async fn raw_action(
    pool: &PgPool,
    origin: &str,
    id: Uuid,
    purpose: &str,
    key: &str,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO imsg_reply_actions (id,origin_id,purpose,action_key,status,sent_at)
         VALUES ($1,$2,$3,$4,$5,CASE WHEN $5='sent' THEN NOW() ELSE NULL END)",
    )
    .bind(id)
    .bind(origin)
    .bind(purpose)
    .bind(key)
    .bind(status)
    .execute(pool)
    .await
    .map(|_| ())
}

fn assert_sqlstate(error: sqlx::Error, code: &str) {
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some(code)
    );
}

async fn snapshot(
    pool: &PgPool,
    origin: &str,
    resolution: &str,
    active: Option<Uuid>,
    actions: &[(&str, &str)],
) {
    let state = read_obligation(pool, origin).await.unwrap().unwrap();
    assert_eq!(state.origin_id, origin);
    assert_eq!(
        (state.resolution, state.active_send_action),
        (resolution.to_string(), active)
    );
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT purpose,status FROM imsg_reply_actions WHERE origin_id=$1 ORDER BY purpose,action_key",
    )
    .bind(origin)
    .fetch_all(pool)
    .await
    .unwrap();
    let expected: Vec<_> = actions
        .iter()
        .map(|(purpose, status)| (purpose.to_string(), status.to_string()))
        .collect();
    assert_eq!(rows, expected);
}

#[tokio::test]
async fn pg_s0_c1_cross_purpose_unique_has_one_winner() {
    let f = Fixture::create().await;
    seed(&f.pool, "c1", "admitted", true).await;
    let (a, b) = tokio::join!(
        raw_action(&f.pool, "c1", Uuid::new_v4(), "final", "a", "claimed"),
        raw_action(&f.peer, "c1", Uuid::new_v4(), "expiry", "b", "claimed")
    );
    let sender = FakeSender::default();
    for result in [a, b] {
        match result {
            Ok(()) => sender.start(),
            Err(error) => assert_sqlstate(error, "23505"),
        }
    }
    assert_eq!(sender.starts.load(Ordering::SeqCst), 1);
    sender.finish();
    sender.assert(1);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM imsg_reply_actions WHERE origin_id='c1' AND status='claimed'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    f.close().await;
}

#[tokio::test]
async fn pg_s0_c2_occupied_completed_token_still_blocks_cas() {
    let f = Fixture::create().await;
    seed(&f.pool, "c2", "admitted", false).await;
    let occupied = Uuid::new_v4();
    raw_action(&f.pool, "c2", occupied, "progress", "old", "sent")
        .await
        .unwrap();
    sqlx::query("UPDATE imsg_reply_obligations SET active_send_action=$1 WHERE origin_id='c2'")
        .bind(occupied)
        .execute(&f.pool)
        .await
        .unwrap();
    let sender = FakeSender::default();
    if claim_action(&f.peer, &action("c2", Purpose::Final, "new"))
        .await
        .unwrap()
        .is_some()
    {
        sender.start();
        sender.finish();
    }
    sender.assert(0);
    snapshot(
        &f.pool,
        "c2",
        "open",
        Some(occupied),
        &[("progress", "sent")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_s0_c3_wrong_origin_ack_and_deferred_composite_fk_reject() {
    let f = Fixture::create().await;
    for origin in ["c3-a", "c3-b"] {
        seed(&f.pool, origin, "admitted", false).await;
    }
    let a = claim_action(&f.pool, &action("c3-a", Purpose::Final, "a"))
        .await
        .unwrap()
        .unwrap();
    let b = claim_action(&f.peer, &action("c3-b", Purpose::Final, "b"))
        .await
        .unwrap()
        .unwrap();
    assert!(!ack(&f.peer, "c3-b", a, AckOutcome::Sent).await);
    snapshot(&f.pool, "c3-a", "open", Some(a), &[("final", "claimed")]).await;
    snapshot(&f.pool, "c3-b", "open", Some(b), &[("final", "claimed")]).await;
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("UPDATE imsg_reply_obligations SET active_send_action=$1 WHERE origin_id='c3-b'")
        .bind(a)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_sqlstate(tx.commit().await.unwrap_err(), "23503");
    snapshot(&f.pool, "c3-b", "open", Some(b), &[("final", "claimed")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_s0_c4_mirror_unknown_excluded_from_external_index() {
    let f = Fixture::create().await;
    seed(&f.pool, "c4", "admitted", false).await;
    raw_action(&f.pool, "c4", Uuid::new_v4(), "mirror", "m", "unknown")
        .await
        .unwrap();
    let claimed = claim_action(&f.peer, &action("c4", Purpose::Final, "f"))
        .await
        .unwrap();
    assert!(claimed.is_some(), "mirror unknown must not block final");
    let id = claimed.unwrap();
    let sender = FakeSender::default();
    sender.start();
    sender.finish();
    sender.assert(1);
    snapshot(
        &f.pool,
        "c4",
        "open",
        Some(id),
        &[("final", "claimed"), ("mirror", "unknown")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_r9_mirror_unknown_does_not_occupy_external_slot() {
    let f = Fixture::create().await;
    seed(&f.pool, "c4", "admitted", false).await;
    let mirror = claim_action(&f.pool, &action("c4", Purpose::Mirror, "m"))
        .await
        .unwrap()
        .unwrap();
    assert!(ack(&f.pool, "c4", mirror, AckOutcome::Unknown).await);
    snapshot(&f.pool, "c4", "open", None, &[("mirror", "unknown")]).await;
    let claimed = claim_action(&f.peer, &action("c4", Purpose::Final, "f"))
        .await
        .unwrap();
    assert!(claimed.is_some(), "mirror failure must not block final");
    let final_id = claimed.unwrap();
    let sender = FakeSender::default();
    sender.start();
    assert!(ack(&f.peer, "c4", final_id, AckOutcome::Sent).await);
    sender.finish();
    sender.assert(1);
    snapshot(
        &f.pool,
        "c4",
        "replied",
        None,
        &[("final", "sent"), ("mirror", "unknown")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_s0_c5_unknown_blocks_direct_insert_without_active_token() {
    let f = Fixture::create().await;
    seed(&f.pool, "c5", "admitted", true).await;
    raw_action(&f.pool, "c5", Uuid::new_v4(), "progress", "p", "unknown")
        .await
        .unwrap();
    assert_sqlstate(
        raw_action(&f.peer, "c5", Uuid::new_v4(), "expiry", "bypass", "claimed")
            .await
            .unwrap_err(),
        "23505",
    );
    assert!(
        claim_action(&f.peer, &action("c5", Purpose::Expiry, "automatic"))
            .await
            .unwrap()
            .is_none()
    );
    FakeSender::default().assert(0);
    snapshot(&f.pool, "c5", "open", None, &[("progress", "unknown")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_r6_unknown_preserves_slot_and_rejects_expiry_after_crash() {
    let f = Fixture::create().await;
    seed(&f.pool, "c5", "admitted", true).await;
    let id = claim_action(&f.pool, &action("c5", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    let sender = FakeSender::default();
    sender.start();
    sender.finish();
    assert!(ack(&f.pool, "c5", id, AckOutcome::Unknown).await);
    assert!(
        claim_action(&f.peer, &action("c5", Purpose::Expiry, "expiry"))
            .await
            .unwrap()
            .is_none()
    );
    assert_sqlstate(
        raw_action(&f.peer, "c5", Uuid::new_v4(), "expiry", "bypass", "claimed")
            .await
            .unwrap_err(),
        "23505",
    );
    sender.assert(1);
    snapshot(&f.pool, "c5", "open", Some(id), &[("final", "unknown")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_s0_c6_lifetime_one_shots_survive_not_sent() {
    let f = Fixture::create().await;
    for (purpose, name) in [
        (Purpose::Fallback, "fallback"),
        (Purpose::Expiry, "expiry"),
        (Purpose::Mirror, "mirror"),
    ] {
        seed(&f.pool, name, "admitted", true).await;
        let id = claim_action(&f.pool, &action(name, purpose, "first"))
            .await
            .unwrap()
            .unwrap();
        assert!(ack(&f.pool, name, id, AckOutcome::NotSent).await);
        assert_sqlstate(
            raw_action(&f.peer, name, Uuid::new_v4(), name, "different", "not_sent")
                .await
                .unwrap_err(),
            "23505",
        );
        snapshot(&f.pool, name, "open", None, &[(name, "not_sent")]).await;
    }
    f.close().await;
}

#[tokio::test]
async fn pg_r1_final_claim_blocks_fallback_before_ack() {
    let f = Fixture::create().await;
    seed(&f.pool, "r1", "admitted", false).await;
    let id = claim_action(&f.pool, &action("r1", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    let sender = FakeSender::default();
    sender.start();
    if claim_action(&f.peer, &action("r1", Purpose::Fallback, "b"))
        .await
        .unwrap()
        .is_some()
    {
        sender.start();
        sender.finish();
    }
    sender.finish();
    sender.assert(1);
    snapshot(&f.pool, "r1", "open", Some(id), &[("final", "claimed")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_r2_fallback_claim_blocks_final_before_ack() {
    let f = Fixture::create().await;
    seed(&f.pool, "r2", "admitted", false).await;
    let id = claim_action(&f.pool, &action("r2", Purpose::Fallback, "b"))
        .await
        .unwrap()
        .unwrap();
    let sender = FakeSender::default();
    sender.start();
    if claim_action(&f.peer, &action("r2", Purpose::Final, "f"))
        .await
        .unwrap()
        .is_some()
    {
        sender.start();
        sender.finish();
    }
    sender.finish();
    sender.assert(1);
    snapshot(&f.pool, "r2", "open", Some(id), &[("fallback", "claimed")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_r3_fallback_sent_keeps_obligation_open_for_final() {
    let f = Fixture::create().await;
    seed(&f.pool, "r3", "admitted", false).await;
    let sender = FakeSender::default();
    let fallback = claim_action(&f.pool, &action("r3", Purpose::Fallback, "b"))
        .await
        .unwrap()
        .unwrap();
    sender.start();
    assert!(ack(&f.pool, "r3", fallback, AckOutcome::Sent).await);
    sender.finish();
    snapshot(&f.pool, "r3", "open", None, &[("fallback", "sent")]).await;
    let final_id = claim_action(&f.peer, &action("r3", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    sender.start();
    assert!(ack(&f.peer, "r3", final_id, AckOutcome::Sent).await);
    sender.finish();
    sender.assert(2);
    snapshot(
        &f.pool,
        "r3",
        "replied",
        None,
        &[("fallback", "sent"), ("final", "sent")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_r4_final_sent_blocks_fallback_and_expiry() {
    let f = Fixture::create().await;
    seed(&f.pool, "r4", "admitted", true).await;
    let id = claim_action(&f.pool, &action("r4", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    let sender = FakeSender::default();
    sender.start();
    assert!(ack(&f.pool, "r4", id, AckOutcome::Sent).await);
    sender.finish();
    for purpose in [Purpose::Fallback, Purpose::Expiry] {
        if claim_action(&f.peer, &action("r4", purpose, "late"))
            .await
            .unwrap()
            .is_some()
        {
            sender.start();
            sender.finish();
        }
    }
    sender.assert(1);
    snapshot(&f.pool, "r4", "replied", None, &[("final", "sent")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_r5_stale_claim_survives_reconnect_without_resend() {
    let f = Fixture::create().await;
    seed(&f.pool, "r5", "admitted", true).await;
    let id = claim_action(&f.pool, &action("r5", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    let sender = FakeSender::default();
    sender.start();
    sender.finish();
    sqlx::query("UPDATE imsg_reply_actions SET claimed_at=NOW()-INTERVAL '2 days' WHERE id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
    f.pool.close().await;
    let restarted = f.db.connect_and_migrate().await;
    for purpose in [Purpose::Fallback, Purpose::Expiry] {
        if claim_action(&restarted, &action("r5", purpose, "restart"))
            .await
            .unwrap()
            .is_some()
        {
            sender.start();
            sender.finish();
        }
    }
    sender.assert(1);
    snapshot(&restarted, "r5", "open", Some(id), &[("final", "claimed")]).await;
    restarted.close().await;
    f.close().await;
}

#[tokio::test]
async fn pg_r7_final_and_expiry_concurrent_claims_start_one_sender() {
    let f = Fixture::create().await;
    seed(&f.pool, "r7", "admitted", true).await;
    sqlx::query("UPDATE imsg_reply_obligations SET opened_at=NOW()-INTERVAL '24 hours',expires_at=NOW() WHERE origin_id='r7'")
        .execute(&f.pool).await.unwrap();
    let final_claim = action("r7", Purpose::Final, "f");
    let expiry_claim = action("r7", Purpose::Expiry, "e");
    let sender = FakeSender::default();
    let barrier = tokio::sync::Barrier::new(2);
    let attempt = async |pool: &PgPool, claim: &ClaimAction<'_>| {
        barrier.wait().await;
        let result = claim_action(pool, claim).await.unwrap();
        if result.is_some() {
            sender.start();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            sender.finish();
        }
        result
    };
    let (a, b) = tokio::join!(
        attempt(&f.pool, &final_claim),
        attempt(&f.peer, &expiry_claim)
    );
    assert_eq!(usize::from(a.is_some()) + usize::from(b.is_some()), 1);
    sender.assert(1);
    let (id, name) = if let Some(id) = a {
        (id, "final")
    } else {
        (b.unwrap(), "expiry")
    };
    snapshot(&f.pool, "r7", "open", Some(id), &[(name, "claimed")]).await;
    assert!(ack(&f.pool, "r7", id, AckOutcome::Sent).await);
    snapshot(
        &f.pool,
        "r7",
        if name == "final" {
            "replied"
        } else {
            "expired"
        },
        None,
        &[(name, "sent")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_r8_progress_receipt_blocks_fallback_only_for_same_episode() {
    let f = Fixture::create().await;
    seed(&f.pool, "r8", "admitted", false).await;
    let id = claim_action(&f.pool, &action("r8", Purpose::Progress, "p"))
        .await
        .unwrap()
        .unwrap();
    let sender = FakeSender::default();
    sender.start();
    assert!(ack(&f.pool, "r8", id, AckOutcome::Sent).await);
    sender.finish();
    let reply: (Option<String>, bool) = sqlx::query_as("SELECT last_reply_episode,last_reply_at IS NOT NULL FROM imsg_reply_obligations WHERE origin_id='r8'")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(reply, (Some("episode".into()), true));
    assert!(
        claim_action(&f.peer, &action("r8", Purpose::Fallback, "same"))
            .await
            .unwrap()
            .is_none()
    );
    snapshot(&f.pool, "r8", "open", None, &[("progress", "sent")]).await;
    sqlx::query("UPDATE imsg_reply_obligations SET episode_key='next' WHERE origin_id='r8'")
        .execute(&f.pool)
        .await
        .unwrap();
    let fallback = claim_action(&f.peer, &action("r8", Purpose::Fallback, "next"))
        .await
        .unwrap()
        .unwrap();
    sender.start();
    assert!(ack(&f.peer, "r8", fallback, AckOutcome::NotSent).await);
    sender.finish();
    sender.assert(2);
    snapshot(
        &f.pool,
        "r8",
        "open",
        None,
        &[("fallback", "not_sent"), ("progress", "sent")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_expiry_exact_now_boundary_and_premature_guard() {
    let f = Fixture::create().await;
    seed(&f.pool, "boundary", "admitted", false).await;
    assert!(
        claim_action(&f.peer, &action("boundary", Purpose::Expiry, "early"))
            .await
            .unwrap()
            .is_none()
    );
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("UPDATE imsg_reply_obligations SET opened_at=NOW()-INTERVAL '24 hours',expires_at=NOW() WHERE origin_id='boundary'")
        .execute(&mut *tx).await.unwrap();
    let exact: (bool, bool) = sqlx::query_as("SELECT expires_at=NOW(),expires_at<NOW() FROM imsg_reply_obligations WHERE origin_id='boundary'")
        .fetch_one(&mut *tx).await.unwrap();
    assert_eq!(exact, (true, false));
    let claim = action("boundary", Purpose::Expiry, "exact");
    let id: Option<Uuid> = sqlx::query_scalar(CLAIM_SQL)
        .bind(claim.origin_id)
        .bind(claim.id)
        .bind(claim.purpose.as_str())
        .bind(claim.action_key)
        .bind(claim.reply_episode_key)
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
    assert_eq!(id, Some(claim.id));
    tx.commit().await.unwrap();
    assert!(ack(&f.peer, "boundary", claim.id, AckOutcome::Sent).await);
    assert!(
        claim_action(&f.pool, &action("boundary", Purpose::Final, "late"))
            .await
            .unwrap()
            .is_none()
    );
    snapshot(&f.pool, "boundary", "expired", None, &[("expiry", "sent")]).await;
    f.close().await;
}

#[tokio::test]
async fn pg_early_final_ack_keeps_admission_and_resolution_independent() {
    let f = Fixture::create().await;
    seed(&f.pool, "early", "admitting", false).await;
    for purpose in [
        Purpose::Progress,
        Purpose::Fallback,
        Purpose::Expiry,
        Purpose::Mirror,
    ] {
        assert!(
            claim_action(&f.peer, &action("early", purpose, "pending"))
                .await
                .unwrap()
                .is_none()
        );
    }
    let id = claim_action(&f.pool, &action("early", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE imsg_reply_obligations SET opened_at=NOW()-INTERVAL '24 hours',expires_at=NOW() WHERE origin_id='early'")
        .execute(&f.pool).await.unwrap();
    assert!(ack(&f.peer, "early", id, AckOutcome::Sent).await);
    sqlx::query("UPDATE imsg_reply_obligations SET admission='admitted',admission_result='{\"kind\":\"started\"}'::jsonb WHERE origin_id='early'")
        .execute(&f.pool).await.unwrap();
    snapshot(&f.pool, "early", "replied", None, &[("final", "sent")]).await;
    for admission in ["refused", "unknown"] {
        seed(&f.pool, admission, admission, true).await;
        for purpose in [Purpose::Final, Purpose::Fallback, Purpose::Expiry] {
            assert!(
                claim_action(&f.peer, &action(admission, purpose, "denied"))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        snapshot(&f.pool, admission, "open", None, &[]).await;
    }
    f.close().await;
}

#[tokio::test]
async fn pg_duplicate_action_key_rolls_back_slot_and_not_sent_allows_new_key() {
    let f = Fixture::create().await;
    seed(&f.pool, "duplicate", "admitted", false).await;
    let first = action("duplicate", Purpose::Progress, "key");
    let id = claim_action(&f.pool, &first).await.unwrap().unwrap();
    assert!(ack(&f.pool, "duplicate", id, AckOutcome::NotSent).await);
    assert!(
        claim_action(&f.peer, &action("duplicate", Purpose::Progress, "key"))
            .await
            .unwrap()
            .is_none()
    );
    snapshot(
        &f.pool,
        "duplicate",
        "open",
        None,
        &[("progress", "not_sent")],
    )
    .await;
    let reused_id = ClaimAction {
        id,
        ..action("duplicate", Purpose::Final, "other")
    };
    assert!(claim_action(&f.peer, &reused_id).await.unwrap().is_none());
    snapshot(
        &f.pool,
        "duplicate",
        "open",
        None,
        &[("progress", "not_sent")],
    )
    .await;
    let next = claim_action(&f.pool, &action("duplicate", Purpose::Final, "next"))
        .await
        .unwrap()
        .unwrap();
    snapshot(
        &f.pool,
        "duplicate",
        "open",
        Some(next),
        &[("final", "claimed"), ("progress", "not_sent")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_ack_receipt_validation_idempotence_and_late_evidence() {
    let f = Fixture::create().await;
    seed(&f.pool, "ack", "admitted", false).await;
    let id = claim_action(&f.pool, &action("ack", Purpose::Final, "f"))
        .await
        .unwrap()
        .unwrap();
    for receipt in [None, Some(serde_json::Value::Null)] {
        assert!(
            !ack_action(
                &f.peer,
                &ActionAck {
                    origin_id: "ack",
                    id,
                    outcome: AckOutcome::Sent,
                    receipt
                }
            )
            .await
            .unwrap()
        );
    }
    snapshot(&f.pool, "ack", "open", Some(id), &[("final", "claimed")]).await;
    assert!(ack(&f.pool, "ack", id, AckOutcome::Unknown).await);
    assert!(ack(&f.peer, "ack", id, AckOutcome::Sent).await);
    assert!(ack(&f.peer, "ack", id, AckOutcome::Sent).await);
    assert!(!ack(&f.peer, "ack", id, AckOutcome::NotSent).await);
    assert!(
        !ack_action(
            &f.peer,
            &ActionAck {
                origin_id: "ack",
                id,
                outcome: AckOutcome::Sent,
                receipt: Some(json!({"message_id":"different"}))
            }
        )
        .await
        .unwrap()
    );
    snapshot(&f.pool, "ack", "replied", None, &[("final", "sent")]).await;
    let persisted = read_action(&f.pool, id).await.unwrap().unwrap();
    assert_eq!(
        (
            persisted.id,
            persisted.origin_id.as_str(),
            persisted.purpose.as_str(),
            persisted.status.as_str()
        ),
        (id, "ack", "final", "sent")
    );
    assert_eq!(persisted.receipt, Some(json!({"message_id":"receipt"})));
    assert!(persisted.sent_at.is_some());
    let timestamps: (bool, bool) = sqlx::query_as("SELECT a.sent_at IS NOT NULL,o.closed_at IS NOT NULL FROM imsg_reply_actions a JOIN imsg_reply_obligations o USING(origin_id) WHERE a.id=$1")
        .bind(id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(timestamps, (true, true));
    f.close().await;
}

#[tokio::test]
async fn pg_fallback_requires_confirmed_episode_and_terminal_evidence() {
    let f = Fixture::create().await;
    for (origin, assignment) in [
        ("no-episode", "episode_key=NULL"),
        ("no-terminal", "terminal_seen_at=NULL"),
        (
            "unconfirmed",
            "admission_kind='unconfirmed',delivery_confirmed=FALSE",
        ),
    ] {
        seed(&f.pool, origin, "admitted", false).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE imsg_reply_obligations SET {assignment} WHERE origin_id=$1"
        )))
        .bind(origin)
        .execute(&f.pool)
        .await
        .unwrap();
        assert!(
            claim_action(&f.peer, &action(origin, Purpose::Fallback, "blocked"))
                .await
                .unwrap()
                .is_none()
        );
        snapshot(&f.pool, origin, "open", None, &[]).await;
    }
    sqlx::query(
        "UPDATE imsg_reply_obligations SET delivery_confirmed=TRUE WHERE origin_id='unconfirmed'",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    let id = claim_action(
        &f.peer,
        &action("unconfirmed", Purpose::Fallback, "confirmed"),
    )
    .await
    .unwrap()
    .unwrap();
    snapshot(
        &f.pool,
        "unconfirmed",
        "open",
        Some(id),
        &[("fallback", "claimed")],
    )
    .await;
    f.close().await;
}

#[tokio::test]
async fn pg_migration_rerun_preserves_checksum_and_check_contract() {
    let f = Fixture::create().await;
    seed(&f.pool, "ddl", "admitted", false).await;
    let id = Uuid::new_v4();
    raw_action(&f.pool, "ddl", id, "progress", "p", "sent")
        .await
        .unwrap();
    let before: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=139")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    crate::db::postgres::migrate(&f.pool).await.unwrap();
    let after: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=139")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(before.len(), 48);
    assert_eq!(before, after);
    let version: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*),MAX(version) FROM _sqlx_migrations WHERE version=139 AND success",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(version, (1, 139));
    for assignment in [
        "source='discord'",
        "person_id=0",
        "origin_id=''",
        "origin_id=repeat('x',257)",
        "expires_at=opened_at",
        "admission='bad'",
        "admission_kind='bad'",
        "resolution='bad'",
    ] {
        assert_sqlstate(
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE imsg_reply_obligations SET {assignment} WHERE origin_id='ddl'"
            )))
            .execute(&f.pool)
            .await
            .unwrap_err(),
            "23514",
        );
    }
    for (purpose, status) in [("invalid", "claimed"), ("final", "invalid")] {
        assert_sqlstate(
            raw_action(&f.pool, "ddl", Uuid::new_v4(), purpose, "invalid", status)
                .await
                .unwrap_err(),
            "23514",
        );
    }
    for assignment in ["sent_at=NULL", "status='not_sent'"] {
        assert_sqlstate(
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE imsg_reply_actions SET {assignment} WHERE id=$1"
            )))
            .bind(id)
            .execute(&f.pool)
            .await
            .unwrap_err(),
            "23514",
        );
    }
    assert_sqlstate(
        sqlx::query("DELETE FROM imsg_reply_obligations WHERE origin_id='ddl'")
            .execute(&f.pool)
            .await
            .unwrap_err(),
        "23503",
    );
    snapshot(&f.pool, "ddl", "open", None, &[("progress", "sent")]).await;
    let restored = read_action(&f.pool, id).await.unwrap().unwrap();
    assert_eq!(
        (restored.origin_id.as_str(), restored.status.as_str()),
        ("ddl", "sent")
    );
    f.close().await;
}
