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
