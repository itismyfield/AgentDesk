use super::*;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, ChannelMailboxRegistry};

async fn accepted_fixture() -> (
    TestPostgresDb,
    sqlx::PgPool,
    RoutineStore,
    RoutineAgentExecutor,
    RunningAgentRoutineRun,
    ChannelMailboxHandle,
) {
    let (handle, _) = owner().await;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO agents(id,name,provider,discord_channel_cc) VALUES('primary','Primary','claude','7272')")
        .execute(&pool).await.unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 262144);
    store
        .attach_routine(NewRoutine {
            agent_id: Some("primary".into()),
            fallback_agent_id: None,
            max_retries: Some(1),
            script_ref: "review.js".into(),
            name: "review".into(),
            status: None,
            execution_strategy: "persistent".into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::minutes(1)),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: Some(60),
        })
        .await
        .unwrap();
    let claimed = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let executor = RoutineAgentExecutor::new(Arc::new(pool.clone()), None, 60);
    OWNER
        .scope(
            handle.clone(),
            STARTS.scope(RefCell::new(VecDeque::from([true])), async {
                let result = executor
                    .start_agent_run(
                        &store,
                        claimed,
                        "review work".into(),
                        None,
                        None,
                        None,
                        None,
                        false,
                    )
                    .await
                    .unwrap();
                assert_eq!(result.status, "running");
            }),
        )
        .await;
    let run = store
        .list_running_agent_runs(10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    (db, pool, store, executor, run, handle)
}

#[tokio::test]
async fn owner_review_p2_bad_owner_does_not_block_a_legacy_completion_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, store, executor, run, handle) = accepted_fixture().await;
    sqlx::query("UPDATE routine_runs SET result_json=jsonb_set(result_json,'{owner_input,context,run_id}','\"foreign-run\"'),started_at=NOW()-INTERVAL '2 hours' WHERE id=$1")
        .bind(&run.run_id).execute(&pool).await.unwrap();
    store
        .attach_routine(NewRoutine {
            agent_id: Some("primary".into()),
            fallback_agent_id: None,
            max_retries: Some(0),
            script_ref: "legacy.js".into(),
            name: "legacy review".into(),
            status: None,
            execution_strategy: "persistent".into(),
            schedule: None,
            next_due_at: Some(Utc::now() - Duration::minutes(1)),
            checkpoint: None,
            discord_thread_id: None,
            timeout_secs: Some(60),
        })
        .await
        .unwrap();
    let legacy = store.claim_due_runs(1).await.unwrap().pop().unwrap();
    let turn = "review-legacy-turn";
    assert!(
        store
            .mark_agent_turn_started(
                &legacy.run_id,
                turn,
                Some(json!({"status":"started","execution_mode":"process","turn_id":turn})),
                "primary",
                "primary"
            )
            .await
            .unwrap()
    );
    sqlx::query(
        "INSERT INTO session_transcripts(turn_id,assistant_message) VALUES($1,'legacy answer')",
    )
    .bind(turn)
    .execute(&pool)
    .await
    .unwrap();
    let outcomes = OWNER
        .scope(handle, executor.poll_agent_runs(&store, 10, false))
        .await
        .unwrap();
    assert_eq!(
        outcomes.len(),
        1,
        "one malformed owner must not abort the batch"
    );
    assert_eq!(outcomes[0].run_id, legacy.run_id);
    assert_eq!(outcomes[0].status, "succeeded");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn owner_review_p2_completion_before_bind_is_preserved_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, store, executor, run, handle) = accepted_fixture().await;
    let turn = run.turn_id.as_deref().unwrap();
    sqlx::query("INSERT INTO session_transcripts(turn_id,assistant_message) VALUES($1,'fast answer before bind')").bind(turn).execute(&pool).await.unwrap();
    handle.v2_arm_head_for_tests("fast-attempt").await.unwrap();
    assert!(
        OWNER
            .scope(handle.clone(), executor.poll_agent_runs(&store, 10, false))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        owner_input::custody::bind_observer(&store, &run, &handle, "fast-attempt", None)
            .await
            .unwrap()
    );
    let outcomes = OWNER
        .scope(handle, executor.poll_agent_runs(&store, 10, false))
        .await
        .unwrap();
    assert_eq!(
        outcomes.len(),
        1,
        "intent < completion < bind must remain observable"
    );
    assert_eq!(outcomes[0].status, "succeeded");
    let attempts: Value = sqlx::query_scalar("SELECT attempts FROM routine_runs WHERE id=$1")
        .bind(&run.run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    for event in attempts
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| matches!(e["event"].as_str(), Some("input_pending" | "started")))
    {
        assert_eq!(event["kind"], "primary");
        assert_eq!(event["agent_id"], "primary");
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn owner_review_p2_armed_timeout_waits_for_settlement_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, store, executor, run, handle) = accepted_fixture().await;
    handle.v2_arm_head_for_tests("slow-attempt").await.unwrap();
    assert!(
        owner_input::custody::bind_observer(&store, &run, &handle, "slow-attempt", None)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE routine_runs SET started_at=NOW()-INTERVAL '2 hours',attempts=jsonb_set(attempts,ARRAY[(jsonb_array_length(attempts)-1)::text,'at'],$2::jsonb) WHERE id=$1")
        .bind(&run.run_id).bind(json!((Utc::now()-Duration::hours(2)).to_rfc3339())).execute(&pool).await.unwrap();
    let outcomes = OWNER
        .scope(handle.clone(), executor.poll_agent_runs(&store, 10, false))
        .await
        .unwrap();
    assert!(
        outcomes.is_empty(),
        "an armed attempt cannot authorize a retry while it still owns the input"
    );
    let count: i32 = sqlx::query_scalar("SELECT retry_count FROM routine_runs WHERE id=$1")
        .bind(&run.run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner missing")?;
            let key = owner.active_attempt()?;
            owner.settle(&key, true, 1000)
        })
        .await
        .unwrap();
    let outcomes = OWNER
        .scope(handle, executor.poll_agent_runs(&store, 10, false))
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    let count: i32 = sqlx::query_scalar("SELECT retry_count FROM routine_runs WHERE id=$1")
        .bind(&run.run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "a settled attempt may enter the existing retry policy"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn owner_review_p2_accepted_missing_owner_never_readmits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, store, executor, run, _old_handle) = accepted_fixture().await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(poise::serenity_prelude::ChannelId::new(7272));
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("empty-owner.json");
    handle
        .v2_adopt(path.clone(), "routine-tests".into())
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(
        OWNER
            .scope(handle, executor.poll_agent_runs(&store, 10, false))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        before,
        std::fs::read(&path).unwrap(),
        "a known accepted receipt cannot be recreated from an absent owner"
    );
    let state: (String, i32) =
        sqlx::query_as("SELECT status,retry_count FROM routine_runs WHERE id=$1")
            .bind(&run.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, ("running".into(), 0));
    pool.close().await;
    db.drop().await;
}

/// The poll's observer binds the owner's armed attempt itself before it reads completion, so no
/// caller supplies the bind: the first poll only binds, the next one completes.
#[tokio::test]
async fn owner_observer_binds_the_armed_attempt_on_its_own_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, store, executor, run, handle) = accepted_fixture().await;
    let turn = run.turn_id.as_deref().unwrap();
    handle.v2_arm_head_for_tests("own-attempt").await.unwrap();
    sqlx::query(
        "INSERT INTO session_transcripts(turn_id,assistant_message) VALUES($1,'owned answer')",
    )
    .bind(turn)
    .execute(&pool)
    .await
    .unwrap();
    let first = OWNER
        .scope(handle.clone(), executor.poll_agent_runs(&store, 10, false))
        .await
        .unwrap();
    assert!(first.is_empty(), "the first poll only binds");
    let result: Value = sqlx::query_scalar("SELECT result_json FROM routine_runs WHERE id=$1")
        .bind(&run.run_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        result["owner_input"]["bind"]["attempt"], "own-attempt",
        "{result}"
    );
    let outcomes = OWNER
        .scope(handle, executor.poll_agent_runs(&store, 10, false))
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, "succeeded");
    pool.close().await;
    db.drop().await;
}

/// The retry as the owner runs it: the head reserved as `nonce` with its own frame and its leaf
/// run in the owner scope past the payload start; the attempt and the frames the leaf was given.
#[cfg(unix)]
async fn retry_leaf(
    handle: &ChannelMailboxHandle,
    nonce: &str,
) -> (serde_json::Value, Vec<String>) {
    let attempt = serde_json::json!({"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":"bootstrap-pane","root":"/test","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let frame = format!("frame-{nonce}");
    let reserved = handle.v2_request(move |slot| {
        let payload = serde_json::json!({"frame":frame,"digest":"digest","attribution":{}});
        let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
        let payload = serde_json::from_value(payload).map_err(|e| e.to_string())?;
        slot.as_mut()
            .ok_or("no owner")?
            .reserve(attempt, payload)
            .map(drop)
    });
    reserved.await.unwrap();
    let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = frames.clone();
    let effect = move |frame: String| {
        crate::services::claude_tui::owner_input::start_payload()?;
        crate::services::claude_tui::owner_input::keyed();
        seen.lock().unwrap().push(frame);
        Ok::<(), String>(())
    };
    let cold = handle.v2_cold_observed(1, std::sync::Arc::new(|| 1), effect);
    let (identity, outcome) = cold.await.unwrap();
    assert_eq!(outcome.result, Ok(()));
    assert!(outcome.not_sent.is_none());
    let frames = frames.lock().unwrap().clone();
    (identity, frames)
}

/// Reserves the head as `nonce` and arms it in its bootstrap window, runs `poll` there, then a
/// modal veto ends it unsent and holds the original for its retry.
#[cfg(unix)]
async fn bootstrap_held_by_modal<T>(
    handle: &ChannelMailboxHandle,
    nonce: &str,
    poll: impl std::future::Future<Output = T>,
) -> T {
    let attempt = serde_json::json!({"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":"bootstrap-pane","root":"/test","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let reserved = handle.v2_request(move |slot| {
        let payload = serde_json::json!({"frame":"frame","digest":"digest","attribution":{}});
        let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
        let payload = serde_json::from_value(payload).map_err(|e| e.to_string())?;
        let owner = slot.as_mut().ok_or("no owner")?;
        owner.reserve(attempt, payload).map(drop)
    });
    reserved.await.unwrap();
    let (armed, in_window) = tokio::sync::oneshot::channel();
    let (polled, release) = std::sync::mpsc::channel::<()>();
    let owner = handle.clone();
    let cold = tokio::spawn(async move {
        let effect = move |_| {
            let _ = armed.send(());
            let _ = release.recv();
            crate::services::claude_tui::owner_input::vetoed("Modal");
            Ok::<(), String>(())
        };
        owner
            .v2_cold_observed(0, std::sync::Arc::new(|| 0), effect)
            .await
    });
    if in_window.await.is_err() {
        panic!("cold: {:?}", cold.await.unwrap().map(|r| r.0));
    }
    let during = poll.await;
    polled.send(()).unwrap();
    let (identity, outcome) = cold.await.unwrap().unwrap();
    let proof = outcome
        .not_sent
        .expect("a modal veto proves nothing was sent");
    let held = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let key = serde_json::from_value(identity).map_err(|e| e.to_string())?;
        owner.hold_not_sent(&key, proof)
    });
    held.await.unwrap();
    during
}

/// An attempt still in its bootstrap window is no observer's: the poll there binds nothing, a modal
/// ends it unsent, and the retry of the same admission is the attempt bound and completed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_observer_skips_a_bootstrapping_attempt_and_completes_its_retry_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, store, executor, run, handle) = accepted_fixture().await;
    let turn = run.turn_id.as_deref().unwrap();
    let poll = || OWNER.scope(handle.clone(), executor.poll_agent_runs(&store, 10, false));
    let during = bootstrap_held_by_modal(&handle, "bootstrap-a", poll()).await;
    assert!(during.unwrap().is_empty());
    let bind = || async {
        let result: Value = sqlx::query_scalar("SELECT result_json FROM routine_runs WHERE id=$1")
            .bind(&run.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        result["owner_input"]["bind"].clone()
    };
    assert_eq!(
        bind().await,
        Value::Null,
        "nothing bound in the bootstrap window"
    );
    // A completion recorded while only the unsent A was attempted completes nothing.
    sqlx::query("INSERT INTO session_transcripts(turn_id,assistant_message) VALUES($1,'retried')")
        .bind(turn)
        .execute(&pool)
        .await
        .unwrap();
    assert!(poll().await.unwrap().is_empty(), "A's completion is stale");
    assert_eq!(bind().await, Value::Null, "still unbound before B");
    let (identity, frames) = retry_leaf(&handle, "retry-b").await;
    assert_eq!(frames, ["frame-retry-b"], "B's own payload, once");
    let settled = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let key = serde_json::from_value(identity).map_err(|e| e.to_string())?;
        owner.settle(&key, true, 1)?;
        Ok((
            owner.snapshot.admission_receipts.len(),
            owner.snapshot.terminal_identities.len(),
        ))
    });
    assert_eq!(
        settled.await.unwrap(),
        (1, 1),
        "one admission, delivered once"
    );
    assert!(
        poll().await.unwrap().is_empty(),
        "the first poll only binds"
    );
    assert_eq!(bind().await["attempt"], "retry-b");
    let outcomes = poll().await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].status, "succeeded");
    pool.close().await;
    db.drop().await;
}
