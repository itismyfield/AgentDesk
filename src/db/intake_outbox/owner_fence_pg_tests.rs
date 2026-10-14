use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::cluster::intake_router_hook::owner_record::{
    OwnerIdentity, TransferOutcome, sweep_stale_pre_accept_claims_fenced, transfer_owner_in_tx,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use std::time::Duration;

pub(crate) async fn seed(pool: &PgPool, channel: &str, status: &str, stamp: bool) -> i64 {
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ('fence-agent', 'Fence', 'claude') ON CONFLICT DO NOTHING")
        .execute(pool).await.unwrap();
    let id = sqlx::query_scalar(
        "INSERT INTO intake_outbox
        (target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
         request_owner_id, user_text, turn_kind, agent_id, provider, status, claim_owner,
         claimed_at, owner_instance_id, owner_generation, admission_kind)
        VALUES ('fence-worker', 'leader', $1, $1, '100', 'hello', 'standard',
                'fence-agent', 'claude', $2, 'token', NOW() - INTERVAL '1 hour',
                $3, $4, 'forwarded') RETURNING id",
    )
    .bind(channel)
    .bind(status)
    .bind(stamp.then_some("fence-worker"))
    .bind(stamp.then_some(0_i64))
    .fetch_one(pool)
    .await
    .unwrap();
    if stamp {
        sqlx::query(
            "INSERT INTO intake_session_owners
            (provider, raw_channel_id, owner_instance_id, generation, status)
            VALUES ('claude', $1, 'fence-worker', 0, 'active')",
        )
        .bind(channel.trim())
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

pub(crate) async fn snapshot(pool: &PgPool, id: i64) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(io) FROM intake_outbox io WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

pub(crate) async fn owners(pool: &PgPool, channel: &str) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_agg(to_jsonb(o) ORDER BY generation)
        FROM intake_session_owners o WHERE provider='claude' AND raw_channel_id=$1",
    )
    .bind(channel.trim())
    .fetch_one(pool)
    .await
    .unwrap()
}

pub(crate) async fn transfer(tx: &mut Transaction<'_, Postgres>, channel: &str, target: &str) {
    let identity = OwnerIdentity::new("claude", channel);
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut **tx)
        .await
        .unwrap();
    assert_eq!(
        transfer_owner_in_tx(tx, &identity, "fence-worker", 0, target)
            .await
            .unwrap(),
        TransferOutcome::Transferred { new_generation: 1 }
    );
}

pub(crate) async fn move_owner(pool: &PgPool, channel: &str, target: &str) {
    let mut tx = pool.begin().await.unwrap();
    transfer(&mut tx, channel, target).await;
    tx.commit().await.unwrap();
}

const STATES: [&str; 8] = [
    "pending", "claimed", "accepted", "spawned", "claimed", "accepted", "spawned", "claimed",
];

async fn write(pool: &PgPool, id: i64, action: usize) -> Result<bool, sqlx::Error> {
    match action {
        0 => Ok(
            claim_pending_for_target(pool, "fence-worker", "claude", "token")
                .await?
                .is_some(),
        ),
        1 => mark_accepted(pool, id, "token").await,
        2 => mark_spawned(pool, id, "token").await,
        3 => mark_done(pool, id, "token").await,
        4 => mark_failed_pre_accept(pool, id, "token", "failure").await,
        5 | 6 => mark_failed_post_accept(pool, id, "token", "failure").await,
        7 => return_claimed_to_pending(pool, id, "token").await,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn owner_fence_all_writers_reject_transferred_and_same_instance_generation_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let mut violations = Vec::new();
    for target in ["successor", "fence-worker"] {
        for (action, state) in STATES.iter().enumerate() {
            let channel = format!("{target}-{action}");
            let id = seed(&pool, &channel, state, true).await;
            move_owner(&pool, &channel, target).await;
            let before = snapshot(&pool, id).await;
            let successor = owners(&pool, &channel).await;
            assert_eq!(successor[0]["status"], "superseded");
            assert_eq!(successor[1]["generation"], 1);
            assert_eq!(successor[1]["status"], "active");
            assert_eq!(successor.as_array().unwrap().len(), 2);
            let changed = write(&pool, id, action).await.unwrap();
            if changed || snapshot(&pool, id).await != before {
                violations.push(format!("stale writer {action}, {target}"));
            }
            assert_eq!(owners(&pool, &channel).await, successor);
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("; "));
    pool.close().await;
    pg.drop().await;
}

#[tokio::test]
async fn owner_fence_legacy_current_partial_and_target_matrix_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    for (kind, owner, generation) in [
        ("legacy", None, None),
        ("current", Some("fence-worker"), Some(0_i64)),
        ("owner-only", Some("fence-worker"), None),
        ("generation-only", None, Some(0)),
        ("blank", Some("\u{2003}"), Some(0)),
        ("negative", Some("fence-worker"), Some(-1)),
    ] {
        for (action, state) in STATES.iter().enumerate() {
            let channel = format!("{kind}-{action}");
            let id = seed(&pool, &channel, state, true).await;
            sqlx::query(
                "UPDATE intake_outbox SET owner_instance_id=$2, owner_generation=$3 WHERE id=$1",
            )
            .bind(id)
            .bind(owner)
            .bind(generation)
            .execute(&pool)
            .await
            .unwrap();
            if kind == "blank" {
                sqlx::query(
                    "UPDATE intake_session_owners SET owner_instance_id=$2 WHERE raw_channel_id=$1",
                )
                .bind(&channel)
                .bind(owner)
                .execute(&pool)
                .await
                .unwrap();
            }
            let before = snapshot(&pool, id).await;
            let accepted = matches!(kind, "legacy" | "current");
            assert_eq!(
                write(&pool, id, action).await.unwrap(),
                accepted,
                "{kind} writer {action}"
            );
            if !accepted {
                assert_eq!(snapshot(&pool, id).await, before);
            }
            // Keep each claim probe scoped to this case without changing rejected rows.
            sqlx::query("UPDATE intake_outbox SET target_instance_id='parked' WHERE id=$1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    let id = seed(&pool, "wrong-target", "pending", true).await;
    sqlx::query("UPDATE intake_outbox SET target_instance_id='other-worker' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let before = snapshot(&pool, id).await;
    assert!(
        claim_pending_for_target(&pool, "other-worker", "claude", "token")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(snapshot(&pool, id).await, before);
    sqlx::query("UPDATE intake_outbox SET target_instance_id='fence-worker', admission_kind='local' WHERE id=$1")
        .bind(id).execute(&pool).await.unwrap();
    assert!(
        claim_pending_for_target(&pool, "fence-worker", "claude", "token")
            .await
            .unwrap()
            .is_none()
    );
    pool.close().await;
    pg.drop().await;
}

async fn wait_for_lock(pool: &PgPool, holder: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (
                SELECT 1 FROM pg_locks held JOIN pg_locks waiter
                  ON waiter.locktype=held.locktype AND waiter.database IS NOT DISTINCT FROM held.database
                 AND waiter.classid IS NOT DISTINCT FROM held.classid
                 AND waiter.objid IS NOT DISTINCT FROM held.objid
                 AND waiter.objsubid IS NOT DISTINCT FROM held.objsubid AND waiter.mode=held.mode
                WHERE held.pid=$1 AND held.locktype='advisory' AND held.granted AND NOT waiter.granted)")
                .bind(holder).fetch_one(pool).await.unwrap();
            if waiting { return; }
            tokio::task::yield_now().await;
        }
    }).await.expect("writer must wait on the exact advisory key");
}

#[tokio::test]
async fn owner_fence_fresh_snapshot_after_transfer_wait_in_repeatable_read_session_pg() {
    transfer_wait(&[1, 2, 3, 4, 5, 6, 7]).await;
}

#[tokio::test]
async fn owner_fence_sweep_transfer_wait_revalidates_in_repeatable_read_session_pg() {
    transfer_wait(&[8, 9]).await;
}

async fn transfer_wait(actions: &[usize]) {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let rr = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_isolation='repeatable read'")
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&pg.database_url)
        .await
        .unwrap();
    for &action in actions {
        let channel = format!("waiting-{action}");
        let state = if action < 8 {
            STATES[action]
        } else {
            "claimed"
        };
        let id = seed(&pool, &channel, state, true).await;
        let before = snapshot(&pool, id).await;
        let mut tx = pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        transfer(&mut tx, &channel, "successor").await;
        let future = async {
            match action {
                8 => Ok(sweep_stale_pre_accept_claims(&rr, 1).await? != 0),
                9 => Ok(sweep_stale_pre_accept_claims_fenced(&rr, 1).await? != 0),
                _ => write(&rr, id, action).await,
            }
        };
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => panic!("writer {action} completed before transfer commit: {result:?}"),
            () = wait_for_lock(&pool, pid) => {}
        }
        tx.commit().await.unwrap();
        let successor = owners(&pool, &channel).await;
        assert!(!future.await.unwrap(), "fresh authority rejects {action}");
        assert_eq!(snapshot(&pool, id).await, before);
        assert_eq!(owners(&pool, &channel).await, successor);
    }
    rr.close().await;
    pool.close().await;
    pg.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_fence_claim_progress_normalization_and_commit_boundary_pg() {
    use crate::services::cluster::intake_worker::test_executor::{self, Checkpoint};
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let stale = seed(&pool, "stale-prefix", "pending", true).await;
    move_owner(&pool, "stale-prefix", "successor").await;
    let partial = seed(&pool, "partial-prefix", "pending", true).await;
    sqlx::query("UPDATE intake_outbox SET owner_generation=NULL WHERE id=$1")
        .bind(partial)
        .execute(&pool)
        .await
        .unwrap();
    let busy = seed(&pool, "busy-prefix", "pending", true).await;
    let mut busy_tx = pool.begin().await.unwrap();
    transfer(&mut busy_tx, "busy-prefix", "successor").await;
    let wrong_provider = seed(&pool, "cross-provider", "pending", true).await;
    sqlx::query(
        "UPDATE intake_session_owners SET provider='codex' WHERE raw_channel_id='cross-provider'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let good = seed(&pool, "\u{2003}trimmed\u{2003}", "pending", true).await;
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = seen.clone();
    let _hook = test_executor::hook(Box::new(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {})
    }));
    let before = [
        snapshot(&pool, stale).await,
        snapshot(&pool, partial).await,
        snapshot(&pool, busy).await,
        snapshot(&pool, wrong_provider).await,
    ];
    let start = std::time::Instant::now();
    let claimed = tokio::time::timeout(
        Duration::from_secs(10),
        claim_pending_for_target(&pool, "fence-worker", "claude", "token"),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, good);
    eprintln!(
        "prefix candidates={}, claim_ms={}",
        seen.load(std::sync::atomic::Ordering::SeqCst),
        start.elapsed().as_millis()
    );
    for (id, expected) in [stale, partial, busy, wrong_provider]
        .into_iter()
        .zip(before)
    {
        assert_eq!(snapshot(&pool, id).await, expected);
    }
    busy_tx.commit().await.unwrap();
    drop(_hook);
    sqlx::query("UPDATE intake_outbox SET target_instance_id='parked'")
        .execute(&pool)
        .await
        .unwrap();
    let selected = seed(&pool, "commit-after-select", "pending", true).await;
    let before = snapshot(&pool, selected).await;
    let p = pool.clone();
    let _hook = test_executor::hook(Box::new(move |point| {
        let p = p.clone();
        Box::pin(async move {
            if point == Checkpoint::ClaimSelected {
                move_owner(&p, "commit-after-select", "successor").await;
            }
        })
    }));
    assert!(
        claim_pending_for_target(&pool, "fence-worker", "claude", "token")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(snapshot(&pool, selected).await, before);
    drop(_hook);
    let normalized = seed(&pool, "provider-normalized", "pending", true).await;
    sqlx::query("UPDATE intake_outbox SET provider=$2 WHERE id=$1")
        .bind(normalized)
        .bind("\u{2003}CLAUDE\u{2003}")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        claim_pending_for_target(&pool, "fence-worker", "\u{2003}CLAUDE\u{2003}", "token")
            .await
            .unwrap()
            .unwrap()
            .id,
        normalized
    );
    pool.close().await;
    pg.drop().await;
}

#[tokio::test]
async fn owner_fence_sweeps_and_partial_progress_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let legacy = seed(&pool, "sweep-legacy", "claimed", false).await;
    let current = seed(&pool, "sweep-current", "claimed", true).await;
    let stale = seed(&pool, "sweep-stale", "claimed", true).await;
    move_owner(&pool, "sweep-stale", "successor").await;
    let partial = seed(&pool, "sweep-partial", "claimed", true).await;
    sqlx::query("UPDATE intake_outbox SET owner_generation=NULL WHERE id=$1")
        .bind(partial)
        .execute(&pool)
        .await
        .unwrap();
    let before = (snapshot(&pool, stale).await, snapshot(&pool, partial).await);
    assert_eq!(
        sweep_stale_pre_accept_claims_fenced(&pool, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(snapshot(&pool, legacy).await["status"], "claimed");
    assert_eq!(snapshot(&pool, current).await["status"], "pending");
    assert_eq!(sweep_stale_pre_accept_claims(&pool, 1).await.unwrap(), 1);
    assert_eq!(snapshot(&pool, legacy).await["status"], "pending");
    assert_eq!(
        (snapshot(&pool, stale).await, snapshot(&pool, partial).await),
        before
    );
    let first = seed(&pool, "progress-first", "claimed", true).await;
    let error = seed(&pool, "progress-error", "claimed", true).await;
    let last = seed(&pool, "progress-last", "claimed", true).await;
    sqlx::raw_sql("CREATE FUNCTION refuse_sweep_fixture() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF OLD.channel_id='progress-error' THEN RAISE EXCEPTION 'injected sweep failure'; END IF;
        RETURN NEW; END $$;
        CREATE TRIGGER refuse_sweep_fixture BEFORE UPDATE ON intake_outbox
        FOR EACH ROW EXECUTE FUNCTION refuse_sweep_fixture();").execute(&pool).await.unwrap();
    assert!(
        sweep_stale_pre_accept_claims_fenced(&pool, 1)
            .await
            .is_err()
    );
    assert_eq!(
        snapshot(&pool, first).await["status"],
        "pending",
        "previous per-row commit survives"
    );
    for id in [error, last] {
        assert_eq!(snapshot(&pool, id).await["status"], "claimed");
    }
    pool.close().await;
    pg.drop().await;
}

#[tokio::test]
async fn owner_fence_capacity_and_replay_projection_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, capabilities)
        VALUES ('fence-worker', '{\"execution_capacity\":{\"version\":1,\"slots\":1}}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let normal = seed(&pool, "normal-done", "spawned", true).await;
    sqlx::query(
        "UPDATE intake_outbox SET replay_disposition='started_unclassified',
        replay_source_message_ids=ARRAY[user_msg_id], replay_episode_nonce='nonce',
        replay_preserved=$2 WHERE id=$1",
    )
    .bind(normal)
    .bind(json!({"projection":{"nonce":"nonce", "opaque":[1,2]}, "kept":true}))
    .execute(&pool)
    .await
    .unwrap();
    let before = snapshot(&pool, normal).await;
    assert_eq!(occupancy(&pool).await, 1);
    assert!(mark_done(&pool, normal, "token").await.unwrap());
    assert_eq!(occupancy(&pool).await, 0);
    let after = snapshot(&pool, normal).await;
    for column in [
        "replay_episode_nonce",
        "replay_preserved",
        "replay_disposition",
        "replay_source_message_ids",
    ] {
        assert_eq!(after[column], before[column]);
    }
    for (channel, state, action) in [
        ("capacity-done", "spawned", 3),
        ("capacity-rearm", "claimed", 7),
    ] {
        let id = seed(&pool, channel, state, true).await;
        move_owner(&pool, channel, "successor").await;
        let before = snapshot(&pool, id).await;
        assert!(!write(&pool, id, action).await.unwrap());
        assert_eq!(snapshot(&pool, id).await, before);
        assert_eq!(occupancy(&pool).await, 1);
        let error = sqlx::query(
            "INSERT INTO intake_outbox
            (target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
             request_owner_id, user_text, turn_kind, agent_id, provider)
            VALUES ('fence-worker', 'leader', 'other-channel', 'other-message', '100', 'hello',
                    'standard', 'fence-agent', 'claude')",
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().constraint(),
            Some("node_execution_capacity_available")
        );
        sqlx::query("UPDATE intake_outbox SET status='unknown' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    pg.drop().await;
}

async fn occupancy(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM node_execution_occupancy('fence-worker')")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn owner_fence_lock_survives_replay_trigger_and_two_writers_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let id = seed(&pool, "lock-held", "spawned", true).await;
    let mut replay = pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *replay)
        .await
        .unwrap();
    sqlx::query("SELECT replay_lock_sources('claude', 'lock-held', ARRAY['lock-held'])")
        .execute(&mut *replay)
        .await
        .unwrap();
    let writer = mark_done(&pool, id, "token");
    tokio::pin!(writer);
    tokio::select! {
        result = &mut writer => panic!("UPDATE must reach replay trigger: {result:?}"),
        () = wait_for_lock(&pool, pid) => {}
    }
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(OwnerIdentity::new("claude", "lock-held").advisory_key())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !free,
        "owner lock must remain held while UPDATE awaits replay trigger"
    );
    replay.commit().await.unwrap();
    assert!(writer.await.unwrap());
    let id = seed(&pool, "two-writers", "spawned", true).await;
    let (a, b) = tokio::join!(mark_done(&pool, id, "token"), mark_done(&pool, id, "token"));
    assert_ne!(a.unwrap(), b.unwrap(), "exactly one writer wins");
    let before = snapshot(&pool, id).await;
    assert!(!mark_done(&pool, id, "token").await.unwrap());
    assert_eq!(snapshot(&pool, id).await, before);
    pool.close().await;
    pg.drop().await;
}

#[tokio::test]
async fn owner_fence_snapshot_cas_and_sweep_token_age_revalidation_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    for mode in 0..3 {
        let channel = format!("cas-{mode}");
        let id = seed(
            &pool,
            &channel,
            if mode == 0 { "spawned" } else { "claimed" },
            true,
        )
        .await;
        let mut lock = pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(OwnerIdentity::new("claude", &channel).advisory_key())
            .execute(&mut *lock)
            .await
            .unwrap();
        let writer = async {
            if mode == 0 {
                mark_done(&pool, id, "token").await
            } else {
                Ok(sweep_stale_pre_accept_claims_fenced(&pool, 1).await? != 0)
            }
        };
        tokio::pin!(writer);
        tokio::select! {
            result = &mut writer => panic!("must read before the owner lock: {result:?}"),
            () = wait_for_lock(&pool, pid) => {}
        }
        let query = match mode {
            0 => "UPDATE intake_outbox SET target_instance_id='restamped-target' WHERE id=$1",
            1 => "UPDATE intake_outbox SET claim_owner='new-token' WHERE id=$1",
            _ => "UPDATE intake_outbox SET claimed_at=NOW() WHERE id=$1",
        };
        sqlx::query(query).bind(id).execute(&pool).await.unwrap();
        let before = snapshot(&pool, id).await;
        lock.commit().await.unwrap();
        assert!(
            !writer.await.unwrap(),
            "snapshot CAS/token/age must be fresh"
        );
        assert_eq!(snapshot(&pool, id).await, before);
        sqlx::query("UPDATE intake_outbox SET status='unknown' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    pg.drop().await;
}

#[tokio::test]
async fn owner_fence_legacy_read_cannot_overwrite_new_stamp_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    for (case, owner, generation) in [
        ("full", Some("new-owner"), Some(1_i64)),
        ("owner-only", Some("new-owner"), None),
        ("generation-only", None, Some(1)),
    ] {
        let channel = format!("legacy-cas-{case}");
        let id = seed(&pool, &channel, "spawned", false).await;
        let mut lock = pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(id)
            .execute(&mut *lock)
            .await
            .unwrap();
        let writer = mark_done(&pool, id, "token");
        tokio::pin!(writer);
        let blocked = async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))")
                .bind(pid).fetch_one(&pool).await.unwrap();
                if waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            result = &mut writer => panic!("row lock must block UPDATE: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(10), blocked) => result.unwrap()
        }
        sqlx::query(
            "UPDATE intake_outbox SET owner_instance_id=$2, owner_generation=$3 WHERE id=$1",
        )
        .bind(id)
        .bind(owner)
        .bind(generation)
        .execute(&mut *lock)
        .await
        .unwrap();
        lock.commit().await.unwrap();
        let before = snapshot(&pool, id).await;
        assert!(
            !writer.await.unwrap(),
            "legacy UPDATE requires NULL/NULL at the write boundary"
        );
        assert_eq!(snapshot(&pool, id).await, before);
    }
    pool.close().await;
    pg.drop().await;
}
