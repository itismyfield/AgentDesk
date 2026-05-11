use sqlx::{Postgres, Row};

pub(crate) async fn execute_pg_transition_intent(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    intent: &crate::engine::transition::TransitionIntent,
) -> Result<(), String> {
    match intent {
        crate::engine::transition::TransitionIntent::UpdateStatus { card_id, to, .. } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET status = $1, updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(to)
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("update status for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::SetLatestDispatchId {
            card_id,
            dispatch_id,
        } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET latest_dispatch_id = $1, updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(dispatch_id.as_deref())
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("set latest_dispatch_id for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::SetReviewStatus {
            card_id,
            review_status,
        } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET review_status = $1, updated_at = NOW()
                 WHERE id = $2",
            )
            .bind(review_status.as_deref())
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("set review_status for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::ApplyClock { card_id, clock, .. } => {
            if let Some(clock) = clock {
                let sql = if clock.mode.as_deref() == Some("coalesce") {
                    format!(
                        "UPDATE kanban_cards
                         SET {field} = COALESCE({field}, NOW()), updated_at = NOW()
                         WHERE id = $1",
                        field = clock.set
                    )
                } else {
                    format!(
                        "UPDATE kanban_cards
                         SET {field} = NOW(), updated_at = NOW()
                         WHERE id = $1",
                        field = clock.set
                    )
                };
                sqlx::query(&sql)
                    .bind(card_id)
                    .execute(&mut **tx)
                    .await
                    .map_err(|error| format!("apply clock {} for {card_id}: {error}", clock.set))?;
            }
        }
        crate::engine::transition::TransitionIntent::ClearTerminalFields { card_id } => {
            sqlx::query(
                "UPDATE kanban_cards
                 SET review_status = NULL,
                     suggestion_pending_at = NULL,
                     review_entered_at = NULL,
                     awaiting_dod_at = NULL,
                     blocked_reason = NULL,
                     review_round = NULL,
                     deferred_dod_json = NULL,
                     updated_at = NOW()
                 WHERE id = $1",
            )
            .bind(card_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("clear terminal fields for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::SyncAutoQueue { card_id } => {
            crate::github::sync::sync_auto_queue_terminal_on_pg(tx, card_id).await?;
        }
        crate::engine::transition::TransitionIntent::SyncReviewState { card_id, state } => {
            crate::github::sync::sync_review_state_on_pg(tx, card_id, state).await?;
        }
        crate::engine::transition::TransitionIntent::AuditLog {
            card_id,
            from,
            to,
            source,
            message,
        } => {
            sqlx::query(
                "INSERT INTO kanban_audit_logs (
                    card_id, from_status, to_status, source, result
                 )
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(card_id)
            .bind(from)
            .bind(to)
            .bind(source)
            .bind(message)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("insert audit log for {card_id}: {error}"))?;
        }
        crate::engine::transition::TransitionIntent::CancelDispatch { dispatch_id } => {
            // #2045 Finding 5 (P1): delegate cancellation to the canonical
            // helper so the cleanup pipeline (semaphore release, session
            // active_dispatch_id clear, auto_queue_entries reset,
            // dispatch_events insert, status_reaction outbox enqueue, thread
            // link teardown) runs inside this transaction. The previous raw
            // UPDATE only flipped the row status and left the rest of the
            // graph stale, which is the same class of leak fixed for the JS
            // bridge in Finding 3.
            crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
                tx,
                dispatch_id,
                Some("transition_intent_cancel"),
            )
            .await
            .map_err(|error| format!("cancel dispatch {dispatch_id}: {error}"))?;
        }
    }

    Ok(())
}

pub(crate) async fn execute_activate_transition_intent_pg(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    intent: &crate::engine::transition::TransitionIntent,
) -> Result<(), String> {
    execute_pg_transition_intent(tx, intent).await
}

fn review_result_has_verdict(result: Option<&str>) -> bool {
    let Some(raw) = result.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("verdict")
                .or_else(|| value.get("decision"))
                .and_then(|field| field.as_str())
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(str::to_string)
        })
        .is_some()
}

pub(crate) async fn cancel_live_dispatches_for_terminal_card_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> Result<usize, String> {
    let rows = sqlx::query(
        "SELECT id, dispatch_type, result
         FROM task_dispatches
         WHERE kanban_card_id = $1 AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load live dispatches for {card_id}: {error}"))?;

    let mut cancelled = 0usize;
    let mut preserved_review_dispatches = Vec::new();

    for row in rows {
        let dispatch_id: String = row
            .try_get("id")
            .map_err(|error| format!("read live dispatch id for {card_id}: {error}"))?;
        let dispatch_type: Option<String> = row
            .try_get("dispatch_type")
            .map_err(|error| format!("read live dispatch type for {dispatch_id}: {error}"))?;
        let result: Option<String> = row
            .try_get("result")
            .map_err(|error| format!("read live dispatch result for {dispatch_id}: {error}"))?;
        if dispatch_type.as_deref() == Some("review")
            && !review_result_has_verdict(result.as_deref())
        {
            preserved_review_dispatches.push(dispatch_id);
            continue;
        }
        // #2045 Finding 5 (P1): route cancellation through the canonical
        // helper so semaphore release, auto_queue_entries reset,
        // dispatch_events audit, status_reaction outbox enqueue, and thread
        // link teardown all happen inside the same terminal-card transaction.
        // The old raw UPDATE only flipped task_dispatches.status and cleared
        // sessions, leaving the rest of the dispatch graph stale (Finding
        // 3 / 4 class).
        let changed = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            tx,
            &dispatch_id,
            Some("auto_cancelled_on_terminal_card"),
        )
        .await
        .map_err(|error| format!("cancel live dispatch {dispatch_id}: {error}"))?;
        if changed > 0 {
            cancelled += 1;
        }
    }

    if let Some(first_review_dispatch_id) = preserved_review_dispatches.first() {
        let reason = format!(
            "terminal cleanup preserved review dispatch without verdict: {}",
            preserved_review_dispatches.join(",")
        );
        sqlx::query(
            "UPDATE kanban_cards
             SET review_status = 'review_recovery_needed',
                 blocked_reason = $2,
                 latest_dispatch_id = $3,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(card_id)
        .bind(&reason)
        .bind(first_review_dispatch_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("mark review recovery needed for {card_id}: {error}"))?;
    }

    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = NULL,
             updated_at = NOW()
         WHERE id = $1
           AND latest_dispatch_id IS NOT NULL
           AND latest_dispatch_id <> ALL($2::text[])",
    )
    .bind(card_id)
    .bind(&preserved_review_dispatches)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("clear latest_dispatch_id for {card_id}: {error}"))?;

    Ok(cancelled)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::engine::transition::TransitionIntent;
    use crate::pipeline::ClockConfig;
    use sqlx::{PgPool, Row};
    use std::collections::HashMap;

    struct TestDatabase {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    async fn connect_test_pool(database_url: &str, max_connections: u32, context: &str) -> PgPool {
        let mut last_error = None;

        for attempt in 1..=3 {
            match sqlx::postgres::PgPoolOptions::new()
                .max_connections(max_connections)
                .acquire_timeout(std::time::Duration::from_secs(30))
                .connect(database_url)
                .await
            {
                Ok(pool) => return pool,
                Err(error) => {
                    last_error = Some(error);
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }

        panic!(
            "{context} after retries: {}",
            last_error.expect("postgres pool connect should capture final error")
        );
    }

    impl TestDatabase {
        async fn create() -> Self {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = admin_database_url();
            let database_name = format!("agentdesk_pg_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "transition executor pg tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> PgPool {
            let pool = connect_test_pool(&self.database_url, 1, "connect postgres test db").await;
            crate::db::postgres::migrate(&pool)
                .await
                .expect("migrate postgres test db");
            pool
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "transition executor pg tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    fn base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", base_database_url(), admin_db)
    }

    async fn execute_intent(pool: &PgPool, intent: &TransitionIntent) {
        let mut tx = pool.begin().await.expect("begin test tx");
        execute_pg_transition_intent(&mut tx, intent)
            .await
            .expect("execute pg intent");
        tx.commit().await.expect("commit test tx");
    }

    async fn seed_agent(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind("agent-1")
        .bind("Agent One")
        .bind("claude")
        .bind("111")
        .execute(pool)
        .await
        .expect("seed agent");
    }

    async fn seed_card(pool: &PgPool, card_id: &str, status: &str) {
        seed_agent(pool).await;
        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind(card_id)
        .bind(format!("Card {card_id}"))
        .bind(status)
        .bind("agent-1")
        .execute(pool)
        .await
        .expect("seed card");
    }

    async fn seed_dispatch(
        pool: &PgPool,
        dispatch_id: &str,
        card_id: &str,
        status: &str,
        dispatch_type: &str,
    ) {
        seed_agent(pool).await;
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6, NOW(), NOW()
             )",
        )
        .bind(dispatch_id)
        .bind(card_id)
        .bind("agent-1")
        .bind(dispatch_type)
        .bind(status)
        .bind(format!("Dispatch {dispatch_id}"))
        .execute(pool)
        .await
        .expect("seed dispatch");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn postgres_transition_intent_variants_mutate_expected_rows() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        seed_card(&pool, "card-status", "requested").await;
        execute_intent(
            &pool,
            &TransitionIntent::UpdateStatus {
                card_id: "card-status".to_string(),
                from: "requested".to_string(),
                to: "in_progress".to_string(),
            },
        )
        .await;
        let updated_status: String =
            sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = $1")
                .bind("card-status")
                .fetch_one(&pool)
                .await
                .expect("load updated status");
        assert_eq!(updated_status, "in_progress");

        seed_card(&pool, "card-latest", "ready").await;
        execute_intent(
            &pool,
            &TransitionIntent::SetLatestDispatchId {
                card_id: "card-latest".to_string(),
                dispatch_id: Some("dispatch-latest".to_string()),
            },
        )
        .await;
        let latest_dispatch_id: Option<String> =
            sqlx::query_scalar("SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1")
                .bind("card-latest")
                .fetch_one(&pool)
                .await
                .expect("load latest_dispatch_id");
        assert_eq!(latest_dispatch_id.as_deref(), Some("dispatch-latest"));

        seed_card(&pool, "card-review-status", "review").await;
        execute_intent(
            &pool,
            &TransitionIntent::SetReviewStatus {
                card_id: "card-review-status".to_string(),
                review_status: Some("reviewing".to_string()),
            },
        )
        .await;
        let review_status: Option<String> =
            sqlx::query_scalar("SELECT review_status FROM kanban_cards WHERE id = $1")
                .bind("card-review-status")
                .fetch_one(&pool)
                .await
                .expect("load review_status");
        assert_eq!(review_status.as_deref(), Some("reviewing"));

        seed_card(&pool, "card-clock", "requested").await;
        execute_intent(
            &pool,
            &TransitionIntent::ApplyClock {
                card_id: "card-clock".to_string(),
                state: "in_progress".to_string(),
                clock: Some(ClockConfig {
                    set: "started_at".to_string(),
                    mode: None,
                }),
            },
        )
        .await;
        let started_at: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT started_at FROM kanban_cards WHERE id = $1")
                .bind("card-clock")
                .fetch_one(&pool)
                .await
                .expect("load started_at");
        assert!(started_at.is_some(), "ApplyClock should set started_at");

        seed_card(&pool, "card-terminal", "done").await;
        sqlx::query(
            "UPDATE kanban_cards
             SET review_status = 'reviewing',
                 suggestion_pending_at = NOW(),
                 review_entered_at = NOW(),
                 awaiting_dod_at = NOW(),
                 blocked_reason = 'needs follow-up',
                 review_round = 3,
                 deferred_dod_json = '{\"missing\":[\"tests\"]}'
             WHERE id = $1",
        )
        .bind("card-terminal")
        .execute(&pool)
        .await
        .expect("seed terminal fields");
        execute_intent(
            &pool,
            &TransitionIntent::ClearTerminalFields {
                card_id: "card-terminal".to_string(),
            },
        )
        .await;
        let cleared = sqlx::query(
            "SELECT review_status, suggestion_pending_at, review_entered_at, awaiting_dod_at,
                    blocked_reason, review_round, deferred_dod_json
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-terminal")
        .fetch_one(&pool)
        .await
        .expect("load cleared terminal fields");
        assert!(cleared.get::<Option<String>, _>("review_status").is_none());
        assert!(
            cleared
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("suggestion_pending_at")
                .is_none()
        );
        assert!(
            cleared
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("review_entered_at")
                .is_none()
        );
        assert!(
            cleared
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("awaiting_dod_at")
                .is_none()
        );
        assert!(cleared.get::<Option<String>, _>("blocked_reason").is_none());
        assert!(cleared.get::<Option<i64>, _>("review_round").is_none());
        assert!(
            cleared
                .get::<Option<String>, _>("deferred_dod_json")
                .is_none()
        );

        seed_card(&pool, "card-aq", "done").await;
        for (run_id, status) in [
            ("run-own", "active"),
            ("run-active-sibling", "active"),
            ("run-paused-sibling", "paused"),
            ("run-generated", "generated"),
            ("run-completed", "completed"),
        ] {
            sqlx::query(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(run_id)
            .bind("test-repo")
            .bind("agent-1")
            .bind(status)
            .execute(&pool)
            .await
            .expect("seed auto_queue_run");
        }
        for (entry_id, run_id, status) in [
            ("entry-own", "run-own", "dispatched"),
            ("entry-active", "run-active-sibling", "pending"),
            ("entry-paused", "run-paused-sibling", "pending"),
            ("entry-generated", "run-generated", "pending"),
            ("entry-completed", "run-completed", "pending"),
        ] {
            sqlx::query(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, created_at
                 ) VALUES (
                    $1, $2, $3, $4, $5, NOW()
                 )",
            )
            .bind(entry_id)
            .bind(run_id)
            .bind("card-aq")
            .bind("agent-1")
            .bind(status)
            .execute(&pool)
            .await
            .expect("seed auto_queue_entry");
        }
        execute_intent(
            &pool,
            &TransitionIntent::SyncAutoQueue {
                card_id: "card-aq".to_string(),
            },
        )
        .await;
        let statuses: HashMap<String, String> = sqlx::query(
            "SELECT id, status
             FROM auto_queue_entries
             WHERE kanban_card_id = $1
             ORDER BY id",
        )
        .bind("card-aq")
        .fetch_all(&pool)
        .await
        .expect("load auto_queue statuses")
        .into_iter()
        .map(|row| (row.get::<String, _>("id"), row.get::<String, _>("status")))
        .collect();
        assert_eq!(statuses.get("entry-own").map(String::as_str), Some("done"));
        assert_eq!(
            statuses.get("entry-active").map(String::as_str),
            Some("skipped")
        );
        assert_eq!(
            statuses.get("entry-paused").map(String::as_str),
            Some("skipped")
        );
        assert_eq!(
            statuses.get("entry-generated").map(String::as_str),
            Some("pending")
        );
        assert_eq!(
            statuses.get("entry-completed").map(String::as_str),
            Some("pending")
        );

        seed_card(&pool, "card-review-state", "review").await;
        execute_intent(
            &pool,
            &TransitionIntent::SyncReviewState {
                card_id: "card-review-state".to_string(),
                state: "reviewing".to_string(),
            },
        )
        .await;
        let review_state = sqlx::query(
            "SELECT state, review_entered_at
             FROM card_review_state
             WHERE card_id = $1",
        )
        .bind("card-review-state")
        .fetch_one(&pool)
        .await
        .expect("load card_review_state");
        assert_eq!(review_state.get::<String, _>("state"), "reviewing");
        assert!(
            review_state
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("review_entered_at")
                .is_some()
        );

        seed_card(&pool, "card-audit", "ready").await;
        execute_intent(
            &pool,
            &TransitionIntent::AuditLog {
                card_id: "card-audit".to_string(),
                from: "ready".to_string(),
                to: "requested".to_string(),
                source: "test".to_string(),
                message: "OK".to_string(),
            },
        )
        .await;
        let audit = sqlx::query(
            "SELECT from_status, to_status, source, result
             FROM kanban_audit_logs
             WHERE card_id = $1
             ORDER BY id DESC
             LIMIT 1",
        )
        .bind("card-audit")
        .fetch_one(&pool)
        .await
        .expect("load audit log");
        assert_eq!(audit.get::<String, _>("from_status"), "ready");
        assert_eq!(audit.get::<String, _>("to_status"), "requested");
        assert_eq!(audit.get::<String, _>("source"), "test");
        assert_eq!(audit.get::<String, _>("result"), "OK");

        seed_card(&pool, "card-dispatch", "requested").await;
        seed_dispatch(
            &pool,
            "dispatch-cancel",
            "card-dispatch",
            "pending",
            "implementation",
        )
        .await;
        execute_intent(
            &pool,
            &TransitionIntent::CancelDispatch {
                dispatch_id: "dispatch-cancel".to_string(),
            },
        )
        .await;
        let dispatch_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-cancel")
                .fetch_one(&pool)
                .await
                .expect("load cancelled dispatch");
        assert_eq!(dispatch_status, "cancelled");

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_cleanup_preserves_live_review_dispatch_without_verdict() {
        let test_db = TestDatabase::create().await;
        let pool = test_db.migrate().await;

        seed_card(&pool, "card-review-recovery", "done").await;
        seed_dispatch(
            &pool,
            "dispatch-review-missing-verdict",
            "card-review-recovery",
            "pending",
            "review",
        )
        .await;
        seed_dispatch(
            &pool,
            "dispatch-impl-terminal",
            "card-review-recovery",
            "pending",
            "implementation",
        )
        .await;
        sqlx::query(
            "UPDATE kanban_cards
             SET review_status = 'reviewing',
                 latest_dispatch_id = 'dispatch-review-missing-verdict'
             WHERE id = $1",
        )
        .bind("card-review-recovery")
        .execute(&pool)
        .await
        .expect("seed review card state");

        let mut tx = pool.begin().await.expect("begin terminal cleanup tx");
        let cancelled =
            cancel_live_dispatches_for_terminal_card_pg(&mut tx, "card-review-recovery")
                .await
                .expect("terminal cleanup");
        tx.commit().await.expect("commit terminal cleanup tx");

        assert_eq!(
            cancelled, 1,
            "only non-review cleanup dispatches should be cancelled"
        );
        let review_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-review-missing-verdict")
                .fetch_one(&pool)
                .await
                .expect("load preserved review dispatch");
        assert_eq!(review_status, "pending");
        let impl_status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
                .bind("dispatch-impl-terminal")
                .fetch_one(&pool)
                .await
                .expect("load cancelled implementation dispatch");
        assert_eq!(impl_status, "cancelled");

        let card = sqlx::query(
            "SELECT review_status, blocked_reason, latest_dispatch_id
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-review-recovery")
        .fetch_one(&pool)
        .await
        .expect("load recovery-marked card");
        assert_eq!(
            card.get::<Option<String>, _>("review_status").as_deref(),
            Some("review_recovery_needed")
        );
        assert_eq!(
            card.get::<Option<String>, _>("latest_dispatch_id")
                .as_deref(),
            Some("dispatch-review-missing-verdict")
        );
        assert!(
            card.get::<Option<String>, _>("blocked_reason")
                .as_deref()
                .is_some_and(|reason| reason.contains("without verdict")),
            "card status should surface the recoverable missing-verdict condition"
        );

        pool.close().await;
        test_db.drop().await;
    }
}
