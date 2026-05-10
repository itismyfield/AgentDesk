//! `intake_outbox` table primitives — Phase 1 of the intake-node-routing
//! design (`docs/design/intake-node-routing.md`).
//!
//! Phase 1 ships only the schema (migration `0052_intake_node_routing.sql`)
//! and a small test module that verifies the migration applies correctly
//! and the constraints behave as designed.
//!
//! Routing helpers, claim functions, and state-transition wrappers all
//! arrive in Phase 2; this file is intentionally otherwise empty so that
//! follow-up PRs add their helpers here without introducing a new
//! `pub mod` declaration.

#[cfg(test)]
mod migration_tests {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::json;
    use sqlx::Row;

    /// The migration must add the new `agents.preferred_intake_node_labels`
    /// column with a default of `'[]'::JSONB`. Existing agent reads must
    /// continue to work — verified by inserting a row without referencing
    /// the column and then reading the default back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agents_preferred_intake_node_labels_defaults_to_empty_array() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-intake-default', 'Test', 'claude', '111')",
        )
        .execute(&pool)
        .await
        .expect("seed agent");

        let value: serde_json::Value = sqlx::query_scalar(
            "SELECT preferred_intake_node_labels FROM agents WHERE id = 'agent-intake-default'",
        )
        .fetch_one(&pool)
        .await
        .expect("read column");
        assert_eq!(value, json!([]));

        pool.close().await;
        pg_db.drop().await;
    }

    /// CHECK constraint must reject an unknown status value. Production
    /// code only writes the seven values from the design; this guard
    /// catches accidental typos at insert time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_status_check_rejects_unknown_value() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let result = insert_minimal_row(&pool, "ch", "msg", 1, "running").await;
        let error = result.expect_err("status='running' must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("intake_outbox_status_check"),
            "expected status CHECK violation, got: {message}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// `attempt_no` must be at least 1 (covers the underflow case where
    /// a follow-up retry helper computes `MAX - 1` by mistake).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_attempt_no_check_rejects_zero() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let result = insert_minimal_row(&pool, "ch", "msg", 0, "pending").await;
        let error = result.expect_err("attempt_no=0 must be rejected");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_attempt_no_positive"),
            "expected attempt_no CHECK violation, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Two rows in the same `(channel_id, user_msg_id)` family with the
    /// same `attempt_no` must violate the named 3-tuple constraint. This
    /// is the constraint name Rust callers of `retry-local` /
    /// `retry-as-new` match against to decide whether to recompute
    /// `family_max + 1` and retry the INSERT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_unique_message_attempt_blocks_duplicate_attempt_no() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Drive the row terminal so the partial unique index does not
        // mask the 3-tuple violation.
        insert_minimal_row(&pool, "ch-attempt", "msg-1", 1, "done")
            .await
            .expect("first attempt insert");

        let result = insert_minimal_row(&pool, "ch-attempt", "msg-1", 1, "done").await;
        let error = result.expect_err("duplicate attempt_no must be rejected");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_unique_message_attempt"),
            "expected 3-tuple constraint violation, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// At most ONE row per channel may exist in any OPEN status. The
    /// partial unique index name is the discriminator the Rust handler
    /// matches against to decide whether to fall back to `Local`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_one_open_route_per_channel_blocks_second_open_row() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        insert_minimal_row(&pool, "ch-open", "msg-A", 1, "pending")
            .await
            .expect("first open row");

        let result = insert_minimal_row(&pool, "ch-open", "msg-B", 1, "pending").await;
        let error = result.expect_err("second OPEN row must be rejected");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_one_open_route_per_channel"),
            "expected partial-unique-index violation, got: {error}"
        );

        // After the first row terminates, a fresh OPEN row for a
        // different user_msg_id is allowed.
        sqlx::query(
            "UPDATE intake_outbox SET status='done', completed_at=NOW()
             WHERE channel_id='ch-open' AND user_msg_id='msg-A' AND attempt_no=1",
        )
        .execute(&pool)
        .await
        .expect("transition to done");

        insert_minimal_row(&pool, "ch-open", "msg-B", 1, "pending")
            .await
            .expect("fresh OPEN row after parent terminal");

        pool.close().await;
        pg_db.drop().await;
    }

    /// The BEFORE-UPDATE trigger must keep `updated_at` fresh on every
    /// state transition without callers having to set it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_touch_updated_at_trigger_advances_on_update() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        insert_minimal_row(&pool, "ch-trig", "msg-T", 1, "pending")
            .await
            .expect("seed row");

        let row = sqlx::query(
            "SELECT created_at, updated_at FROM intake_outbox
             WHERE channel_id='ch-trig' AND user_msg_id='msg-T' AND attempt_no=1",
        )
        .fetch_one(&pool)
        .await
        .expect("load timestamps");
        let initial_updated_at: chrono::DateTime<chrono::Utc> = row
            .try_get("updated_at")
            .expect("decode initial updated_at");

        // Sleep just enough for NOW() to advance reliably.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        sqlx::query(
            "UPDATE intake_outbox SET last_error='probe'
             WHERE channel_id='ch-trig' AND user_msg_id='msg-T' AND attempt_no=1",
        )
        .execute(&pool)
        .await
        .expect("update last_error");

        let new_updated_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT updated_at FROM intake_outbox
             WHERE channel_id='ch-trig' AND user_msg_id='msg-T' AND attempt_no=1",
        )
        .fetch_one(&pool)
        .await
        .expect("read new updated_at");

        assert!(
            new_updated_at > initial_updated_at,
            "updated_at must advance on UPDATE: was {initial_updated_at}, now {new_updated_at}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// `parent_outbox_id REFERENCES intake_outbox(id) ON DELETE SET NULL`
    /// is design-required for retention safety: when a future cleanup job
    /// prunes ancestor rows, the child's audit-chain pointer becomes NULL
    /// rather than cascading the delete or leaving a dangling FK. Verify
    /// the constraint actually behaves that way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_parent_on_delete_set_null_preserves_child() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Seed a terminal parent. attempt_no = 1.
        insert_minimal_row(&pool, "ch-cascade", "msg-X", 1, "failed_pre_accept")
            .await
            .expect("seed parent");
        let parent_id: i64 = sqlx::query_scalar(
            "SELECT id FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=1",
        )
        .fetch_one(&pool)
        .await
        .expect("load parent id");

        // Seed a child attempt_no = 2 referencing the parent. Use a raw
        // INSERT that includes parent_outbox_id (the helper does not).
        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, request_owner_name,
                user_text, turn_kind, agent_id,
                status, attempt_no, parent_outbox_id
             ) VALUES (
                'leader-1', 'leader-1', '[]'::JSONB,
                'ch-cascade', 'msg-X', 'user-1', 'Tester',
                'hello', 'standard', 'agent-x',
                'done', 2, $1
             )",
        )
        .bind(parent_id)
        .execute(&pool)
        .await
        .expect("seed child");

        // Sanity: child references the parent.
        let pre_delete: Option<i64> = sqlx::query_scalar(
            "SELECT parent_outbox_id FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=2",
        )
        .fetch_one(&pool)
        .await
        .expect("read parent ref");
        assert_eq!(pre_delete, Some(parent_id));

        // Delete the parent row directly. The child must remain (no
        // cascade) but its parent_outbox_id must be NULLed.
        let deleted = sqlx::query("DELETE FROM intake_outbox WHERE id = $1")
            .bind(parent_id)
            .execute(&pool)
            .await
            .expect("delete parent")
            .rows_affected();
        assert_eq!(deleted, 1, "parent must delete cleanly");

        let child_after: Option<i64> = sqlx::query_scalar(
            "SELECT parent_outbox_id FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=2",
        )
        .fetch_one(&pool)
        .await
        .expect("read child after parent delete");
        assert_eq!(
            child_after, None,
            "child's parent_outbox_id must be NULLed by ON DELETE SET NULL"
        );

        let child_id_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=2",
        )
        .fetch_one(&pool)
        .await
        .expect("count child");
        assert_eq!(child_id_count, 1, "child row must NOT cascade-delete");

        pool.close().await;
        pg_db.drop().await;
    }

    async fn insert_minimal_row(
        pool: &sqlx::PgPool,
        channel_id: &str,
        user_msg_id: &str,
        attempt_no: i32,
        status: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, request_owner_name,
                user_text, turn_kind, agent_id,
                status, attempt_no
             ) VALUES (
                'worker-1', 'leader-1', '[]'::JSONB,
                $1, $2, 'user-1', 'Tester',
                'hello', 'standard', 'agent-x',
                $3, $4
             )",
        )
        .bind(channel_id)
        .bind(user_msg_id)
        .bind(status)
        .bind(attempt_no)
        .execute(pool)
        .await
        .map(|_| ())
    }
}
