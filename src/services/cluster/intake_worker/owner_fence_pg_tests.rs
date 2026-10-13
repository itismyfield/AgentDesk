use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::owner_fence_pg_tests::{move_owner, owners, seed, snapshot};
use crate::services::discord::input_runtime::fence::{Closing, Gate, test_health};
use crate::services::provider::ProviderKind;
use serde_json::Value;
use std::sync::Mutex;
use test_executor::Checkpoint;

async fn transfer_at(point: Checkpoint, fail: bool, channel: u64) {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let id = seed(&pool, &channel.to_string(), "pending", true).await;
    let (_registry, shared) =
        crate::services::discord::health::owner_runtime_for_tests::registered("claude").await;
    let at_boundary = Arc::new(Mutex::new(None::<(Value, Value)>));
    let (captured, p) = (at_boundary.clone(), pool.clone());
    let _hook = test_executor::hook(Box::new(move |seen| {
        let (captured, p) = (captured.clone(), p.clone());
        Box::pin(async move {
            if seen == point {
                move_owner(&p, &channel.to_string(), "successor").await;
                *captured.lock().unwrap() = Some((
                    snapshot(&p, id).await,
                    owners(&p, &channel.to_string()).await,
                ));
            }
        })
    }));
    let recorder = test_executor::record();
    if fail {
        test_executor::fail_execution();
    }
    let outcome =
        run_intake_worker_tick(&pool, &shared, "fence-worker", "claude", "token", &|| false)
            .await
            .unwrap();
    let (before, successor) = at_boundary
        .lock()
        .unwrap()
        .clone()
        .expect("real tick checkpoint reached");
    assert_eq!(
        snapshot(&pool, id).await,
        before,
        "tick must reject all stale writes, including fallback rearm"
    );
    assert_eq!(owners(&pool, &channel.to_string()).await, successor);
    if point == Checkpoint::FinalDb {
        assert_eq!(recorder.channels(), vec![channel]);
        assert_eq!(outcome, TickOutcome::Processed);
        assert_eq!(before["status"], "spawned");
    } else {
        assert!(recorder.channels().is_empty());
        assert_eq!(outcome, TickOutcome::LostClaimBeforeAccept);
        assert_eq!(before["status"], "claimed");
    }
    pool.close().await;
    pg.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_fence_tick_after_claim_transfer_never_executes_pg() {
    transfer_at(Checkpoint::AfterClaim, false, 47_770_101).await;
}
#[tokio::test(flavor = "current_thread")]
async fn owner_fence_tick_preaccept_transfer_never_executes_pg() {
    transfer_at(Checkpoint::PreAccept, false, 47_770_102).await;
}
#[tokio::test(flavor = "current_thread")]
async fn owner_fence_tick_final_done_preserves_stale_row_pg() {
    transfer_at(Checkpoint::FinalDb, false, 47_770_103).await;
}
#[tokio::test(flavor = "current_thread")]
async fn owner_fence_tick_final_failure_preserves_stale_row_pg() {
    transfer_at(Checkpoint::FinalDb, true, 47_770_104).await;
}

#[tokio::test(flavor = "current_thread")]
async fn owner_fence_tick_legacy_and_current_success_failure_restart_hold_pg() {
    let pg = TestPostgresDb::create().await;
    let pool = pg.connect_and_migrate().await;
    let (_registry, shared) =
        crate::services::discord::health::owner_runtime_for_tests::registered("claude").await;
    for stamp in [false, true] {
        for mode in 0..4 {
            let channel = 47_770_200 + u64::from(stamp) * 10 + mode;
            let id = seed(&pool, &channel.to_string(), "pending", stamp).await;
            let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
            let _health = test_health::Clear::new(&gate);
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let close = Arc::new(Mutex::new(None::<Closing>));
            let (cancel, closed) = (cancelled.clone(), close.clone());
            let _hook = test_executor::hook(Box::new(move |point| {
                if point == Checkpoint::AfterClaim {
                    if mode == 2 {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    if mode == 3 {
                        *closed.lock().unwrap() = Some(gate.close().unwrap());
                    }
                }
                Box::pin(async {})
            }));
            let recorder = test_executor::record();
            if mode == 1 {
                test_executor::fail_execution();
            }
            let result =
                run_intake_worker_tick(&pool, &shared, "fence-worker", "claude", "token", &|| {
                    cancelled.load(Ordering::SeqCst)
                })
                .await
                .unwrap();
            let row = snapshot(&pool, id).await;
            let (expected, count) = match mode {
                0 => ("done", 1),
                1 => ("failed_post_accept", 1),
                _ => ("pending", 0),
            };
            assert_eq!(row["status"], expected);
            assert_eq!(recorder.channels().len(), count);
            assert_eq!(
                result,
                match mode {
                    2 => TickOutcome::Cancelled,
                    3 => TickOutcome::Held,
                    _ => TickOutcome::Processed,
                }
            );
            if mode >= 2 {
                assert_eq!(row["claim_owner"], Value::Null);
                assert_eq!(row["claimed_at"], Value::Null);
                assert_eq!(row["accepted_at"], Value::Null);
                assert_eq!(row["spawned_at"], Value::Null);
                assert_eq!(row["last_error"], Value::Null);
                assert_eq!(row["retry_count"], 0);
            }
            if !stamp {
                assert_eq!(row["owner_instance_id"], Value::Null);
                assert_eq!(row["owner_generation"], Value::Null);
                let mut comparable = row.clone();
                for column in [
                    "created_at",
                    "updated_at",
                    "claimed_at",
                    "accepted_at",
                    "spawned_at",
                    "completed_at",
                ] {
                    comparable[column] = Value::Bool(!row[column].is_null());
                }
                eprintln!("legacy observation mode={mode}: {comparable}");
            }
            sqlx::query("UPDATE intake_outbox SET target_instance_id='parked' WHERE id=$1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
    }
    pool.close().await;
    pg.drop().await;
}
