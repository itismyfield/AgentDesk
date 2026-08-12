/// SQL-list form of every status protected by the channel open-route invariant.
/// Shared between migration schema (index WHERE clause) and Rust query sites to prevent
/// drift. Lifecycle: pending → claimed → accepted → spawned → dispatched → done.
/// Keep synchronized with:
///   - `migrations/postgres/0105_intake_outbox_dispatched_status.sql` (staged index WHERE)
///   - `migrations/postgres/0106_intake_outbox_dispatched_status_swap.sql` (CHECK and index swap)
///   - All usages in `src/db/intake_outbox.rs` and related services
///   - Partial unique index `intake_outbox_one_open_route_per_channel` predicate
pub(crate) const INTAKE_OUTBOX_OPEN_STATUSES_SQL: &str =
    "'pending', 'claimed', 'accepted', 'spawned', 'dispatched'";

#[cfg(test)]
mod tests {
    use super::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
    use std::collections::BTreeSet;
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

    fn quoted_statuses(sql_list: &str) -> Vec<&str> {
        sql_list
            .split(',')
            .map(|status| status.trim().trim_matches('\''))
            .filter(|status| !status.is_empty())
            .collect()
    }

    #[test]
    fn intake_open_status_sql_textually_matches_migration_0105() {
        // Text-level validation of migration 0105/0106 content. This cannot verify that the
        // migrations will apply successfully to a running PostgreSQL (no fixture in unit tests).
        // VALIDATES (text patterns only):
        //   1. Migration opts out of SQLx's transaction for CREATE INDEX CONCURRENTLY
        //   2. Constant includes 'dispatched' in lifecycle order
        //   3. Migration CHECK retains every pre-T2-M status and adds dispatched
        //   4. Index WHERE clause uses the shared constant (not legacy literal)
        //   5. Index name is preserved (needed for 23505 error classification in Rust)
        // DOES NOT VALIDATE (requires PG integration):
        //   - Constraint actually admits dispatched after apply
        //   - Index partial predicate works correctly on queries
        //   - Compatibility with deployed versions
        // LIMITS:
        //   - Query-site coverage is a hard-coded four-file allowlist, so a new file can add an
        //     unguarded legacy predicate without this contract seeing it.
        //   - The source scan is intentionally lexical; a legacy literal in a comment is a
        //     false positive even though PostgreSQL would never execute it.
        //   - The query-site test rejects only the exact legacy four-status set; an inline
        //     non-legacy tuple can bypass the shared-constant contract.
        let migration_0105 =
            repo_source("migrations/postgres/0105_intake_outbox_dispatched_status.sql");
        let migration_0106 =
            repo_source("migrations/postgres/0106_intake_outbox_dispatched_status_swap.sql");
        assert_eq!(
            migration_0105.lines().next(),
            Some("-- no-transaction"),
            "migration 0105 must opt out of SQLx's transaction on its first line"
        );
        assert_eq!(
            INTAKE_OUTBOX_OPEN_STATUSES_SQL,
            "'pending', 'claimed', 'accepted', 'spawned', 'dispatched'",
            "shared open-status SQL must include dispatched in lifecycle order"
        );
        assert_eq!(
            migration_0105
                .matches("CREATE UNIQUE INDEX CONCURRENTLY")
                .count(),
            1,
            "migration 0105 must contain exactly one concurrent index build"
        );
        assert!(
            !migration_0105.contains("ALTER TABLE") && !migration_0105.contains("BEGIN;"),
            "migration 0105 must stay single-stage so SQLx does not create an implicit transaction block"
        );
        let check_sql = migration_0106
            .split_once("ADD CONSTRAINT intake_outbox_status_check CHECK (status IN (")
            .expect("migration must add the named status CHECK")
            .1
            .split_once(")) NOT VALID;")
            .expect("status CHECK must remain NOT VALID to avoid a redundant table scan")
            .0;
        let check_statuses = quoted_statuses(check_sql);
        for pre_t2m_status in [
            "pending",
            "claimed",
            "accepted",
            "spawned",
            "done",
            "failed_pre_accept",
            "failed_post_accept",
        ] {
            assert!(
                check_statuses.contains(&pre_t2m_status),
                "migration status CHECK must preserve pre-T2-M status {pre_t2m_status}"
            );
        }
        assert!(
            check_statuses.contains(&"dispatched"),
            "migration status CHECK must add dispatched"
        );
        assert!(
            migration_0105.contains(&format!(
                "WHERE status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})"
            )),
            "index WHERE clause must consume the shared constant"
        );
        let normalized_migration_0105 = normalize_sql(&migration_0105);
        let normalized_migration_0106 = normalize_sql(&migration_0106);
        assert!(
            normalized_migration_0105.contains(
                "CREATE UNIQUE INDEX CONCURRENTLY intake_outbox_one_open_route_per_channel_t2m"
            ) && normalized_migration_0106.contains(
                "ALTER INDEX intake_outbox_one_open_route_per_channel_t2m RENAME TO intake_outbox_one_open_route_per_channel"
            ),
            "index discriminator name must be preserved for 23505 error classification"
        );
    }

    #[test]
    fn intake_open_status_query_sites_do_not_reintroduce_the_legacy_literal() {
        let legacy_statuses = BTreeSet::from(["pending", "claimed", "accepted", "spawned"]);
        for (site, source, shared_predicate) in [
            (
                "db::intake_outbox",
                repo_source("src/db/intake_outbox.rs"),
                "status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})",
            ),
            (
                "cluster::intake_router_hook",
                repo_source("src/services/cluster/intake_router_hook.rs"),
                "status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})",
            ),
            (
                "cluster::intake_router_hook::owner_record",
                repo_source("src/services/cluster/intake_router_hook/owner_record.rs"),
                "status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})",
            ),
            (
                "discord::router::intake_dispatch::tests",
                repo_source("src/services/discord/router/intake_dispatch/tests.rs"),
                "status IN ({})\",crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL",
            ),
        ] {
            let normalized = normalize_sql(&source);
            let shared_predicate_count = normalized.matches(shared_predicate).count();
            assert!(
                shared_predicate_count > 0,
                "{site} must retain at least one shared intake status predicate so this source scan is not vacuous"
            );
            for suffix in normalized.split("status IN (").skip(1) {
                let Some(sql_list) = suffix.split_once(')').map(|(list, _)| list) else {
                    continue;
                };
                let found = quoted_statuses(sql_list)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                assert_ne!(
                    found, legacy_statuses,
                    "{site} must consume INTAKE_OUTBOX_OPEN_STATUSES_SQL instead of a legacy literal in any tuple order"
                );
            }
        }
    }
}
