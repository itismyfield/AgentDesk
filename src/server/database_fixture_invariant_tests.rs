//! Source-level contract for the cluster test fixtures that build their own
//! database URL (#5218).
//!
//! Four fixtures used to end with `format!("postgres://{user}@<loopback>")`
//! when `POSTGRES_TEST_DATABASE_URL_BASE` was unset. That fallback made a test
//! run connect to whatever database server happened to be listening on the
//! developer's loopback — an operational one, on the machines where these lanes
//! actually run — and create, migrate, and drop databases on it.
//!
//! These assertions exist because the behavioural version of this check cannot
//! be written honestly: proving "the fixture does not connect anywhere" needs a
//! server to be absent, and a test that requires a server to be absent is not
//! something a lane can schedule. Reading the fixture sources costs nothing,
//! needs no server, and fails loudly the moment the fallback comes back.
//!
//! This module is intentionally named without a lane token so it runs in every
//! lane, including the PG-less ones the fallback used to endanger. It must stay
//! free of the fixture seed identifiers that
//! `scripts/check_pg_test_lane_membership.py` scans for, or the classifier will
//! read it as a database-dependent test and schedule it out of exactly the
//! lanes it is meant to protect.

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The fixture sources under contract, paired with the module path a
    /// reviewer would grep for.
    const FIXTURE_SOURCES: &[(&str, &str)] = &[
        (
            "server::multinode_regression",
            include_str!("multinode_regression.rs"),
        ),
        ("server::issue_specs", include_str!("issue_specs.rs")),
        ("server::resource_locks", include_str!("resource_locks.rs")),
        (
            "server::task_dispatch_claims",
            include_str!("task_dispatch_claims.rs"),
        ),
    ];

    /// Every source currently carrying the legacy PG host seed. The membership
    /// test below discovers this set from `src/**/*.rs`, so a new file or an
    /// additional seed in an existing file fails until it is reviewed here.
    const PG_HOST_SEED_SOURCES: &[&str] = &[
        "src/db/auto_queue/test_support.rs",
        "src/db/dispatched_sessions.rs",
        "src/db/dispatches/delivery_events.rs",
        "src/db/postgres.rs",
        "src/dispatch/test_support.rs",
        "src/engine/ops/db_ops.rs",
        "src/high_risk_recovery.rs",
        "src/reconcile.rs",
        "src/server/routes/dispatches/crud.rs",
        "src/server/routes/escalation.rs",
        "src/services/discord/mod.rs",
        "src/services/discord/runtime_bootstrap/gateway_lease_recovery_tests.rs",
        "src/services/dispatches/outbox_claiming.rs",
        "src/services/dispatches/wait_queue.rs",
        "src/services/observability/recovery_audit.rs",
        "src/services/observability/turn_lifecycle.rs",
        "src/services/pipeline_override.rs",
        "src/voice/turn_link.rs",
    ];

    fn rust_sources() -> Vec<(String, String)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        fn visit(root: &Path, directory: &Path, sources: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(directory).expect("read Rust source directory") {
                let path = entry.expect("read Rust source entry").path();
                if path.is_dir() {
                    visit(root, &path, sources);
                } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                    let source = std::fs::read_to_string(&path).expect("read Rust source");
                    let relative = path
                        .strip_prefix(root)
                        .expect("Rust source must stay below the repository root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    sources.push((relative, source));
                }
            }
        }
        let mut sources = Vec::new();
        visit(root, &root.join("src"), &mut sources);
        sources.sort();
        sources
    }

    /// Assembled at runtime so this file does not itself contain the literal it
    /// forbids; a plain grep for the address stays a reliable audit.
    fn forbidden_address() -> String {
        format!("{}:{}", "127.0.0.1", "5432")
    }

    /// The shared helper the four fixtures now depend on.
    const SHARED_HELPER_SOURCE: &str = include_str!("../db/postgres.rs");

    /// The contract has to be read inside the helper's own body. The identical
    /// `AGENTDESK_REQUIRE_PG` comparison also appears in `require_pg_guard`
    /// further down the same file, so a whole-file search reports the contract
    /// as intact even after the helper's copy is gutted — a mutation that
    /// replaced the helper's condition survived a whole-file assertion here
    /// before this narrowing was added.
    fn shared_helper_body() -> &'static str {
        const SIGNATURE: &str =
            "pub(crate) fn postgres_test_database_url_base() -> Option<String> {";
        let start = SHARED_HELPER_SOURCE
            .find(SIGNATURE)
            .expect("db::postgres no longer defines postgres_test_database_url_base (#5218)");
        let rest = &SHARED_HELPER_SOURCE[start..];
        let end = rest
            .find("\n}\n")
            .expect("cannot find the end of postgres_test_database_url_base (#5218)");
        &rest[..end]
    }

    /// The fixtures answer "no base configured" with a skip, which is only
    /// defensible because the required lanes turn that same condition into a
    /// panic. Nothing else in the tree pins that escalation, so deleting it
    /// would silently downgrade every one of those lanes to a soft-skip green —
    /// the exact failure mode this module exists to prevent. Asserted on the
    /// source because the behaviour needs process-wide environment mutation,
    /// which this module must not do while other tests run beside it.
    #[test]
    fn a_missing_fixture_base_stays_fatal_for_lanes_that_require_a_database() {
        let body = shared_helper_body();
        for fragment in [
            "std::env::var(AGENTDESK_REQUIRE_PG_ENV).ok().as_deref() == Some(\"1\")",
            "base.is_none()",
            "panic!(\"PG required but POSTGRES_TEST_DATABASE_URL_BASE unset\")",
        ] {
            assert!(
                body.contains(fragment),
                "the body of postgres_test_database_url_base no longer contains \
                 `{fragment}`; without it a missing fixture base stops being \
                 fatal under AGENTDESK_REQUIRE_PG=1 and the fixtures' skip \
                 becomes a silent green (#5218, #4979 S2)"
            );
        }
    }

    #[test]
    fn fixture_url_gate_is_wired_to_every_shared_database_entrypoint() {
        for (fragment, expected) in [
            ("test_database_url_is_configured(", 8),
            ("test_database_config_is_configured(", 2),
            ("let base = postgres_test_database_url_base();", 1),
            (
                "test_database_url_matches_configured_base(base.as_deref(),",
                1,
            ),
            ("let Some(base) = base else {", 1),
            ("if !database_url.starts_with(base) {", 1),
        ] {
            assert_eq!(
                SHARED_HELPER_SOURCE.matches(fragment).count(),
                expected,
                "shared fixture gate wiring changed at `{fragment}` (#5229)"
            );
        }
    }

    #[test]
    fn every_pg_host_seed_source_is_registered_with_its_current_count() {
        let seed = ["PG", "HOST"].concat();
        let discovered: Vec<_> = rust_sources()
            .into_iter()
            .flat_map(|(path, source)| std::iter::repeat_n(path, source.matches(&seed).count()))
            .collect();
        assert_eq!(
            discovered, PG_HOST_SEED_SOURCES,
            "legacy PG host seed membership changed; source grep is only an auxiliary alarm, but every occurrence must be reviewed (#5229)"
        );
    }

    #[test]
    fn create_database_sql_has_one_chokepoint_under_db_postgres() {
        let needle = ["CREATE", " DATABASE"].concat();
        let emissions: Vec<_> = rust_sources()
            .into_iter()
            .flat_map(|(path, source)| {
                source
                    .lines()
                    .filter(|line| line.contains(&needle) && !line.trim_start().starts_with("//"))
                    .map(move |_| path.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            emissions,
            ["src/db/postgres.rs"],
            "database creation must have one guarded source emission; found {emissions:?} (#5229)"
        );
    }

    #[test]
    fn cluster_fixtures_never_hardcode_a_database_server_address() {
        let needle = forbidden_address();
        for (module, source) in FIXTURE_SOURCES {
            assert!(
                !source.contains(&needle),
                "{module} hardcodes {needle}; a fixture must never name a server \
                 the lane did not configure (#5218)"
            );
        }
    }

    #[test]
    fn cluster_fixtures_have_no_private_base_url_builder() {
        for (module, source) in FIXTURE_SOURCES {
            assert!(
                !source.contains("fn postgres_base_database_url"),
                "{module} reintroduced a private base-URL builder; the shared \
                 helper is the only sanctioned source and it is the one that \
                 honours AGENTDESK_REQUIRE_PG (#5218)"
            );
        }
    }

    #[test]
    fn cluster_fixtures_read_the_shared_base_url_helper() {
        for (module, source) in FIXTURE_SOURCES {
            assert!(
                source.contains("postgres_test_database_url_base()"),
                "{module} no longer reads the shared fixture base; without it a \
                 missing base cannot be turned into a hard failure under \
                 AGENTDESK_REQUIRE_PG=1 (#5218)"
            );
        }
    }

    /// A missing base must stay a `?`/`else` skip and never widen into a
    /// silently invented value. `unwrap_or_else` on the helper would restore
    /// the defect with different syntax.
    #[test]
    fn cluster_fixtures_do_not_substitute_a_value_for_a_missing_base() {
        for (module, source) in FIXTURE_SOURCES {
            // Match against whitespace-stripped source: rustfmt puts a method
            // chain on its own line as soon as it grows, and an assertion that
            // only sees the single-line spelling would miss the reformatted one.
            let dense: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            for banned in [
                "postgres_test_database_url_base().unwrap",
                "postgres_test_database_url_base().unwrap_or",
                "postgres_test_database_url_base().unwrap_or_else",
                "postgres_test_database_url_base().unwrap_or_default",
                "PGUSER",
            ] {
                assert!(
                    !dense.contains(banned),
                    "{module} contains `{banned}`; a missing fixture base must \
                     stay missing, not be substituted for (#5218)"
                );
            }
        }
    }

    /// The lane token has to sit inside the module path, not at its end. The PR
    /// sweep skips `_pg`/`pg_`/`postgres`; the nightly `full_macos` and
    /// `full_windows` jobs skip `_pg_`/`postgres_`. A trailing `_pg` satisfies
    /// the first and slips through the second, which is how these modules came
    /// to run in lanes that had no server.
    #[test]
    fn database_backed_modules_carry_a_token_both_skip_filters_match() {
        const DATABASE_BACKED_MODULES: &[(&str, &str)] = &[
            (
                "server::multinode_regression",
                "multinode_regression_pg_tests",
            ),
            ("server::issue_specs", "issue_specs_pg_tests"),
            ("server::resource_locks", "resource_locks_pg_tests"),
            (
                "server::task_dispatch_claims",
                "task_dispatch_claims_pg_tests",
            ),
        ];
        const PR_SWEEP_SKIPS: &[&str] = &["_pg", "pg_", "postgres"];
        const NIGHTLY_SKIPS: &[&str] = &["_pg_", "postgres_"];

        for ((module, module_name), (source_module, source)) in
            DATABASE_BACKED_MODULES.iter().zip(FIXTURE_SOURCES.iter())
        {
            assert_eq!(
                module, source_module,
                "the two fixture tables drifted out of order; every entry must \
                 describe the same module (#5218)"
            );
            assert!(
                source.contains(&format!("mod {module_name} {{")),
                "{module} no longer declares `mod {module_name}`; renaming it \
                 back puts its database-backed tests into every PG-less lane \
                 (#5218)"
            );
            let test_path = format!("{module}::{module_name}");
            assert!(
                PR_SWEEP_SKIPS.iter().any(|token| test_path.contains(token)),
                "{test_path} matches none of the PR sweep skip tokens \
                 {PR_SWEEP_SKIPS:?} (#5218)"
            );
            assert!(
                NIGHTLY_SKIPS.iter().any(|token| test_path.contains(token)),
                "{test_path} matches none of the nightly skip tokens \
                 {NIGHTLY_SKIPS:?}; a trailing `_pg` is the classic way to pass \
                 the PR sweep and still run on the nightly lanes (#5218)"
            );
        }
    }

    /// This module is the audit that protects the PG-less lanes, so it must be
    /// scheduled into them. If its own path ever picked up a lane token, both
    /// filters would skip it and the contract above would stop being checked
    /// anywhere.
    #[test]
    fn this_audit_module_is_not_itself_skipped_by_either_filter() {
        let test_path = module_path!();
        for token in ["_pg", "pg_", "postgres"] {
            assert!(
                !test_path.contains(token),
                "{test_path} contains `{token}`, so the PR sweep would skip the \
                 very audit that guards the PG-less lanes (#5218)"
            );
        }
    }
}
