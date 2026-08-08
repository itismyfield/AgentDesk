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
    use regex::Regex;
    use std::collections::BTreeSet;
    use std::ops::Range;

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

    /// Assembled at runtime so this file does not itself contain the literal it
    /// forbids; a plain grep for the address stays a reliable audit.
    fn forbidden_address() -> String {
        format!("{}:{}", "127.0.0.1", "5432")
    }

    /// The shared helper the four fixtures now depend on.
    const SHARED_HELPER_SOURCE: &str = include_str!("../db/postgres.rs");
    const TARGET_RULES_SOURCE: &str = include_str!("../db/fixture_target.rs");

    fn masked_rust_source(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut output = bytes.to_vec();
        let mut index = 0;
        while index < bytes.len() {
            let raw_prefix = if bytes[index] == b'r' {
                Some(index + 1)
            } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'r') {
                Some(index + 2)
            } else {
                None
            };
            if let Some(mut quote) = raw_prefix {
                while bytes.get(quote) == Some(&b'#') {
                    quote += 1;
                }
                if bytes.get(quote) == Some(&b'"') {
                    let hashes = quote - raw_prefix.unwrap();
                    let start = index;
                    index = quote + 1;
                    while index < bytes.len() {
                        if bytes[index] == b'"'
                            && bytes.get(index + 1..index + 1 + hashes)
                                == Some(&vec![b'#'; hashes][..])
                        {
                            index += 1 + hashes;
                            break;
                        }
                        index += 1;
                    }
                    for offset in start..index {
                        if output[offset] != b'\n' {
                            output[offset] = b' ';
                        }
                    }
                    continue;
                }
            }
            if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
                let start = index;
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                output[start..index].fill(b' ');
                continue;
            }
            if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                let start = index;
                index += 2;
                let mut depth = 1;
                while index < bytes.len() && depth > 0 {
                    if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                        depth += 1;
                        index += 2;
                    } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                for offset in start..index {
                    if output[offset] != b'\n' {
                        output[offset] = b' ';
                    }
                }
                continue;
            }
            let quote = if bytes[index] == b'"' {
                Some(index)
            } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"') {
                Some(index + 1)
            } else {
                None
            };
            if let Some(quote) = quote {
                let start = index;
                index = quote + 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else {
                        let end = bytes[index] == b'"';
                        index += 1;
                        if end {
                            break;
                        }
                    }
                }
                for offset in start..index {
                    if output[offset] != b'\n' {
                        output[offset] = b' ';
                    }
                }
                continue;
            }
            let char_start = if bytes[index] == b'\'' {
                Some(index)
            } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'\'') {
                Some(index + 1)
            } else {
                None
            };
            if let Some(quote) = char_start {
                let closing = if bytes.get(quote + 1) == Some(&b'\\') {
                    quote + 3
                } else {
                    quote + 2
                };
                if bytes.get(closing) == Some(&b'\'') {
                    output[index..=closing].fill(b' ');
                    index = closing + 1;
                    continue;
                }
            }
            index += 1;
        }
        String::from_utf8(output).expect("masking Rust source preserves UTF-8")
    }

    fn function_range(source: &str, function_name: &str) -> Range<usize> {
        let masked = masked_rust_source(source);
        let signature = Regex::new(&format!(r"\bfn\s+{}\s*\(", regex::escape(function_name)))
            .expect("function signature regex");
        let start = signature
            .find(&masked)
            .unwrap_or_else(|| panic!("missing function {function_name}"))
            .start();
        let open = masked[start..]
            .find('{')
            .map(|offset| start + offset)
            .unwrap_or_else(|| panic!("missing body for {function_name}"));
        let mut depth = 0_u32;
        for (offset, byte) in masked.as_bytes()[open..].iter().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return start..open + offset + 1;
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated body for {function_name}")
    }

    fn function_range_after(source: &str, anchor: &str, function_name: &str) -> Range<usize> {
        let offset = source
            .find(anchor)
            .unwrap_or_else(|| panic!("missing {anchor}"));
        let range = function_range(&source[offset..], function_name);
        offset + range.start..offset + range.end
    }

    fn own_test_names(source: &str) -> Vec<String> {
        Regex::new(
            r"(?s)#\s*\[\s*(?:tokio::)?test(?:\s*\([^]]*\))?\s*\]\s*(?:#\s*\[[^]]*\]\s*)*(?:async\s+)?fn\s+([A-Za-z0-9_]+)",
        )
        .expect("test declaration regex")
        .captures_iter(source)
        .map(|capture| capture[1].to_string())
        .collect()
    }

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

    #[test]
    fn target_guard_syntactically_dominates_pool_construction() {
        let range = function_range(SHARED_HELPER_SOURCE, "connect_test_pool_with_options");
        let body = &masked_rust_source(SHARED_HELPER_SOURCE)[range];
        let guard = body
            .find("super::fixture_target::enforce_configured_target(")
            .expect("test pool funnel must enforce its configured target");
        let pool = body
            .find("PgPoolOptions::new(")
            .expect("test pool funnel must construct its pool");
        assert!(
            guard < pool,
            "the fixture target guard must be textually before pool construction (#5229)"
        );

        let range = function_range_after(SHARED_HELPER_SOURCE, "struct TestDatabase", "create");
        let body = &masked_rust_source(SHARED_HELPER_SOURCE)[range];
        let preflight = body
            .find("config_base_is_representable(&parsed_base)")
            .expect("Config fixture creation must preflight its base");
        let create = body
            .find("create_test_database(")
            .expect("Config fixture creation must call the shared CREATE helper");
        assert!(
            preflight < create,
            "Config base representability must be checked before CREATE (#5229)"
        );
    }

    #[test]
    fn every_direct_connection_constructor_has_a_registered_owner() {
        let masked = masked_rust_source(SHARED_HELPER_SOURCE);
        let owners = [
            "connect_with_settings_typed",
            "pool_options",
            "connect_test_pool_with_options",
            "try_acquire_with_application_name",
        ]
        .map(|name| (name, function_range(SHARED_HELPER_SOURCE, name)));
        for needle in ["PgPoolOptions::new(", "PgConnection::connect"] {
            for (offset, _) in masked.match_indices(needle) {
                let matching: Vec<_> = owners
                    .iter()
                    .filter(|(_, range)| range.contains(&offset))
                    .map(|(name, _)| *name)
                    .collect();
                assert_eq!(
                    matching.len(),
                    1,
                    "direct connection constructor `{needle}` at byte {offset} must belong to exactly one registered function; owners={matching:?} (#5229)"
                );
            }
        }
    }

    #[test]
    fn shared_target_entrypoint_set_is_exact() {
        let declaration = Regex::new(
            r"(?s)#\[cfg\(test\)\](?:(?:\s*#\[[^]]+\])|(?:\s*///[^\n]*\n)|(?:\s*//[^\n]*\n))*\s*pub\(crate\)\s+async\s+fn\s+([A-Za-z0-9_]+)\s*\((.*?)\)",
        )
        .expect("shared entrypoint declaration regex");
        let actual: BTreeSet<_> = declaration
            .captures_iter(SHARED_HELPER_SOURCE)
            .filter(|capture| {
                let arguments = &capture[2];
                arguments.contains("database_url:")
                    || arguments.contains("admin_url:")
                    || arguments.contains("config: &Config")
            })
            .map(|capture| capture[1].to_string())
            .collect();
        let expected = BTreeSet::from([
            "connect_test_pool".to_string(),
            "connect_test_pool_and_migrate".to_string(),
            "connect_test_pool_and_migrate_config".to_string(),
            "connect_test_pool_with_max_connections".to_string(),
            "connect_test_pool_with_max_connections_and_migrate".to_string(),
            "create_test_database".to_string(),
            "drop_test_database".to_string(),
        ]);
        assert_eq!(
            actual, expected,
            "shared fixture entrypoint set drifted (#5229)"
        );
    }

    #[test]
    fn override_and_config_derivation_sources_stay_single() {
        assert_eq!(
            SHARED_HELPER_SOURCE
                .matches("slot.replace(Some(fixture_base.map(str::to_string)))")
                .count(),
            1,
            "the thread-local fixture-base override must have exactly one setter (#5229)"
        );
        let range = function_range(SHARED_HELPER_SOURCE, "test_database_config_options");
        let body = &masked_rust_source(SHARED_HELPER_SOURCE)[range];
        for fragment in [
            ".host(&config.database.host)",
            ".port(config.database.port)",
        ] {
            assert!(
                body.contains(fragment),
                "Config option derivation lost `{fragment}`, reviving ambient endpoint input (#5229)"
            );
        }
    }

    #[test]
    fn endpoint_enumeration_is_pinned_to_the_reviewed_dependency_version() {
        let lock = include_str!("../../Cargo.lock");
        let package =
            Regex::new(r#"(?ms)^\[\[package\]\]\nname = "sqlx-postgres"\nversion = "([^"]+)""#)
                .expect("lockfile package regex")
                .captures(lock)
                .expect("Cargo.lock must contain sqlx-postgres");
        assert_eq!(
            &package[1], "0.8.6",
            "endpoint input enumeration was derived from sqlx-postgres 0.8.6 PgStream::connect and its TLS path; when upgrading, reread connection/stream.rs, options/parse.rs, and the TLS implementation, then update the enumeration (#5229)"
        );
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
    fn every_audited_test_id_is_free_of_lane_skip_tokens() {
        for (module, source) in [
            (
                "server::database_fixture_invariant_tests::tests",
                include_str!("database_fixture_invariant_tests.rs"),
            ),
            ("db::fixture_target::tests", TARGET_RULES_SOURCE),
        ] {
            for name in own_test_names(source) {
                let test_id = format!("{module}::{name}");
                for token in ["_pg", "pg_", "postgres", "_pg_", "postgres_"] {
                    assert!(
                        !test_id.contains(token),
                        "{test_id} contains lane skip token `{token}` and would evade an intended database-less audit lane (#5229)"
                    );
                }
            }
        }
    }
}
