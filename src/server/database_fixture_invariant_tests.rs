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
//! It must stay free of the fixture seed identifiers `scripts/ci/pg_test_classifier.py` scans for, or
//! the native non-PG runs it is meant to protect would leave it out as a database-dependent test.

#[cfg(test)]
mod tests {
    use crate::test_support::{rust_code_mask, strip_rust_comments};

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

    /// Fixture sources that select the shared base directly, beyond the four
    /// cluster fixtures above. The bounded constructor inventory below checks
    /// both lists against direct calls in `src/**/*.rs`, including the real
    /// fixture in `db::postgres`; that file is not merely a helper definition.
    /// `engine::ops::config_ops` delegates base selection through the listed
    /// `dispatch::test_support` and is an explicit exception to direct base-call checks only.
    /// Its source still participates in the address/environment token checks. Target-parser and tunnel contract
    /// tests have no direct constructor calls and need no inventory exception.
    const PG_FIXTURE_SOURCES: &[(&str, &str)] = &[
        (
            "db::auto_queue::test_support",
            include_str!("../db/auto_queue/test_support.rs"),
        ),
        (
            "db::dispatched_sessions",
            include_str!("../db/dispatched_sessions.rs"),
        ),
        (
            "db::dispatched_sessions::canonical_identity_pg_tests",
            include_str!("../db/dispatched_sessions/canonical_identity_pg_tests.rs"),
        ),
        (
            "db::dispatches::delivery_events",
            include_str!("../db/dispatches/delivery_events.rs"),
        ),
        ("db::postgres", include_str!("../db/postgres.rs")),
        (
            "db::postgres::advisory_lock_tests",
            include_str!("../db/postgres/advisory_lock_tests.rs"),
        ),
        (
            "db::postgres::test_db_reclaim",
            include_str!("../db/postgres/test_db_reclaim.rs"),
        ),
        (
            "db::prompt_manifests::tests",
            include_str!("../db/prompt_manifests/tests.rs"),
        ),
        (
            "dispatch::test_support",
            include_str!("../dispatch/test_support.rs"),
        ),
        (
            "engine::ops::db_ops",
            include_str!("../engine/ops/db_ops.rs"),
        ),
        (
            "high_risk_recovery",
            include_str!("../high_risk_recovery.rs"),
        ),
        ("reconcile", include_str!("../reconcile.rs")),
        (
            "server::routes::dispatches::crud",
            include_str!("routes/dispatches/crud.rs"),
        ),
        (
            "server::routes::escalation",
            include_str!("routes/escalation.rs"),
        ),
        (
            "server::routes::memory_api",
            include_str!("routes/memory_api.rs"),
        ),
        ("server::routes::stats", include_str!("routes/stats.rs")),
        (
            "services::discord",
            include_str!("../services/discord/mod.rs"),
        ),
        (
            "services::discord::runtime_bootstrap::gateway_handback_integration_tests",
            include_str!(
                "../services/discord/runtime_bootstrap/gateway_handback_integration_tests.rs"
            ),
        ),
        (
            "services::discord::runtime_bootstrap::gateway_lease_recovery_tests",
            include_str!("../services/discord/runtime_bootstrap/gateway_lease_recovery_tests.rs"),
        ),
        (
            "services::dispatches::outbox_claiming",
            include_str!("../services/dispatches/outbox_claiming.rs"),
        ),
        (
            "services::dispatches::wait_queue",
            include_str!("../services/dispatches/wait_queue.rs"),
        ),
        (
            "services::observability::recovery_audit",
            include_str!("../services/observability/recovery_audit.rs"),
        ),
        (
            "services::observability::turn_lifecycle",
            include_str!("../services/observability/turn_lifecycle.rs"),
        ),
        (
            "services::pipeline_override",
            include_str!("../services/pipeline_override.rs"),
        ),
        (
            "services::settings",
            include_str!("../services/settings.rs"),
        ),
    ];

    const DELEGATED_FIXTURE_SOURCE: (&str, &str) = (
        "engine::ops::config_ops",
        include_str!("../engine/ops/config_ops.rs"),
    );

    fn address_checked_sources() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
        PG_FIXTURE_SOURCES
            .iter()
            .chain(std::iter::once(&DELEGATED_FIXTURE_SOURCE))
    }

    /// Assembled at runtime so this file does not itself contain the literal it
    /// forbids; a plain grep for the address stays a reliable audit.
    fn forbidden_address() -> String {
        format!("{}:{}", "127.0.0.1", "5432")
    }

    /// Classification 2: comment-free text (literals kept, so a quoted address still counts);
    /// required calls use the code mask. Lexical tripwire: no aliases, macros or reachability.
    fn code_text(source: &str) -> String {
        strip_rust_comments(source).unwrap_or_else(|error| panic!("fixture source: {error}"))
    }

    fn code_mask(source: &str) -> String {
        rust_code_mask(source).unwrap_or_else(|error| panic!("fixture source: {error}"))
    }

    const HOST_PORT_KEYS: [&str; 2] = ["PGHOST", "PGPORT"];

    fn names_forbidden_address(source: &str) -> bool {
        code_text(source).contains(&forbidden_address())
    }

    fn host_port_keys_read(source: &str) -> Vec<&'static str> {
        let code = code_text(source);
        HOST_PORT_KEYS
            .into_iter()
            .filter(|key| code.contains(key))
            .collect()
    }

    fn calls_shared_base(source: &str) -> bool {
        code_mask(source).contains("postgres_test_database_url_base()")
    }

    #[test]
    fn cluster_fixtures_never_hardcode_a_database_server_address() {
        let needle = forbidden_address();
        for (module, source) in FIXTURE_SOURCES {
            assert!(
                !names_forbidden_address(source),
                "{module} hardcodes {needle}; a fixture must never name a server \
                 the lane did not configure (#5218)"
            );
        }
    }

    #[test]
    fn fixture_sources_never_hardcode_a_database_server_address() {
        let needle = forbidden_address();
        for (module, source) in address_checked_sources() {
            assert!(
                !names_forbidden_address(source),
                "{module} hardcodes {needle}; fixture addresses must come from the lane (#5510)"
            );
        }
    }

    /// Lexical code tokens only: comments (including nested blocks), ordinary
    /// strings, raw strings and character literals cannot register a caller.
    fn fixture_code_tokens(source: &str) -> Vec<&str> {
        static LEXEMES: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let lexemes = LEXEMES.get_or_init(|| {
            regex::Regex::new(
                r#"(?s)//[^\n]*|/\*|(?:br|cr|r)(#*)"|(?:b|c)?"(?:\\.|[^"\\])*"|b?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\])'|(?:r#)?[A-Za-z_][A-Za-z_0-9]*|[^\s]"#,
            )
            .expect("fixture source lexer")
        });
        let mut tokens = Vec::new();
        let mut offset = 0;
        while let Some(parts) = lexemes.captures(&source[offset..]) {
            let token = parts.get(0).expect("matched lexeme");
            offset += token.end();
            let token = token.as_str();
            if token == "/*" {
                let mut depth = 1;
                while depth > 0 {
                    let rest = &source[offset..];
                    assert!(!rest.is_empty(), "unterminated source comment");
                    if rest.starts_with("/*") {
                        depth += 1;
                        offset += 2;
                    } else if rest.starts_with("*/") {
                        depth -= 1;
                        offset += 2;
                    } else {
                        offset += rest.chars().next().expect("comment character").len_utf8();
                    }
                }
            } else if let Some(hashes) = parts.get(1) {
                let end = format!("\"{}", hashes.as_str());
                offset += source[offset..]
                    .find(&end)
                    .expect("unterminated raw source string")
                    + end.len();
            } else if !token.starts_with("//")
                && !token.contains('"')
                && !token.starts_with('\'')
                && !token.starts_with("b'")
            {
                tokens.push(token.strip_prefix("r#").unwrap_or(token));
            }
        }
        tokens
    }

    fn has_named_fixture_call(source: &str, name: &str) -> bool {
        if !source.contains(name) {
            return false;
        }
        let tokens = fixture_code_tokens(source);
        tokens
            .windows(2)
            .enumerate()
            .any(|(index, pair)| pair == [name, "("] && (index == 0 || tokens[index - 1] != "fn"))
    }

    /// This is a bounded lexical inventory, not a Rust call graph: it checks
    /// regular files under `src`, without following symlinks or resolving
    /// aliases, generated code, macro expansion or generic call syntax. A
    /// direct named invocation in macro input is conservatively a caller.
    #[test]
    fn fixture_constructor_sources_are_registered() {
        // Assemble the name so the lane classifier does not mark this audit
        // as database-dependent from a seed identifier in its own source.
        let constructor = ["create", "test", "database"].join("_");
        let delegated = fixture_code_tokens(DELEGATED_FIXTURE_SOURCE.1);
        for helper in ["postgres_base_database_url", "postgres_admin_database_url"] {
            let qualified = [
                "crate",
                ":",
                ":",
                "dispatch",
                ":",
                ":",
                "test_support",
                ":",
                ":",
                helper,
                "(",
            ];
            assert!(
                delegated
                    .windows(qualified.len())
                    .any(|call| call == qualified),
                "config_ops must keep delegating to dispatch::test_support::{helper}"
            );
        }
        let expected: std::collections::BTreeSet<_> = FIXTURE_SOURCES
            .iter()
            .chain(PG_FIXTURE_SOURCES)
            .map(|(module, _)| module.to_string())
            .chain(std::iter::once(DELEGATED_FIXTURE_SOURCE.0.to_string()))
            .collect();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut pending = vec![root.clone()];
        let mut actual = std::collections::BTreeSet::new();
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).expect("read fixture source directory") {
                let entry = entry.expect("fixture source entry");
                let kind = entry.file_type().expect("fixture source type");
                let path = entry.path();
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                    let source = std::fs::read_to_string(&path).expect("read fixture source");
                    // Inventory labels encode physical paths (with mod.rs collapsed),
                    // not Rust's module graph: #[path] can give a file another name.
                    let relative = path.strip_prefix(&root).expect("source below root");
                    let mut module = relative.with_extension("");
                    if module.file_name().is_some_and(|name| name == "mod") {
                        module.pop();
                    }
                    let module = module
                        .iter()
                        .map(|part| part.to_str().expect("UTF-8 module"))
                        .collect::<Vec<_>>()
                        .join("::");
                    for (label, included) in FIXTURE_SOURCES.iter().chain(address_checked_sources())
                    {
                        if *label == module {
                            assert!(
                                *included == source,
                                "{label} include_str source differs from physical file {} (#5756)",
                                relative.display()
                            );
                        }
                    }
                    if has_named_fixture_call(&source, &constructor) {
                        actual.insert(module);
                    }
                }
            }
        }
        assert_eq!(
            actual, expected,
            "fixture constructor inventory drift (#5510)"
        );
    }

    /// The guard is deliberately lexical. It rejects every non-comment
    /// `PGHOST`/`PGPORT` token in the enumerated fixture sources, which include all
    /// of `db::postgres` and its shared base helper, so changing `var` to `var_os`, hiding the read
    /// behind a constant, or reformatting the fallback cannot evade it. It
    /// does not claim to cover a fixture file omitted from `PG_FIXTURE_SOURCES`,
    /// an imported helper in another file, a fallback using a different
    /// environment key or literal address, or an environment key assembled
    /// dynamically (for example, `format!("PG{}", "HOST")`); those require
    /// inventory review and a behavioural test rather than a source-token
    /// assertion.
    #[test]
    fn fixture_sources_never_read_process_host_or_port() {
        for (module, source) in address_checked_sources() {
            let read = host_port_keys_read(source);
            assert!(
                read.is_empty(),
                "{module} contains {read:?}; fixture URLs must come only from \
                 db::postgres::postgres_test_database_url_base() (#5229)"
            );
        }
    }

    #[test]
    fn fixture_sources_use_the_shared_base_authority() {
        for (module, source) in PG_FIXTURE_SOURCES {
            assert!(
                calls_shared_base(source),
                "{module} no longer calls the shared fixture-base authority (#5229)"
            );
        }
    }

    #[test]
    fn cluster_fixtures_have_no_private_base_url_builder() {
        for (module, source) in FIXTURE_SOURCES {
            assert!(
                !code_text(source).contains("fn postgres_base_database_url"),
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
                calls_shared_base(source),
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
            let found = base_substitutions(source);
            assert!(
                found.is_empty(),
                "{module} contains {found:?}; a missing fixture base must \
                 stay missing, not be substituted for (#5218)"
            );
        }
    }

    fn base_substitutions(source: &str) -> Vec<&'static str> {
        // Match against whitespace-stripped source: rustfmt puts a method
        // chain on its own line as soon as it grows, and an assertion that
        // only sees the single-line spelling would miss the reformatted one.
        let dense: String = code_text(source)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        [
            "postgres_test_database_url_base().unwrap",
            "postgres_test_database_url_base().unwrap_or",
            "postgres_test_database_url_base().unwrap_or_else",
            "postgres_test_database_url_base().unwrap_or_default",
            "PGUSER",
        ]
        .into_iter()
        .filter(|banned| dense.contains(banned))
        .collect()
    }
}
