use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};
use crate::services::discord::input_runtime::fence::{Closing, Gate, effect, test_health};
use crate::services::provider::ProviderKind;
use futures::FutureExt;
use test_executor::Checkpoint;

async fn seed(pool: &PgPool, channel: u64) -> i64 {
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('c1-worker', 'Test', 'claude', 'unused')")
        .execute(pool).await.unwrap();
    insert_pending(
        pool,
        &InsertPendingPayload {
            target_instance_id: "c1-worker".into(),
            forwarded_by_instance_id: "c1-leader".into(),
            required_labels: serde_json::json!([]),
            execution_requirements: serde_json::json!({}),
            attachment_refs: serde_json::json!([]),
            channel_id: channel.to_string(),
            user_msg_id: channel.to_string(),
            request_owner_id: "100".into(),
            request_owner_name: Some("Tester".into()),
            user_text: "C1 effect".into(),
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: "standard".into(),
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            preserve_on_cancel: false,
            agent_id: "c1-worker".into(),
            provider: "claude".into(),
            home_epoch: None,
        },
        1,
        None,
    )
    .await
    .unwrap()
}
async fn row_state(pool: &PgPool, row: i64) -> (String, Option<String>, bool, bool) {
    sqlx::query_as("SELECT status::TEXT, claim_owner, accepted_at IS NULL, spawned_at IS NULL FROM intake_outbox WHERE id=$1")
        .bind(row).fetch_one(pool).await.unwrap()
}

async fn refused_at(channel: u64, point: Option<Checkpoint>) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let row = seed(&pool, channel).await;
    let (_registry, shared) =
        crate::services::discord::health::owner_runtime_for_tests::registered("claude").await;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    let closed = Arc::new(std::sync::Mutex::new(None::<Closing>));
    let close_slot = closed.clone();
    let close_gate = gate.clone();
    let _hook = test_executor::hook(Box::new(move |seen| {
        if Some(seen) == point {
            if seen == Checkpoint::PreAccept {
                assert!(
                    effect::current().is_some(),
                    "already admitted before accept"
                );
            }
            *close_slot.lock().unwrap() = Some(close_gate.close().unwrap());
        }
        Box::pin(async {})
    }));
    let recorder = test_executor::record();
    if point.is_none() {
        *closed.lock().unwrap() = Some(gate.close().unwrap());
    }
    let outcome =
        run_intake_worker_tick(&pool, &shared, "c1-worker", "claude", "c1-owner", &|| false)
            .await
            .unwrap();
    assert_eq!(
        outcome,
        if point.is_none() {
            TickOutcome::QueueEmpty
        } else {
            TickOutcome::Held
        }
    );
    assert_eq!(
        row_state(&pool, row).await,
        ("pending".into(), None, true, true)
    );
    assert!(recorder.channels().is_empty(), "refusal never executes");
    closed
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .drain()
        .now_or_never()
        .expect("claim cleanup drained");
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn c1_worker_beforeclaim_excludes_closing_population_pg() {
    refused_at(6_325_421, None).await;
}
#[tokio::test(flavor = "current_thread")]
async fn c1_worker_afterclaim_closing_returns_only_own_claim_pg() {
    refused_at(6_325_422, Some(Checkpoint::AfterClaim)).await;
}
#[tokio::test(flavor = "current_thread")]
async fn c1_worker_preaccept_closing_releases_admitted_claim_pg() {
    refused_at(6_325_423, Some(Checkpoint::PreAccept)).await;
}

#[derive(Clone, Copy)]
enum Finish {
    Done,
    Failed,
    Miss,
    DbError,
}
async fn final_db(channel: u64, finish: Finish) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let row = seed(&pool, channel).await;
    let (_registry, shared) =
        crate::services::discord::health::owner_runtime_for_tests::registered("claude").await;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = test_health::Clear::new(&gate);
    let recorder = test_executor::record();
    if matches!(finish, Finish::Failed) {
        test_executor::fail_execution();
    }
    if matches!(finish, Finish::DbError) {
        sqlx::query("CREATE FUNCTION refuse_c1_done() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'C1 final DB refusal'; END $$ LANGUAGE plpgsql")
            .execute(&pool).await.unwrap();
        sqlx::query("CREATE TRIGGER refuse_c1_done BEFORE UPDATE ON intake_outbox FOR EACH ROW WHEN (NEW.status='done') EXECUTE FUNCTION refuse_c1_done()")
            .execute(&pool).await.unwrap();
    }
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let mut entered = Some((entered_tx, release_rx));
    let mut done = Some((done_tx, exit_rx));
    let _hook = test_executor::hook(Box::new(move |seen| {
        let pair = match seen {
            Checkpoint::FinalDb => entered.take(),
            Checkpoint::FinalDbDone => done.take(),
            _ => None,
        };
        Box::pin(async move {
            if let Some((notify, release)) = pair {
                assert!(
                    effect::current().is_some(),
                    "DB disposition retains original effect"
                );
                notify.send(()).unwrap();
                release.await.unwrap();
                assert!(
                    effect::current().is_some(),
                    "effect survives DB checkpoint await"
                );
            }
        })
    }));
    let tick = run_intake_worker_tick(&pool, &shared, "c1-worker", "claude", "c1-owner", &|| false);
    let observe = async {
        entered_rx.await.unwrap();
        assert_eq!(row_state(&pool, row).await.0, "spawned");
        let closing = gate.close().unwrap();
        let mut lock = pool.begin().await.unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(row)
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        if matches!(finish, Finish::Miss) {
            sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
                .bind(row)
                .execute(&mut *lock)
                .await
                .unwrap();
        }
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))")
                    .bind(blocker).fetch_one(&pool).await.unwrap();
                if blocked { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("real final DB query blocked on our row lock");
        assert!(
            closing.drain().now_or_never().is_none(),
            "DB wait retains effect"
        );
        lock.commit().await.unwrap();
        if !matches!(finish, Finish::DbError) {
            done_rx.await.unwrap();
            assert_eq!(
                row_state(&pool, row).await.0,
                if matches!(finish, Finish::Failed) {
                    "failed_post_accept"
                } else {
                    "done"
                }
            );
            assert!(
                closing.drain().now_or_never().is_none(),
                "final disposition still owns effect"
            );
            exit_tx.send(()).unwrap();
        }
        closing.drain().await;
    };
    let (outcome, ()) = tokio::join!(tick, observe);
    if matches!(finish, Finish::DbError) {
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("C1 final DB refusal")
        );
        assert_eq!(row_state(&pool, row).await.0, "spawned");
    } else {
        assert_eq!(outcome.unwrap(), TickOutcome::Processed);
    }
    assert_eq!(recorder.channels(), [channel]);
    pool.close().await;
    fixture.drop().await;
}
#[tokio::test(flavor = "current_thread")]
async fn c1_worker_final_done_waits_for_real_db_disposition_pg() {
    final_db(6_325_424, Finish::Done).await;
}
#[tokio::test(flavor = "current_thread")]
async fn c1_worker_final_failure_waits_for_real_db_disposition_pg() {
    final_db(6_325_425, Finish::Failed).await;
}
#[tokio::test(flavor = "current_thread")]
async fn c1_worker_final_cas_miss_retains_effect_through_classification_pg() {
    final_db(6_325_426, Finish::Miss).await;
}
#[tokio::test(flavor = "current_thread")]
async fn c1_worker_final_db_error_releases_effect_after_error_pg() {
    final_db(6_325_427, Finish::DbError).await;
}
