use super::*;

#[tokio::test]
async fn held_dispatch_claim_autoqueue_history_outbox_and_atomic_rollback_pg() {
    let (fixture, pool) = setup().await;
    for state in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ] {
        let channel = format!("aq-{state}");
        let card = format!("card-{state}");
        let dispatch_id = format!("dispatch-{state}");
        let run = format!("run-{state}");
        let entry = format!("entry-{state}");
        let receipt = receipt_on(
            &pool,
            &channel,
            "claude",
            &["source"],
            "registered_not_started",
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO kanban_cards (id,title) VALUES ($1,'held card')")
            .bind(&card)
            .execute(&pool)
            .await
            .unwrap();
        dispatch(&pool, &dispatch_id, receipt, Some(&card)).await;
        sqlx::query("INSERT INTO auto_queue_runs (id,status) VALUES ($1,'active')")
            .bind(&run)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO auto_queue_entries (id,run_id,kanban_card_id,status,dispatch_id,slot_index)
                     VALUES ($1,$2,$3,'dispatched',$4,2)")
            .bind(&entry).bind(&run).bind(&card).bind(&dispatch_id).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id,dispatch_id) VALUES ($1,$2)",
        )
        .bind(&entry)
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key,channel_id,status,active_dispatch_id,
                     current_replay_receipt_id,claude_session_id,raw_provider_session_id)
                     VALUES ($1,$1,'turn_active',$2,$3,'provider-session','provider-session')",
        )
        .bind(&channel)
        .bind(&dispatch_id)
        .bind(receipt)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id,action,status) VALUES ($1,'notify','failed')",
        )
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1",
        )
        .bind(receipt)
        .execute(&pool)
        .await
        .unwrap();
        if state != "started_unclassified" {
            sqlx::query("UPDATE intake_outbox SET replay_disposition=$2 WHERE id=$1")
                .bind(receipt)
                .bind(state)
                .execute(&pool)
                .await
                .unwrap();
        }
        let unchanged = crate::db::auto_queue::record_entry_dispatch_failure_on_pg(
            &pool,
            &entry,
            3,
            "held-policy",
        )
        .await
        .expect("held reducer returns a no-op");
        assert!(
            !unchanged.changed,
            "AutoQueue must explicitly consume {state}"
        );
        assert_eq!(unchanged.to_status, "dispatched");
        assert_eq!(unchanged.retry_count, 0);
        for sql in [
            "UPDATE task_dispatches SET claim_owner='replacement-node',claim_expires_at=NOW()+INTERVAL '1 hour' WHERE id=$1",
            "UPDATE task_dispatches SET status='pending' WHERE id=$1",
            "UPDATE task_dispatches SET replay_receipt_id=NULL WHERE id=$1",
            "UPDATE task_dispatches SET result=NULL WHERE id=$1",
            "DELETE FROM task_dispatches WHERE id=$1",
        ] {
            assert_fenced(
                sqlx::query(sql).bind(&dispatch_id).execute(&pool).await,
                sql,
            );
        }
        assert!(
            crate::db::dispatches::outbox::dispatch_notify_delivery_suppressed_pg(
                &pool,
                &dispatch_id,
                "notify",
            )
            .await
            .unwrap()
            .is_some()
        );
        assert_fenced(sqlx::query("UPDATE dispatch_outbox SET status='pending',retry_count=0 WHERE dispatch_id=$1 AND action='notify'")
            .bind(&dispatch_id).execute(&pool).await, "notify retry does not reexecute held provider request");
        assert_fenced(
            sqlx::query("INSERT INTO dispatch_outbox (dispatch_id,action) VALUES ($1,'followup')")
                .bind(&dispatch_id)
                .execute(&pool)
                .await,
            "followup cannot launch held request lineage",
        );
        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id,action) VALUES ($1,'status_reaction')",
        )
        .bind(&dispatch_id)
        .execute(&pool)
        .await
        .expect("status delivery obligation is preserved");
        assert_fenced(sqlx::query("UPDATE dispatch_outbox SET action='followup' WHERE dispatch_id=$1 AND action='status_reaction'")
            .bind(&dispatch_id).execute(&pool).await, "pending delivery action cannot become a provider-launching action");
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("UPDATE auto_queue_runs SET status='completed' WHERE id=$1")
            .bind(&run)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_fenced(sqlx::query("UPDATE auto_queue_entries SET status='pending',dispatch_id=NULL,slot_index=NULL,retry_count=1 WHERE id=$1")
            .bind(&entry).execute(&mut *tx).await, "old writer entry requeue aborts whole transaction");
        let aborted = sqlx::query("SELECT 1")
            .execute(&mut *tx)
            .await
            .expect_err("a fence exception must abort the transaction");
        assert_eq!(
            aborted
                .as_database_error()
                .and_then(|e| e.code())
                .as_deref(),
            Some("25P02")
        );
        // PostgreSQL converts COMMIT of an aborted transaction into ROLLBACK.
        let _ = tx.commit().await;
        let invariant: (String, String, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT r.status,e.status,e.dispatch_id,e.slot_index FROM auto_queue_runs r
             JOIN auto_queue_entries e ON e.run_id=r.id WHERE e.id=$1",
        )
        .bind(&entry)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            invariant,
            (
                "active".into(),
                "dispatched".into(),
                Some(dispatch_id.clone()),
                Some(2)
            )
        );
        assert_fenced(
            sqlx::query("DELETE FROM auto_queue_entry_dispatch_history WHERE entry_id=$1")
                .bind(&entry)
                .execute(&pool)
                .await,
            "history cannot erase held lineage",
        );
        assert_fenced(sqlx::query("INSERT INTO task_dispatches (id,parent_dispatch_id,status) VALUES ($1,$2,'pending')")
            .bind(format!("child-{state}")).bind(&dispatch_id).execute(&pool).await, "retry child preserves held origin");
        assert_fenced(
            sqlx::query(
                "INSERT INTO task_dispatches (id,kanban_card_id,status) VALUES ($1,$2,'pending')",
            )
            .bind(format!("replacement-{state}"))
            .bind(&card)
            .execute(&pool)
            .await,
            "AutoQueue replacement dispatch cannot bypass history",
        );
        assert_fenced(sqlx::query("UPDATE sessions SET status='idle',active_dispatch_id=NULL,claude_session_id=NULL WHERE session_key=$1")
            .bind(&channel).execute(&pool).await, "restore/review session clear preserves held provider selector");
        sqlx::query(
            "UPDATE sessions SET last_heartbeat=NOW(),status='awaiting_user' WHERE session_key=$1",
        )
        .bind(&channel)
        .execute(&pool)
        .await
        .expect("held session may await new input while delivery remains active");
        let fresh_source = format!("fresh-{state}");
        let fresh = receipt_on(
            &pool,
            &channel,
            "claude",
            &[&fresh_source],
            "registered_not_started",
        )
        .await
        .expect("separate explicit input prepares its own request identity");
        assert_fenced(sqlx::query("UPDATE sessions SET current_replay_receipt_id=$2,replay_episode_nonce='episode-1' WHERE session_key=$1")
            .bind(&channel).bind(fresh).execute(&pool).await,
            "fresh receipt cannot leave the old held dispatch attached");
        sqlx::query("UPDATE sessions SET current_replay_receipt_id=$2,replay_episode_nonce='episode-1',active_dispatch_id=NULL WHERE session_key=$1")
            .bind(&channel).bind(fresh).execute(&pool).await.expect("fresh disjoint input changes attribution while preserving provider selector");
        assert_eq!(
            receipt_disposition(&pool, receipt)
                .await
                .unwrap()
                .as_deref(),
            Some(state)
        );
    }
    finish(fixture, pool).await;
}

#[tokio::test]
async fn legacy_null_link_cannot_requeue_via_held_history_pg() {
    let (fixture, pool) = setup().await;
    let receipt = receipt_on(
        &pool,
        "null-link",
        "claude",
        &["source"],
        "registered_not_started",
    )
    .await
    .unwrap();
    dispatch(&pool, "history-dispatch", receipt, None).await;
    sqlx::query("INSERT INTO auto_queue_runs (id,status) VALUES ('history-run','active')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO auto_queue_entries (id,run_id,status) VALUES ('history-entry','history-run','skipped')")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO auto_queue_entry_dispatch_history (entry_id,dispatch_id) VALUES ('history-entry','history-dispatch')")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1")
        .bind(receipt)
        .execute(&pool)
        .await
        .unwrap();
    assert_fenced(
        sqlx::query("UPDATE auto_queue_entries SET status='pending' WHERE id='history-entry'")
            .execute(&pool)
            .await,
        "preexisting null current link does not erase held history",
    );
    sqlx::query("INSERT INTO task_dispatches (id,status) VALUES ('unrelated','pending')")
        .execute(&pool)
        .await
        .unwrap();
    assert_fenced(sqlx::query("INSERT INTO auto_queue_entry_dispatch_history (entry_id,dispatch_id) VALUES ('history-entry','unrelated')")
        .execute(&pool).await, "history cannot attach a new retry dispatch");
    finish(fixture, pool).await;
}

#[tokio::test]
async fn held_dispatch_nonnull_result_rewrite_is_fenced_pg() {
    let (fixture, pool) = setup().await;
    let receipt = receipt_on(&pool, "result-rewrite", "claude", &["source"], "withheld")
        .await
        .unwrap();
    dispatch(&pool, "result-rewrite-dispatch", receipt, None).await;
    assert_fenced(
        sqlx::query("UPDATE task_dispatches SET result='replacement nonnull result' WHERE id='result-rewrite-dispatch'")
            .execute(&pool)
            .await,
        "a same-status old writer cannot replace an existing held result with another nonnull value",
    );
    let retained: String =
        sqlx::query_scalar("SELECT result FROM task_dispatches WHERE id='result-rewrite-dispatch'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, "preserved output");
    sqlx::query("UPDATE task_dispatches SET result=result || ' appended capture' WHERE id='result-rewrite-dispatch'")
        .execute(&pool).await.expect("held result may append captured output without replacing its prefix");
    let progressed: String =
        sqlx::query_scalar("SELECT result FROM task_dispatches WHERE id='result-rewrite-dispatch'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        progressed.strip_prefix(&retained),
        Some(" appended capture")
    );
    assert_eq!(
        receipt_disposition(&pool, receipt)
            .await
            .unwrap()
            .as_deref(),
        Some("withheld")
    );
    finish(fixture, pool).await;
}

#[tokio::test]
async fn held_session_canonical_identity_rewrites_are_fenced_pg() {
    let (fixture, pool) = setup().await;
    let receipt = receipt_on(&pool, "identity-rewrite", "claude", &["source"], "withheld")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions (session_key,channel_id,provider,status,current_replay_receipt_id,
            claude_session_id,raw_provider_session_id,identity_kind,discord_token_hash)
         VALUES ('identity-rewrite-session','identity-rewrite','claude','awaiting_user',$1,
            'preserved-selector','preserved-selector','discord_channel','discord_0123456789abcdef')",
    )
    .bind(receipt)
    .execute(&pool)
    .await
    .unwrap();
    for sql in [
        "UPDATE sessions SET provider='codex' WHERE session_key='identity-rewrite-session'",
        "UPDATE sessions SET channel_id='other-channel' WHERE session_key='identity-rewrite-session'",
        "UPDATE sessions SET identity_kind='scheduled_snapshot' WHERE session_key='identity-rewrite-session'",
        "UPDATE sessions SET discord_token_hash='discord_fedcba9876543210' WHERE session_key='identity-rewrite-session'",
    ] {
        assert_fenced(sqlx::query(sql).execute(&pool).await, sql);
    }
    let retained: (String, String, String, String) = sqlx::query_as(
        "SELECT provider,channel_id,identity_kind,discord_token_hash FROM sessions
         WHERE session_key='identity-rewrite-session'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        retained,
        (
            "claude".into(),
            "identity-rewrite".into(),
            "discord_channel".into(),
            "discord_0123456789abcdef".into(),
        )
    );
    finish(fixture, pool).await;
}

async fn normal_retry_parent(pool: &PgPool, channel: &str) -> (i64, i64) {
    let authority = receipt_on(
        pool,
        channel,
        "claude",
        &["earlier"],
        "registered_not_started",
    )
    .await
    .unwrap();
    let parent = intake_outbox::insert_pending(pool, &payload(channel, "last"), 1, None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE intake_outbox SET status='failed_pre_accept',admission_kind='local',
            replay_disposition='registered_not_started',
            replay_source_message_ids=ARRAY['earlier','last'] WHERE id=$1",
    )
    .bind(parent)
    .execute(pool)
    .await
    .unwrap();
    (parent, authority)
}

#[tokio::test]
async fn normal_retry_parent_absorbed_sources_survive_sweep_and_old_writer_child_pg() {
    let (fixture, pool) = setup().await;
    sqlx::query("INSERT INTO worker_nodes (instance_id,status,labels,capabilities,last_heartbeat_at)
                 VALUES ('worker-a','online','[]','{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"]}}',NOW())")
        .execute(&pool).await.unwrap();

    // A normal parent was registered before an earlier absorbed source became held elsewhere.
    let (parent, authority) = normal_retry_parent(&pool, "parent-already-held").await;
    sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld' WHERE id=$1")
        .bind(authority)
        .execute(&pool)
        .await
        .unwrap();
    let outcome = intake_outbox::sweep_failed_pre_accept_once(&pool, "leader", 3, 60, None)
        .await
        .unwrap();
    assert!(
        !matches!(outcome, FailedPreAcceptSweepOutcome::Retried { .. }),
        "sweep cannot retry a normal parent whose nonrepresentative absorbed source is held: {outcome:?}",
    );
    assert_fenced(
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO intake_outbox (target_instance_id,forwarded_by_instance_id,
                channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,
                provider,status,attempt_no,parent_outbox_id)
             SELECT target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
                request_owner_id,user_text,turn_kind,agent_id,provider,'pending',2,id
             FROM intake_outbox WHERE id=$1 RETURNING id",
        )
        .bind(parent)
        .fetch_one(&pool)
        .await,
        "an old writer's child INSERT must consume the normal parent's complete source family",
    );

    // A legitimate child prepared before the hold must keep the earlier source identity too.
    let (parent, authority) = normal_retry_parent(&pool, "child-before-hold").await;
    let outcome = intake_outbox::sweep_failed_pre_accept_once(&pool, "leader", 3, 60, None)
        .await
        .unwrap();
    let FailedPreAcceptSweepOutcome::Retried {
        source_id, new_id, ..
    } = outcome
    else {
        panic!("unstarted normal parent remains retryable before the hold: {outcome:?}");
    };
    assert_eq!(source_id, parent);
    let sources: Option<Vec<String>> =
        sqlx::query_scalar("SELECT replay_source_message_ids FROM intake_outbox WHERE id=$1")
            .bind(new_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sources, Some(vec!["earlier".into(), "last".into()]));
    for sql in [
        "UPDATE intake_outbox SET replay_source_message_ids=NULL WHERE id=$1",
        "UPDATE intake_outbox SET replay_source_message_ids=ARRAY['last'] WHERE id=$1",
        "UPDATE intake_outbox SET provider='codex' WHERE id=$1",
        "UPDATE intake_outbox SET channel_id='rewritten-child-channel' WHERE id=$1",
        "UPDATE intake_outbox SET user_text='replacement request' WHERE id=$1",
    ] {
        assert_fenced(sqlx::query(sql).bind(new_id).execute(&pool).await, sql);
    }
    let original_tuple: (String, String, String, String) = sqlx::query_as(
        "SELECT provider,channel_id,user_msg_id,user_text FROM intake_outbox WHERE id=$1",
    )
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        original_tuple,
        (
            "claude".into(),
            "child-before-hold".into(),
            "last".into(),
            "original request".into()
        )
    );
    sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld' WHERE id=$1")
        .bind(authority)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        intake_outbox::claim_pending_for_target(&pool, "worker-a", "claude", "replacement-node")
            .await
            .unwrap()
            .is_none(),
        "the already-created child cannot shed its earlier held source on another worker",
    );
    finish(fixture, pool).await;
}
