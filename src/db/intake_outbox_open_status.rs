/// SQL-list form of every status protected by the channel open-route invariant.
/// Shared between migration schema (index WHERE clause) and Rust query sites to prevent
/// drift. Lifecycle: pending → claimed → accepted → spawned → dispatched → done.
/// Keep synchronized with:
///   - `migrations/postgres/0105_intake_outbox_dispatched_status.sql` (CHECK and index WHERE)
///   - All usages in `src/db/intake_outbox.rs` and related services
///   - Partial unique index `intake_outbox_one_open_route_per_channel` predicate
pub(crate) const INTAKE_OUTBOX_OPEN_STATUSES_SQL: &str =
    "'pending', 'claimed', 'accepted', 'spawned', 'dispatched'";

#[cfg(test)]
mod tests {
    use super::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
    use std::path::PathBuf;

    fn repo_source(path: &str) -> String {
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap_or_else(|error| panic!("read {path}: {error}"))
    }

    fn normalize_sql(source: &str) -> String {
        source
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(", ", ",")
    }

    #[test]
    fn intake_open_status_sql_textually_matches_migration_0105() {
        // Text-level validation of migration 0105 content. This cannot verify that the migration
        // will apply successfully to a running PostgreSQL (no fixture available in unit tests).
        // VALIDATES (text patterns only):
        //   1. Constant includes 'dispatched' in lifecycle order
        //   2. Migration contains 'spawned'/'dispatched'/'done' (indicates CHECK extension)
        //   3. Index WHERE clause uses the shared constant (not legacy literal)
        //   4. Index name is preserved (needed for 23505 error classification in Rust)
        // DOES NOT VALIDATE (requires PG integration):
        //   - Constraint actually admits dispatched after apply
        //   - Index partial predicate works correctly on queries
        //   - Compatibility with deployed versions
        let migration_0105 =
            repo_source("migrations/postgres/0105_intake_outbox_dispatched_status.sql");
        assert_eq!(
            INTAKE_OUTBOX_OPEN_STATUSES_SQL,
            "'pending', 'claimed', 'accepted', 'spawned', 'dispatched'",
            "shared open-status SQL must include dispatched in lifecycle order"
        );
        assert!(
            migration_0105.contains("'spawned',\n        'dispatched',\n        'done'"),
            "migration text must contain pattern indicating CHECK constraint extension"
        );
        assert!(
            migration_0105.contains(&format!(
                "WHERE status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})"
            )),
            "index WHERE clause must consume the shared constant"
        );
        assert!(
            migration_0105.contains("CREATE UNIQUE INDEX intake_outbox_one_open_route_per_channel"),
            "index discriminator name must be preserved for 23505 error classification"
        );
    }

    #[test]
    fn intake_open_status_sql_preserves_pre_t2m_tuple_when_dispatched_is_unwritten() {
        let prior = ["'pending'", "'claimed'", "'accepted'", "'spawned'"];
        let without_dispatched = INTAKE_OUTBOX_OPEN_STATUSES_SQL
            .split(", ")
            .filter(|status| *status != "'dispatched'")
            .collect::<Vec<_>>();
        assert_eq!(
            without_dispatched, prior,
            "removing the dormant status must reproduce the pre-T2-M SQL tuple"
        );
    }

    #[test]
    fn intake_open_status_query_sites_do_not_reintroduce_the_legacy_literal() {
        let legacy_predicate = "status IN ('pending','claimed','accepted','spawned')";
        for (site, source) in [
            ("db::intake_outbox", repo_source("src/db/intake_outbox.rs")),
            (
                "cluster::intake_router_hook",
                repo_source("src/services/cluster/intake_router_hook.rs"),
            ),
            (
                "cluster::intake_router_hook::owner_record",
                repo_source("src/services/cluster/intake_router_hook/owner_record.rs"),
            ),
            (
                "discord::router::intake_dispatch::tests",
                repo_source("src/services/discord/router/intake_dispatch/tests.rs"),
            ),
        ] {
            assert!(
                !normalize_sql(&source).contains(legacy_predicate),
                "{site} must consume INTAKE_OUTBOX_OPEN_STATUSES_SQL instead of a legacy literal"
            );
        }
    }
}
