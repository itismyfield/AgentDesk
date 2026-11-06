/// SQL-list form of every status protected by the channel open-route invariant.
/// Shared between migration schema (index WHERE clause) and Rust query sites to prevent
/// drift. Lifecycle: pending → claimed → accepted → spawned → dispatched → done.
/// Keep synchronized with:
///   - `migrations/postgres/0105_intake_outbox_dispatched_status.sql` (staged index WHERE)
///   - `migrations/postgres/0106_intake_outbox_dispatched_status_swap.sql` (CHECK and index swap)
///   - `migrations/postgres/0107_intake_outbox_dispatched_clock.sql` (nine-state CHECK)
///   - All usages in `src/db/intake_outbox.rs` and related services
///   - Partial unique index `intake_outbox_one_open_route_per_channel` predicate
pub(crate) const INTAKE_OUTBOX_OPEN_STATUSES_SQL: &str =
    "'pending', 'claimed', 'accepted', 'spawned', 'dispatched'";

#[cfg(test)]
mod tests {
    use super::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::intake_outbox_status::IntakeOutboxStatus;
    use std::collections::BTreeSet;

    /// The single-quoted values of a SQL list or a catalog definition.
    fn quoted_statuses(sql: &str) -> BTreeSet<String> {
        sql.split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    /// After every migration, the open-route fence covers exactly the shared open statuses
    /// and the status CHECK admits exactly the typed statuses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migrated_open_route_fence_and_status_check_match_the_shared_statuses_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let (unique, valid, key, predicate): (bool, bool, String, String) = sqlx::query_as(
            "SELECT i.indisunique, i.indisvalid,
                    pg_get_indexdef(i.indexrelid, 1, true),
                    pg_get_expr(i.indpred, i.indrelid)
               FROM pg_index i
               JOIN pg_class c ON c.oid = i.indexrelid
              WHERE c.relname = 'intake_outbox_one_open_route_per_channel'
                AND i.indrelid = 'intake_outbox'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("read the open-route index catalog");
        assert!(
            unique && valid,
            "the open-route fence must be a valid unique index"
        );
        assert_eq!(
            key, "channel_id",
            "the fence admits one open row per channel"
        );
        assert_eq!(
            quoted_statuses(&predicate),
            quoted_statuses(INTAKE_OUTBOX_OPEN_STATUSES_SQL),
            "fence predicate {predicate} must cover exactly the shared open statuses"
        );

        let check: String = sqlx::query_scalar(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint
              WHERE conrelid = 'intake_outbox'::regclass
                AND conname = 'intake_outbox_status_check'",
        )
        .fetch_one(&pool)
        .await
        .expect("read the status CHECK");
        let typed: BTreeSet<String> = IntakeOutboxStatus::ALL
            .into_iter()
            .map(|status| status.as_str().to_owned())
            .collect();
        assert_eq!(quoted_statuses(&check), typed, "status CHECK {check}");

        pool.close().await;
        pg_db.drop().await;
    }
}
