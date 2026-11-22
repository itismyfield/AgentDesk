use super::{
    AGENTDESK_REQUIRE_PG_ENV, AdvisoryLockLease, POSTGRES_MIGRATOR,
    STARTUP_PG_ACQUIRE_TIMEOUT_SECS, bootstrap_pool_settings, checksum_hex,
    clamp_foreground_reserve, close_test_pool, config_database_summary, connect_options,
    connect_test_pool, connect_test_pool_and_migrate, connect_test_pool_and_migrate_config,
    connect_test_pool_with_max_connections, connect_test_pool_with_max_connections_and_migrate,
    create_test_database, database_enabled, database_summary, health_check, require_pg_guard,
    run_test_postgres_sqlx_op_with_timeout, runtime_pool_settings, should_yield_for_counters,
    startup_pool_settings, sync_agents_from_config_pg, with_startup_advisory_lock,
};
use crate::services::settings::startup::{shared_config_sync_enabled, startup_reseed};
use crate::test_support::wait::{WAIT_CAP, join_within};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Executor as _, PgPool, Row};
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// Contract: the three fixtures wired through
/// `postgres_test_database_url_base()` take the shared lifecycle lock for
/// their create/connect/drop sequence. The `just test-postgres` recipe is
/// the serial lane (`--test-threads=1`); the `test_fast` Terminal delivery
/// evidence step has no skip flag, but its narrow positive filters do not
/// select these guard tests. The lock plus lane selection makes this
/// temporary process environment override effective; this guard is not a
/// substitute for either prerequisite.
struct RequirePgEnvGuard {
    _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
    _lifecycle: super::PostgresTestLifecycleGuard,
    previous: Option<std::ffi::OsString>,
    previous_acquire_timeout: Option<std::ffi::OsString>,
    previous_fixture_base_override: Option<Option<String>>,
}

impl RequirePgEnvGuard {
    fn set(value: Option<&str>, fixture_base: Option<&str>) -> Self {
        let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let lifecycle = super::lock_test_lifecycle();
        let previous = std::env::var_os(AGENTDESK_REQUIRE_PG_ENV);
        let previous_acquire_timeout = std::env::var_os(super::TEST_POSTGRES_ACQUIRE_TIMEOUT_ENV);
        let previous_fixture_base_override = super::FIXTURE_BASE_OVERRIDE
            .with(|slot| slot.replace(Some(fixture_base.map(str::to_string))));
        match value {
            Some(value) => unsafe { std::env::set_var(AGENTDESK_REQUIRE_PG_ENV, value) },
            None => unsafe { std::env::remove_var(AGENTDESK_REQUIRE_PG_ENV) },
        }
        // SQLx retries connection-refused errors until the pool's acquire
        // timeout; keep this test-only failure mode bounded.
        unsafe { std::env::set_var(super::TEST_POSTGRES_ACQUIRE_TIMEOUT_ENV, "250") };
        Self {
            _env_lock: env_lock,
            _lifecycle: lifecycle,
            previous,
            previous_acquire_timeout,
            previous_fixture_base_override,
        }
    }
}

impl Drop for RequirePgEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(AGENTDESK_REQUIRE_PG_ENV, value) },
            None => unsafe { std::env::remove_var(AGENTDESK_REQUIRE_PG_ENV) },
        }
        match self.previous_acquire_timeout.take() {
            Some(value) => unsafe {
                std::env::set_var(super::TEST_POSTGRES_ACQUIRE_TIMEOUT_ENV, value)
            },
            None => unsafe { std::env::remove_var(super::TEST_POSTGRES_ACQUIRE_TIMEOUT_ENV) },
        }
        super::FIXTURE_BASE_OVERRIDE.with(|slot| {
            slot.replace(self.previous_fixture_base_override.take());
        });
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    "non-string panic payload".to_string()
}

fn block_on_test_runtime<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres guard test")
        .block_on(future)
}

fn assert_required_pg_panic<T, F>(future: F)
where
    T: std::fmt::Debug,
    F: Future<Output = Result<T, String>>,
{
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on_test_runtime(future)
    }))
    .expect_err(
        "required PG lane must panic on a PostgreSQL error; this probe assumes no PostgreSQL listener on 127.0.0.1:1",
    );
    let message = panic_message(panic);
    assert!(
        message.contains("AGENTDESK_REQUIRE_PG=1"),
        "panic must identify the require env: {message}"
    );
    assert!(
        message.contains("unavailable:"),
        "panic must retain the original helper error after its context: {message}"
    );
    let lower_message = message.to_ascii_lowercase();
    assert!(
        lower_message.contains("pooltimedout")
            || lower_message.contains("timed out")
            || lower_message.contains("timeout"),
        "panic must retain the deterministic timeout failure reason (the 127.0.0.1:1 no-listener premise): {message}"
    );
}

fn assert_soft_skip_error<T, F>(future: F)
where
    T: std::fmt::Debug,
    F: Future<Output = Result<T, String>>,
{
    let error = block_on_test_runtime(future).expect_err(
        "PG-less test helper must preserve its connection error; this probe assumes no PostgreSQL listener on 127.0.0.1:1",
    );
    let lower_error = error.to_ascii_lowercase();
    assert!(
        lower_error.contains("pooltimedout")
            || lower_error.contains("timed out")
            || lower_error.contains("timeout"),
        "PG-less test helper must preserve the deterministic timeout failure (the 127.0.0.1:1 no-listener premise): {error}"
    );
}

fn unreachable_postgres_config() -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.database.enabled = true;
    config.database.host = "127.0.0.1".to_string();
    // The guard probes assume privileged loopback port 1 has no listener.
    config.database.port = 1;
    config.database.user = "postgres".to_string();
    config.database.dbname = "postgres".to_string();
    config
}

#[test]
fn require_pg_guard_panics_for_all_entrypoints_on_unreachable_database() {
    let _env = RequirePgEnvGuard::set(Some("1"), Some(UNREACHABLE_POSTGRES_URL));
    // Environment premise: CI runners and developer machines do not run a
    // PostgreSQL listener on privileged loopback port 1. If that premise
    // is violated, the assertions above must identify it as the cause.
    const UNREACHABLE_POSTGRES_URL: &str = "postgresql://postgres@127.0.0.1:1/postgres";

    assert_required_pg_panic(connect_test_pool_with_max_connections(
        UNREACHABLE_POSTGRES_URL,
        "guard max",
        1,
    ));
    assert_required_pg_panic(connect_test_pool(UNREACHABLE_POSTGRES_URL, "guard default"));
    assert_required_pg_panic(create_test_database(
        UNREACHABLE_POSTGRES_URL,
        "guard_db",
        "guard create",
    ));
    assert_required_pg_panic(connect_test_pool_and_migrate(
        UNREACHABLE_POSTGRES_URL,
        "guard migrate",
    ));
    assert_required_pg_panic(connect_test_pool_with_max_connections_and_migrate(
        UNREACHABLE_POSTGRES_URL,
        "guard max migrate",
        1,
    ));
    assert_required_pg_panic(connect_test_pool_and_migrate_config(
        &unreachable_postgres_config(),
        "guard config",
    ));
}

#[test]
fn require_pg_guard_preserves_errors_when_ci_lane_does_not_require_postgres() {
    let _env = RequirePgEnvGuard::set(None, Some(UNREACHABLE_POSTGRES_URL));
    // Keep the same explicit no-listener premise as the required-lane
    // probe; a live port-1 service would invalidate the timeout contract.
    const UNREACHABLE_POSTGRES_URL: &str = "postgresql://postgres@127.0.0.1:1/postgres";

    assert_soft_skip_error(connect_test_pool_with_max_connections(
        UNREACHABLE_POSTGRES_URL,
        "soft max",
        1,
    ));
    assert_soft_skip_error(connect_test_pool(UNREACHABLE_POSTGRES_URL, "soft default"));
    assert_soft_skip_error(create_test_database(
        UNREACHABLE_POSTGRES_URL,
        "soft_db",
        "soft create",
    ));
    assert_soft_skip_error(connect_test_pool_and_migrate(
        UNREACHABLE_POSTGRES_URL,
        "soft migrate",
    ));
    assert_soft_skip_error(connect_test_pool_with_max_connections_and_migrate(
        UNREACHABLE_POSTGRES_URL,
        "soft max migrate",
        1,
    ));
    assert_soft_skip_error(connect_test_pool_and_migrate_config(
        &unreachable_postgres_config(),
        "soft config",
    ));
}

#[test]
fn require_pg_guard_leaves_success_untouched_when_ci_lane_requires_postgres() {
    let _env = RequirePgEnvGuard::set(Some("1"), None);
    assert_eq!(require_pg_guard(Ok::<_, String>(42)), Ok(42));
    let config = crate::config::Config::default();
    assert!(matches!(
        block_on_test_runtime(connect_test_pool_and_migrate_config(
            &config,
            "guard disabled config",
        )),
        Ok(None)
    ));
}

/// Fixtures skip when no base is configured; required lanes must turn that into a failure.
#[test]
fn missing_fixture_base_panics_only_when_the_lane_requires_postgres() {
    {
        let _env = RequirePgEnvGuard::set(Some("1"), None);
        let panic = std::panic::catch_unwind(super::postgres_test_database_url_base)
            .expect_err("a required lane must fail on a missing fixture base");
        let message = panic_message(panic);
        assert!(
            message.contains("POSTGRES_TEST_DATABASE_URL_BASE unset"),
            "panic must name the missing base: {message}"
        );
    }
    let _env = RequirePgEnvGuard::set(None, None);
    assert_eq!(super::postgres_test_database_url_base(), None);
}

struct TestDatabase {
    admin_url: String,
    database_name: String,
    database_url: String,
    base_options: PgConnectOptions,
    base_password: Option<String>,
    cleanup_armed: bool,
}

impl TestDatabase {
    async fn create() -> Self {
        let base = super::postgres_test_database_url_base().unwrap_or_else(|| {
            panic!(
                "db::postgres Config fixtures require POSTGRES_TEST_DATABASE_URL_BASE before CREATE"
            )
        });
        let parsed_base = crate::db::fixture_target::parse_fixture_url(
            &base,
            "db::postgres configured fixture base",
        )
        .unwrap_or_else(|error| {
            panic!("db::postgres fixture base is invalid before CREATE: {error}")
        });
        crate::db::fixture_target::config_base_is_representable(&parsed_base).unwrap_or_else(
            |error| {
                panic!(
                    "db::postgres Config fixtures cannot represent the configured base before CREATE: {error}"
                )
            },
        );
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let database_name = format!("agentdesk_pg_{}", uuid::Uuid::new_v4().simple());
        let database_url_for = |database: &str| {
            let mut url = parsed_base.url.clone();
            url.set_path(&format!("/{database}"));
            url.to_string()
        };
        let admin_url = database_url_for(&admin_db);
        let database_url = database_url_for(&database_name);
        create_test_database(&admin_url, &database_name, "db::postgres tests")
            .await
            .expect("create postgres test db");

        Self {
            admin_url,
            database_name,
            database_url,
            base_options: parsed_base.options,
            base_password: parsed_base.password,
            cleanup_armed: true,
        }
    }

    async fn drop(mut self) {
        let result =
            super::drop_test_database(&self.admin_url, &self.database_name, "db::postgres tests")
                .await;
        if result.is_ok() {
            self.cleanup_armed = false;
        }
        result.expect("drop postgres test db");
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        let result =
            cleanup_test_database_from_drop(self.admin_url.clone(), self.database_name.clone());
        if let Err(error) = result {
            if std::thread::panicking() {
                eprintln!("LEAKED-TEST-DATABASE {}: {error}", self.database_name);
            } else {
                panic!(
                    "postgres test database {} leaked: {error}",
                    self.database_name
                );
            }
        }
    }
}

fn cleanup_test_database_from_drop(admin_url: String, database_name: String) -> Result<(), String> {
    let worker_name = format!("db fixture cleanup {database_name}");
    let handle = std::thread::Builder::new()
        .name(worker_name)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("cleanup runtime build failed: {error}"))?;
            runtime.block_on(super::drop_test_database(
                &admin_url,
                &database_name,
                "db::postgres tests Drop",
            ))
        })
        .map_err(|error| format!("cleanup worker spawn failed: {error}"))?;
    // Return join failures to Drop so a timeout can be reported during unwinding.
    let result = catch_unwind(AssertUnwindSafe(|| {
        join_within(handle, Duration::from_secs(120))
    }))
    .map_err(|payload| format!("cleanup worker failed: {}", panic_message(payload)))?;
    result.map_err(|error| format!("async cleanup failed: {error}"))
}

async fn migrate_through_version(pool: &PgPool, max_version: i64) -> Result<(), String> {
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;

    let migrator = Migrator {
        migrations: Cow::Owned(
            POSTGRES_MIGRATOR
                .iter()
                .filter(|migration| migration.version <= max_version)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    migrator
        .run(pool)
        .await
        .map_err(|error| format!("run postgres migrations through {max_version}: {error}"))
}

async fn apply_deploy_gate_constraint(pool: &PgPool) -> Result<(), String> {
    let migration = POSTGRES_MIGRATOR
        .iter()
        .find(|migration| migration.version == 100)
        .expect("deploy-gate constraint migration"); // agentdesk-audit: allow-unwrap — embedded migration 0100 is required by this test
    pool.execute(sqlx::AssertSqlSafe(migration.sql.as_ref()))
        .await
        .map(|_| ())
        .map_err(|error| format!("apply deploy-gate constraint migration: {error}"))
}

async fn seed_auto_queue_run(pool: &PgPool, run_id: &str) {
    sqlx::query("INSERT INTO auto_queue_runs (id, status) VALUES ($1, 'generated')")
        .bind(run_id)
        .execute(pool)
        .await
        .expect("seed auto-queue run"); // agentdesk-audit: allow-unwrap — test-only PostgreSQL fixture
}

async fn insert_phase_gate_kind(
    pool: &PgPool,
    entry_id: &str,
    run_id: &str,
    phase_gate_kind: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, status, phase_gate_kind)
             VALUES ($1, $2, 'pending', $3)",
    )
    .bind(entry_id)
    .bind(run_id)
    .bind(phase_gate_kind)
    .execute(pool)
    .await
    .map(|_| ())
}
fn postgres_test_config(test_db: &TestDatabase) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    config.database.enabled = true;
    config.database.pool_max = 4;
    config.database.host = test_db.base_options.get_host().to_string();
    config.database.port = test_db.base_options.get_port();
    config.database.dbname = test_db.database_name.clone();
    config.database.user = test_db.base_options.get_username().to_string();
    config.database.password = test_db.base_password.clone();
    config.github.repos = vec!["itismyfield/AgentDesk".to_string()];
    config.agents = vec![crate::config::AgentDef {
        id: "pg-agent".to_string(),
        name: "PG Agent".to_string(),
        name_ko: Some("피지 에이전트".to_string()),
        provider: "codex".to_string(),
        channels: crate::config::AgentChannels::new()
            .with("codex", crate::config::AgentChannel::from("pg-agent-cdx")),
        peer_keywords: Vec::new(),
        peer_directory: true,
        keywords: Vec::new(),
        department: Some("platform".to_string()),
        avatar_emoji: Some(":gear:".to_string()),
        preferred_intake_node_labels: None,
        memory_group: None,
    }];
    config
}

#[test]
fn ownership_registry_key_matches_between_admin_url_and_config_options() {
    let admin_url = "postgresql://fixture@db.example:15432/postgres";
    let admin_options = super::parse_test_postgres_options(admin_url, "registry-key admin")
        .expect("parse registry-key admin URL");
    let database_name = "agentdesk_pg_key";
    let mut config = crate::config::Config::default();
    config.database.host = admin_options.get_host().to_string();
    config.database.port = admin_options.get_port();
    config.database.dbname = database_name.to_string();
    let database_options = super::test_database_config_options(&config);

    assert_eq!(
        super::test_database_registry_key(&admin_options, database_name),
        super::test_database_registry_key(&database_options, database_name)
    );
}

#[test]
fn checksum_hex_formats_lowercase_byte_pairs() {
    assert_eq!(checksum_hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
}

#[test]
fn checksum_resolution_filters_down_migrations_to_avoid_false_positive() {
    // Regression for #1688: applied_migration_checksum_mismatches() builds
    // a `version → expected_checksum` BTreeMap from POSTGRES_MIGRATOR.iter().
    // sqlx yields ReversibleUp + ReversibleDown for the same version, and
    // the applied checksum stored in `_sqlx_migrations` is the Up checksum
    // (sqlx::run_direct skips Down when applying). Without filtering Down
    // out, a Down checksum would shadow the Up in the BTreeMap and the
    // helper would report a false-positive mismatch on every reversible
    // migration in the tree.
    use sqlx::migrate::MigrationType;

    // Identify any reversible pair currently in the tree. As of #1688 this
    // is at least the agent-quality-daily migration (version 13).
    let mut reversible_versions: Vec<i64> = POSTGRES_MIGRATOR
        .iter()
        .filter(|m| matches!(m.migration_type, MigrationType::ReversibleUp))
        .map(|m| m.version)
        .collect();
    reversible_versions.sort();
    reversible_versions.dedup();

    assert!(
        !reversible_versions.is_empty(),
        "test guards a real bug only when the tree contains at least one reversible migration; \
             if all migrations became `Simple` the filter is no longer load-bearing — adjust this test"
    );

    // Helper using the same filter as the production code (mirrors sqlx's
    // own `Migrator::run_direct`).
    let filtered: BTreeMap<i64, &[u8]> = POSTGRES_MIGRATOR
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
        .map(|migration| (migration.version, migration.checksum.as_ref()))
        .collect();

    for version in reversible_versions {
        let up_checksum = POSTGRES_MIGRATOR
            .iter()
            .find(|m| {
                m.version == version && matches!(m.migration_type, MigrationType::ReversibleUp)
            })
            .map(|m| m.checksum.as_ref())
            .expect("reversible up entry");
        let down_checksum = POSTGRES_MIGRATOR
            .iter()
            .find(|m| {
                m.version == version && matches!(m.migration_type, MigrationType::ReversibleDown)
            })
            .map(|m| m.checksum.as_ref())
            .expect("reversible down entry");
        assert_ne!(
            up_checksum, down_checksum,
            "test only catches the bug when up/down checksums actually differ"
        );

        let resolved = filtered
            .get(&version)
            .copied()
            .expect("filtered map must retain reversible up version");
        assert_eq!(
            resolved, up_checksum,
            "filtered checksum map for version {version} must keep Up, not Down"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_gate_constraint_allows_supported_legacy_values_and_rejects_deploy_gate() {
    let test_db = TestDatabase::create().await;
    let pool = super::connect_test_pool_with_max_connections(
        &test_db.database_url,
        "deploy-gate constraint values",
        4,
    )
    .await
    .expect("connect deploy-gate constraint database");
    super::migrate(&pool)
        .await
        .expect("migrate deploy-gate constraint database");
    seed_auto_queue_run(&pool, "run-values").await;

    for (entry_id, phase_gate_kind) in [
        ("entry-null", None),
        ("entry-blank", Some(" \t\n\r\u{000c}\u{000b}")),
        ("entry-pr-confirm", Some("pr-confirm")),
    ] {
        insert_phase_gate_kind(&pool, entry_id, "run-values", phase_gate_kind)
            .await
            .expect("supported phase-gate kind remains writable");
    }
    for (entry_id, phase_gate_kind) in [
        ("entry-deploy-gate", "deploy-gate"),
        ("entry-deploy-gate-upper", "DEPLOY-GATE"),
        (
            "entry-deploy-gate-whitespace",
            " \t\n\r\u{000c}\u{000b}DePlOy-GaTe\u{000b}\u{000c}\r\n\t ",
        ),
    ] {
        let error = insert_phase_gate_kind(&pool, entry_id, "run-values", Some(phase_gate_kind))
            .await
            .expect_err("deploy-gate variants must be rejected");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.constraint()),
            Some("auto_queue_entries_deploy_gate_unavailable_check")
        );
    }

    pool.close().await;
    test_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_gate_constraint_migration_blocks_existing_deploy_gate_rows() {
    let test_db = TestDatabase::create().await;
    let pool = super::connect_test_pool_with_max_connections(
        &test_db.database_url,
        "deploy-gate existing-row migration",
        4,
    )
    .await
    .expect("connect existing-row migration database");
    migrate_through_version(&pool, 99)
        .await
        .expect("migrate existing-row database through 99");
    seed_auto_queue_run(&pool, "run-existing").await;
    insert_phase_gate_kind(
        &pool,
        "entry-existing-deploy-gate",
        "run-existing",
        Some("deploy-gate"),
    )
    .await
    .expect("seed pre-constraint deploy-gate row");

    let error = apply_deploy_gate_constraint(&pool)
        .await
        .expect_err("existing deploy-gate row must block migration");
    assert!(error.contains("auto_queue_entries_deploy_gate_unavailable_check"));
    let constraint_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
             FROM pg_constraint
             WHERE conname = 'auto_queue_entries_deploy_gate_unavailable_check'",
    )
    .fetch_one(&pool)
    .await
    .expect("count rolled-back constraint");
    assert_eq!(constraint_count, 0);

    pool.close().await;
    test_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_gate_constraint_serializes_concurrent_legacy_writer() {
    let test_db = TestDatabase::create().await;
    let pool = super::connect_test_pool_with_max_connections(
        &test_db.database_url,
        "deploy-gate concurrent migration",
        6,
    )
    .await
    .expect("connect concurrent migration database");
    migrate_through_version(&pool, 99)
        .await
        .expect("migrate concurrent database through 99");
    seed_auto_queue_run(&pool, "run-concurrent").await;

    let mut writer = pool.acquire().await.expect("acquire legacy writer");
    sqlx::query("BEGIN")
        .execute(&mut *writer)
        .await
        .expect("begin legacy writer");
    insert_phase_gate_kind(
        &pool,
        "entry-safe-before-lock",
        "run-concurrent",
        Some("pr-confirm"),
    )
    .await
    .expect("seed safe row before migration");
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, status, phase_gate_kind)
             VALUES ('entry-legacy-writer', 'run-concurrent', 'pending', 'pr-confirm')",
    )
    .execute(&mut *writer)
    .await
    .expect("seed uncommitted legacy writer row");

    let migration_pool = pool.clone();
    let migration_task =
        tokio::spawn(async move { apply_deploy_gate_constraint(&migration_pool).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !migration_task.is_finished(),
        "ALTER TABLE must wait for the concurrent legacy writer"
    );
    sqlx::query("COMMIT")
        .execute(&mut *writer)
        .await
        .expect("commit safe legacy writer");
    migration_task
        .await
        .expect("join constraint migration")
        .expect("apply constraint after writer commit");

    let error = sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, status, phase_gate_kind)
             VALUES ('entry-after-constraint', 'run-concurrent', 'pending', 'deploy-gate')",
    )
    .execute(&mut *writer)
    .await
    .expect_err("legacy writer cannot commit deploy-gate after constraint");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("auto_queue_entries_deploy_gate_unavailable_check")
    );

    drop(writer);
    pool.close().await;
    test_db.drop().await;
}

#[test]
fn postgres_config_is_disabled_by_default() {
    let config = crate::config::Config::default();
    assert!(!database_enabled(&config));
    assert_eq!(database_summary(&config), "disabled");
    assert!(connect_options(&config).unwrap().is_none());
}

#[test]
fn postgres_summary_uses_config_fields_when_enabled() {
    let mut config = crate::config::Config::default();
    config.database.enabled = true;
    config.database.host = "db.internal".to_string();
    config.database.port = 5433;
    config.database.dbname = "agentdesk_dev".to_string();
    config.database.user = "agentdesk_app".to_string();
    config.database.pool_max = 16;
    config.database.foreground_reserve = 5;

    assert_eq!(
        config_database_summary(&config),
        "db.internal:5433/agentdesk_dev user=agentdesk_app pool_max=16 fg_reserve=5"
    );
    assert!(connect_options(&config).unwrap().is_some());
}

#[test]
fn startup_pool_settings_raise_pool_size_and_acquire_timeout() {
    let mut config = crate::config::Config::default();
    config.database.enabled = true;
    config.database.pool_max = 5;

    let settings = startup_pool_settings(&config);

    assert_eq!(settings.max_connections, 8);
    assert_eq!(
        settings.acquire_timeout,
        Duration::from_secs(STARTUP_PG_ACQUIRE_TIMEOUT_SECS)
    );
    assert_eq!(settings.idle_timeout, Duration::from_secs(5 * 60));
    assert_eq!(settings.max_lifetime, Duration::from_secs(30 * 60));
    assert!(settings.test_before_acquire);
}

#[test]
fn bootstrap_migration_pool_has_longer_scoped_deadline() {
    let mut config = crate::config::Config::default();
    config.database.enabled = true;
    config.database.pool_max = 18;
    let settings = bootstrap_pool_settings(&config);

    assert_eq!(settings.max_connections, 18);
    assert_eq!(settings.acquire_timeout, Duration::from_secs(10));
}

#[test]
fn runtime_pool_settings_enable_dead_peer_detection() {
    let mut config = crate::config::Config::default();
    config.database.enabled = true;
    config.database.pool_max = 5;

    let settings = runtime_pool_settings(&config);

    assert_eq!(settings.max_connections, 5);
    assert_eq!(settings.acquire_timeout, Duration::from_secs(10));
    assert_eq!(settings.idle_timeout, Duration::from_secs(5 * 60));
    assert_eq!(settings.max_lifetime, Duration::from_secs(30 * 60));
    assert!(settings.test_before_acquire);
}

#[test]
fn background_backpressure_disabled_when_reserve_zero() {
    // #3651 behaviour-preserving guard: reserve=0 → always false regardless
    // of how saturated the pool looks. This is the no-config / startup-pool
    // / pre-#3651 path.
    assert!(!should_yield_for_counters(0, 18, 0, 18));
    assert!(!should_yield_for_counters(0, 18, 18, 18));
    // Even a fully in-flight pool does not yield when disabled.
    assert!(!should_yield_for_counters(0, 100, 0, 18));
}

#[test]
fn background_backpressure_yields_only_at_or_past_budget() {
    // pool_max=18, reserve=6 → background budget=12.
    let max = 18;
    let reserve = 6;
    // Idle pool (in_flight=0) → never yields.
    assert!(!should_yield_for_counters(reserve, 0, 0, max));
    // size grows but all idle → in_flight=0 → no yield.
    assert!(!should_yield_for_counters(reserve, 18, 18, max));
    // in_flight=11 (< budget 12) → no yield.
    assert!(!should_yield_for_counters(reserve, 11, 0, max));
    // in_flight=12 (== budget) → yield (protect the reserved 6).
    assert!(should_yield_for_counters(reserve, 12, 0, max));
    // in_flight=18 (pool fully checked out) → yield.
    assert!(should_yield_for_counters(reserve, 18, 0, max));
    // Mixed: size=15, idle=4 → in_flight=11 < 12 → no yield.
    assert!(!should_yield_for_counters(reserve, 15, 4, max));
    // Mixed: size=15, idle=3 → in_flight=12 == 12 → yield.
    assert!(should_yield_for_counters(reserve, 15, 3, max));
}

#[test]
fn background_backpressure_saturating_boundaries() {
    // num_idle > size cannot happen in practice, but the saturating
    // subtraction must not panic and must clamp in_flight to 0 → no yield.
    assert!(!should_yield_for_counters(6, 2, 5, 18));
    // reserve >= max_connections → budget saturates to 0 → any in_flight
    // (>= 0) yields, but an idle pool (in_flight=0 >= 0) also yields. This
    // is the pathological "reserve too large" config; it is documented as a
    // misconfiguration risk and handled without panic.
    assert!(should_yield_for_counters(18, 1, 0, 18));
    assert!(should_yield_for_counters(20, 1, 0, 18));
    // reserve == max with a fully-idle pool: in_flight=0 >= budget 0 → yield.
    assert!(should_yield_for_counters(18, 5, 5, 18));
}

#[test]
fn clamp_foreground_reserve_always_leaves_a_background_slot() {
    // Normal: a reserve comfortably below pool_max is unchanged.
    assert_eq!(clamp_foreground_reserve(6, 18), 6);
    // reserve == pool_max → clamp to max-1 so the background budget stays >= 1.
    assert_eq!(clamp_foreground_reserve(6, 6), 5);
    // reserve > pool_max (default 6 against a small pool_max: 4) → clamp to 3.
    // This is the backward-compat regression case codex flagged.
    assert_eq!(clamp_foreground_reserve(6, 4), 3);
    // pool_max 1 → no slot can be reserved → 0 (disables backpressure entirely).
    assert_eq!(clamp_foreground_reserve(6, 1), 0);
    // pool_max 0 is normalised to 1 → reserve 0 (no panic, no infinite yield).
    assert_eq!(clamp_foreground_reserve(6, 0), 0);
    // reserve 0 stays 0 (backpressure disabled / behaviour-preserving).
    assert_eq!(clamp_foreground_reserve(0, 18), 0);
}

#[tokio::test]
async fn postgres_migrations_and_startup_reseed_prepare_empty_database() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool = connect_test_pool_and_migrate_config(&config, "db::postgres migration test pool")
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");
    startup_reseed(&pool, &config)
        .await
        .expect("startup reseed postgres");

    let migration_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .expect("count sqlx migrations");
    assert!(migration_count >= 1);

    let server_port: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind("server_port")
        .fetch_one(&pool)
        .await
        .expect("server_port in kv_meta");
    assert_eq!(server_port, config.server.port.to_string());

    let runtime_config_raw: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind("runtime-config")
        .fetch_one(&pool)
        .await
        .expect("runtime-config in kv_meta");
    let runtime_config: serde_json::Value =
        serde_json::from_str(&runtime_config_raw).expect("parse runtime-config json");
    assert_eq!(
        runtime_config
            .get("maxRetries")
            .and_then(|value| value.as_u64()),
        Some(3)
    );

    let repo_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM github_repos WHERE id = $1")
        .bind("itismyfield/AgentDesk")
        .fetch_one(&pool)
        .await
        .expect("count github_repos");
    assert_eq!(repo_count, 1);

    let pipeline_stage_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pipeline_stages")
        .fetch_one(&pool)
        .await
        .expect("count pipeline_stages");
    assert_eq!(
        pipeline_stage_count, 0,
        "pipeline stages are user-configured; startup must not seed card states into them"
    );

    let agent_row = sqlx::query(
        "SELECT id, provider, discord_channel_cdx
             FROM agents
             WHERE id = $1",
    )
    .bind("pg-agent")
    .fetch_one(&pool)
    .await
    .expect("load seeded postgres agent");
    assert_eq!(agent_row.get::<String, _>("provider"), "codex");
    assert_eq!(
        agent_row.get::<Option<String>, _>("discord_channel_cdx"),
        Some("pg-agent-cdx".to_string())
    );

    close_test_pool(pool, "db::postgres migration test pool")
        .await
        .expect("close postgres pool");
    assert!(test_db.database_url.contains("agentdesk_pg_"));
    test_db.drop().await;
}

#[test]
fn agent_roster_sync_gated_to_leader_or_single_node() {
    // #3692: only a single-node deployment or the configured leader owns the
    // destructive config→DB agent roster sync.
    let mut config = crate::config::Config::default();

    config.cluster.enabled = false; // single-node: always owns the roster
    config.cluster.role = crate::config::ClusterRole::Auto;
    assert!(shared_config_sync_enabled(&config));

    config.cluster.enabled = true;
    for (role, expected) in [
        ("leader", true),
        ("Leader", true),
        ("  leader  ", true),
        ("worker", false),
        ("auto", false),
        ("runner", false),
        ("hub", true),
    ] {
        config.cluster.role = role.parse().unwrap();
        assert_eq!(
            shared_config_sync_enabled(&config),
            expected,
            "cluster.role={role:?}"
        );
    }
}

async fn shared_configuration_snapshot(pool: &PgPool) -> BTreeMap<String, serde_json::Value> {
    let mut snapshot = BTreeMap::new();
    for table in ["kv_meta", "agents", "github_repos", "pipeline_stages"] {
        let sql = format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM {table} t"
        );
        let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(pool)
            .await
            .expect("snapshot shared configuration");
        snapshot.insert(table.to_string(), rows);
    }
    snapshot
}

const RETIRED_ESCALATION_KEY: &str = concat!("escalation-settings", "-override");

#[tokio::test]
async fn postgres_worker_reseed_preserves_all_leader_configuration_and_overrides() {
    let test_db = TestDatabase::create().await;
    let mut leader = postgres_test_config(&test_db);
    leader.cluster.enabled = true;
    leader.cluster.role = crate::config::ClusterRole::Hub;
    let pool = connect_test_pool_and_migrate_config(&leader, "shared config ownership")
        .await
        .expect("migrate")
        .expect("pool");
    startup_reseed(&pool, &leader).await.expect("seed leader");
    for (key, value) in [
        ("runtime-config", r#"{"maxRetries":8}"#),
        (RETIRED_ESCALATION_KEY, r#"{"mode":"pm"}"#),
        ("workspace_root", "/leader/workspaces"),
    ] {
        super::shared_config::upsert_kv_meta(&pool, key, value)
            .await
            .expect("live override");
    }
    let before = shared_configuration_snapshot(&pool).await;
    let mut worker = leader.clone();
    worker.server.port = 12345;
    worker.policies.dir = std::path::PathBuf::from("missing-worker-local-policy-directory");
    worker.github.repos = vec!["worker/local-only".to_string()];
    worker.agents.clear();
    worker.runtime.max_retries = Some(7);
    worker.runtime.reset_overrides_on_restart = true;
    for role in ["runner", "worker", "auto"] {
        worker.cluster.role = role.parse().unwrap();
        with_startup_advisory_lock(&pool, || startup_reseed(&pool, &worker))
            .await
            .expect("read shared configuration");
        assert_eq!(
            shared_configuration_snapshot(&pool).await,
            before,
            "role={role}"
        );
    }

    // A plain leader boot deletes the retired escalation routing row.
    startup_reseed(&pool, &leader).await.expect("leader reseed");
    let retired: i64 = sqlx::query_scalar("SELECT count(*) FROM kv_meta WHERE key = $1")
        .bind(RETIRED_ESCALATION_KEY)
        .fetch_one(&pool)
        .await
        .expect("retired row count");
    assert_eq!(retired, 0);

    // The same reset flag remains effective for the configured owner.
    leader.runtime.reset_overrides_on_restart = true;
    leader.runtime.max_retries = Some(5);
    startup_reseed(&pool, &leader).await.expect("leader reset");
    let raw: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = 'runtime-config'")
        .fetch_one(&pool)
        .await
        .expect("runtime config");
    let runtime: serde_json::Value = serde_json::from_str(&raw).expect("runtime JSON");
    assert_eq!(runtime["maxRetries"], 5);
    close_test_pool(pool, "shared config ownership")
        .await
        .expect("close");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_worker_first_boot_requires_leader_without_seeding_shared_config() {
    let test_db = TestDatabase::create().await;
    let mut config = postgres_test_config(&test_db);
    config.cluster.enabled = true;
    config.cluster.role = crate::config::ClusterRole::Runner;
    let pool = connect_test_pool_and_migrate_config(&config, "worker first boot")
        .await
        .expect("schema migrations still run")
        .expect("pool");
    let before = shared_configuration_snapshot(&pool).await;
    let error = startup_reseed(&pool, &config)
        .await
        .expect_err("leader is absent");
    assert!(error.contains("cluster.role=hub"), "{error}");
    assert_eq!(shared_configuration_snapshot(&pool).await, before);

    let mut leader = config.clone();
    leader.cluster.role = crate::config::ClusterRole::Hub;
    // Both paths use the same startup lock. A worker that wins the race may
    // refuse startup, but it must never become the configuration writer.
    let (leader_result, worker_result) = tokio::join!(
        with_startup_advisory_lock(&pool, || startup_reseed(&pool, &leader)),
        with_startup_advisory_lock(&pool, || startup_reseed(&pool, &config)),
    );
    leader_result.expect("leader initializes shared configuration");
    if let Err(error) = worker_result {
        assert!(error.contains("cluster.role=hub"), "{error}");
    }
    startup_reseed(&pool, &config)
        .await
        .expect("worker retry after leader boot");
    close_test_pool(pool, "worker first boot")
        .await
        .expect("close");
    test_db.drop().await;
}

#[tokio::test]
async fn worker_node_reseed_does_not_clobber_shared_agent_roster() {
    // #3692: a cluster worker/auto node must NOT run the destructive agent
    // sync at boot — doing so would delete leader-owned agents from the
    // shared table and re-add its own, causing roster flip-flop per deploy.
    let test_db = TestDatabase::create().await;
    let mut config = postgres_test_config(&test_db);

    let pool = connect_test_pool_and_migrate_config(
        &config,
        "db::postgres worker-reseed gating test pool",
    )
    .await
    .expect("connect and migrate postgres")
    .expect("postgres pool");

    startup_reseed(&pool, &config).await.expect("leader reseed");
    sqlx::query("DELETE FROM agents WHERE id = 'pg-agent'")
        .execute(&pool)
        .await
        .expect("remove fixture agent");

    // Simulate a leader-owned agent already present in the shared table.
    sqlx::query("INSERT INTO agents (id, name, provider) VALUES ($1, $2, $3)")
        .bind("leader-owned")
        .bind("Leader Owned")
        .bind("claude")
        .execute(&pool)
        .await
        .expect("seed leader-owned agent");

    // This node is a cluster worker: reseed must skip the agent sync.
    config.cluster.enabled = true;
    config.cluster.role = crate::config::ClusterRole::Runner;
    startup_reseed(&pool, &config).await.expect("worker reseed");

    let leader_owned: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agents WHERE id = 'leader-owned'")
            .fetch_one(&pool)
            .await
            .expect("count leader-owned");
    assert_eq!(
        leader_owned, 1,
        "worker reseed must not delete leader-owned agents"
    );
    let own_agent: i64 = sqlx::query_scalar("SELECT count(*) FROM agents WHERE id = 'pg-agent'")
        .fetch_one(&pool)
        .await
        .expect("count pg-agent");
    assert_eq!(
        own_agent, 0,
        "worker reseed must not sync its own config agents into the shared roster"
    );

    close_test_pool(pool, "db::postgres worker-reseed gating test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn sync_agents_writes_and_preserves_preferred_intake_node_labels() {
    // #3667: config-declared node-affinity labels (`Option<Vec<String>>`)
    // are synced to the agents.preferred_intake_node_labels JSONB column:
    //   Some([...]) sets/overwrites, Some([]) clears, None preserves an
    //   out-of-band value (the ch-td DB-only `["mac-book"]` case, absent
    //   from yaml).
    let test_db = TestDatabase::create().await;
    let mut config = postgres_test_config(&test_db);

    let pool =
        connect_test_pool_and_migrate_config(&config, "db::postgres intake-label sync test pool")
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

    let read_labels = |id: &'static str| {
        let pool = pool.clone();
        async move {
            let value: serde_json::Value =
                sqlx::query_scalar("SELECT preferred_intake_node_labels FROM agents WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("read preferred_intake_node_labels");
            value
        }
    };

    // 1. Config declares Some(["mac-mini"]) -> written verbatim on insert.
    config.agents[0].preferred_intake_node_labels = Some(vec!["mac-mini".to_string()]);
    sync_agents_from_config_pg(&pool, &config.agents)
        .await
        .expect("sync with labels");
    assert_eq!(
        read_labels("pg-agent").await,
        serde_json::json!(["mac-mini"])
    );

    // 2. Seed an out-of-band label, then sync with the field ABSENT (None:
    //    agent in yaml but no key). COALESCE($11=NULL, existing) must
    //    preserve the existing value instead of wiping it.
    sqlx::query("UPDATE agents SET preferred_intake_node_labels = $1 WHERE id = $2")
        .bind(serde_json::json!(["mac-book"]))
        .bind("pg-agent")
        .execute(&pool)
        .await
        .expect("seed out-of-band label");
    config.agents[0].preferred_intake_node_labels = None;
    sync_agents_from_config_pg(&pool, &config.agents)
        .await
        .expect("sync with absent field");
    assert_eq!(
        read_labels("pg-agent").await,
        serde_json::json!(["mac-book"]),
        "absent field (None) must not wipe an out-of-band label"
    );

    // 3. A non-empty config list is authoritative again on conflict/update.
    config.agents[0].preferred_intake_node_labels =
        Some(vec!["mac-mini".to_string(), "release".to_string()]);
    sync_agents_from_config_pg(&pool, &config.agents)
        .await
        .expect("sync overwrite labels");
    assert_eq!(
        read_labels("pg-agent").await,
        serde_json::json!(["mac-mini", "release"])
    );

    // 4. Some([]) is an explicit clear, distinct from None.
    config.agents[0].preferred_intake_node_labels = Some(Vec::new());
    sync_agents_from_config_pg(&pool, &config.agents)
        .await
        .expect("sync explicit clear");
    assert_eq!(
        read_labels("pg-agent").await,
        serde_json::json!([]),
        "Some([]) must explicitly clear the label"
    );

    close_test_pool(pool, "db::postgres intake-label sync test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn core_status_constraints_reject_invalid_values_and_allow_valid_states() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool = connect_test_pool_and_migrate_config(&config, "db::postgres status check test pool")
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");

    let agent_error = sqlx::query(
        "INSERT INTO agents (id, name, provider, status)
             VALUES ('agent-bad-status', 'Bad Status', 'claude', 'busy-now')",
    )
    .execute(&pool)
    .await
    .expect_err("unknown agents.status must be rejected");
    assert!(
        agent_error
            .to_string()
            .contains("agents_status_known_check"),
        "expected agents.status CHECK violation, got: {agent_error}"
    );

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status)
             VALUES ('agent-valid-status', 'Valid Status', 'claude', 'working')",
    )
    .execute(&pool)
    .await
    .expect("valid agents.status should insert");

    let kanban_error = sqlx::query(
        "INSERT INTO kanban_cards (id, title, status)
             VALUES ('card-bad-status', 'Bad Status', 'in progress')",
    )
    .execute(&pool)
    .await
    .expect_err("non-slug kanban_cards.status must be rejected");
    assert!(
        kanban_error
            .to_string()
            .contains("kanban_cards_status_slug_check"),
        "expected kanban status CHECK violation, got: {kanban_error}"
    );

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status)
             VALUES ('card-valid-status', 'Valid Status', 'qa_test')",
    )
    .execute(&pool)
    .await
    .expect("valid custom kanban slug should insert");

    close_test_pool(pool, "db::postgres status check test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_json_extract_0058_up_and_down_preserve_array_path_behavior() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool = connect_test_pool_and_migrate_config(&config, "db::postgres json_extract test pool")
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");

    let (array_value, strict_mismatch, malformed_path, volatility): (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = sqlx::query_as(
        "SELECT
                json_extract('{\"items\":[{\"id\":\"first\"}]}'::jsonb, '$.items[0].id'),
                json_extract('{\"phase_gate\":[{\"run_id\":\"run-array\"}]}'::jsonb, '$.phase_gate.run_id'),
                json_extract('{}'::jsonb, '$['),
                (SELECT provolatile::text
                 FROM pg_proc
                 WHERE oid = 'json_extract(jsonb,text)'::regprocedure)",
    )
    .fetch_one(&pool)
    .await
    .expect("query 0058 json_extract up behavior");
    assert_eq!(array_value.as_deref(), Some("first"));
    assert_eq!(strict_mismatch, None);
    assert_eq!(malformed_path, None);
    assert_eq!(volatility, "s");

    POSTGRES_MIGRATOR
        .undo(&pool, 57)
        .await
        .expect("undo 0058 json_extract migration");

    let (
        down_literal_value,
        down_array_value,
        down_lax_value,
        down_malformed_path,
        down_volatility,
    ): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = sqlx::query_as(
        "SELECT
                json_extract('{\"phase_gate\":{\"run_id\":\"run-object\"}}'::jsonb, '$.phase_gate.run_id'),
                json_extract('{\"items\":[{\"id\":\"first\"}]}'::jsonb, '$.items[0].id'),
                json_extract('{\"phase_gate\":[{\"run_id\":\"run-array\"}]}'::jsonb, '$.phase_gate.run_id'),
                json_extract('{}'::jsonb, '$['),
                (SELECT provolatile::text
                 FROM pg_proc
                 WHERE oid = 'json_extract(jsonb,text)'::regprocedure)",
    )
    .fetch_one(&pool)
    .await
    .expect("query 0058 json_extract down behavior");
    // After `down`, json_extract is restored verbatim from
    // 0002_sqlite_compat_functions.sql: literal-key navigation only
    // (the path regex `^\$((\.[A-Za-z0-9_]+)*)$` rejects anything with
    // brackets), no jsonpath operators, and IMMUTABLE volatility.
    // Positive case: pure literal-key navigation still works (this is
    // the legacy 0057-era contract we are restoring).
    assert_eq!(down_literal_value.as_deref(), Some("run-object"));
    // Negative cases: bracket paths and lax auto-unwrapping are gone.
    assert_eq!(down_array_value, None);
    assert_eq!(down_lax_value, None);
    assert_eq!(down_malformed_path, None);
    assert_eq!(down_volatility, "i");

    close_test_pool(pool, "db::postgres json_extract test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

/// #2224 follow-up: the migration 0058 fix replaced `WHEN OTHERS` with
/// an explicit list of jsonpath SQLSTATE classes so non-JSON failure
/// modes (statement timeouts, cancellations, OOM) propagate to the
/// caller instead of being swallowed into a silent NULL. Without a
/// regression test the original concern — operators losing visibility
/// into a timed-out json_extract query — could be silently reintroduced
/// by a future "let's go back to WHEN OTHERS" refactor.
///
/// We can't easily simulate OOM, but `statement_timeout` is the
/// canonical signal we care about and is cheap to trigger: a tight
/// timeout + a jsonpath against a deeply-nested payload (or a
/// pg_sleep wrapper) will exhaust the budget mid-evaluation.
#[tokio::test]
async fn postgres_json_extract_0058_propagates_statement_timeout_not_silent_null() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool =
        connect_test_pool_and_migrate_config(&config, "db::postgres json_extract timeout pool")
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

    // Tight per-statement timeout. Run the timeout-bound query in a
    // separate `query` call so the SET LOCAL stays scoped to the
    // dedicated statement.
    let mut conn = pool.acquire().await.expect("acquire pg connection");
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .expect("begin");
    sqlx::query("SET LOCAL statement_timeout = '5ms'")
        .execute(&mut *conn)
        .await
        .expect("set short statement_timeout");

    // Force a wait long enough to outlast the 5ms budget. We can't
    // make `json_extract` itself slow in a deterministic way, but we
    // can compose with `pg_sleep` inside the same statement so the
    // post-fix code path observes the `query_canceled` SQLSTATE
    // *during* the json_extract evaluation chain. If a future
    // refactor reintroduces `WHEN OTHERS`, this errors becomes a
    // silent NULL and the assertion below fails.
    let result: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
        "SELECT json_extract(
                ('{\"x\":' || (SELECT pg_sleep(0.5)::text) || '}')::jsonb,
                '$.x'
            )",
    )
    .fetch_one(&mut *conn)
    .await;

    // Roll back the test tx whether or not the statement aborted so
    // we don't leave a poisoned tx behind for `close_test_pool`.
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;

    match result {
        Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("57014") => {
            // 57014 = `query_canceled` — the SQLSTATE PostgreSQL
            // raises for statement_timeout. This is exactly the
            // signal the WHEN OTHERS fix was meant to preserve.
        }
        Err(sqlx::Error::Database(db_err)) => {
            panic!(
                "expected statement_timeout (SQLSTATE 57014) to surface; got {:?} (code={:?})",
                db_err.message(),
                db_err.code()
            );
        }
        Err(other) => {
            panic!("expected DatabaseError; got {other:?}");
        }
        Ok(value) => {
            panic!(
                "json_extract silently swallowed statement_timeout — got Ok({value:?}) instead of query_canceled. \
                     Regression of #2224 fix: WHEN OTHERS catch-all reintroduced."
            );
        }
    }

    drop(conn);
    close_test_pool(pool, "db::postgres json_extract timeout pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}
#[tokio::test]
async fn postgres_startup_reseed_migrates_legacy_openclaw_agent_ids() {
    let test_db = TestDatabase::create().await;
    let mut config = postgres_test_config(&test_db);
    config.agents = vec![crate::config::AgentDef {
        id: "maker".to_string(),
        name: "Maker".to_string(),
        name_ko: Some("뚝딱이".to_string()),
        provider: "codex".to_string(),
        channels: crate::config::AgentChannels::new()
            .with("codex", crate::config::AgentChannel::from("maker-cdx")),
        peer_keywords: Vec::new(),
        peer_directory: true,
        keywords: Vec::new(),
        department: Some("engineering".to_string()),
        avatar_emoji: Some("🛠️".to_string()),
        preferred_intake_node_labels: None,
        memory_group: None,
    }];

    let pool =
        connect_test_pool_and_migrate_config(&config, "db::postgres legacy reseed test pool")
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

    sqlx::query(
        "INSERT INTO agents (
                id, name, provider, status, sprite_number, description, system_prompt, pipeline_config
             ) VALUES ($1, 'Legacy Maker', 'codex', 'working', 7, 'legacy-desc', 'legacy-prompt', '{\"k\":1}')",
    )
    .bind("openclaw-maker")
    .execute(&pool)
    .await
    .expect("insert legacy agent");
    sqlx::query("INSERT INTO github_repos (id, default_agent_id) VALUES ($1, $2)")
        .bind("owner/repo")
        .bind("openclaw-maker")
        .execute(&pool)
        .await
        .expect("insert github repo");
    sqlx::query("INSERT INTO kanban_cards (id, title, assigned_agent_id) VALUES ($1, $2, $3)")
        .bind("card-1")
        .bind("Card")
        .bind("openclaw-maker")
        .execute(&pool)
        .await
        .expect("insert card");
    sqlx::query("INSERT INTO sessions (session_key, agent_id, status) VALUES ($1, $2, $3)")
        .bind("sess-1")
        .bind("openclaw-maker")
        .bind("turn_active")
        .execute(&pool)
        .await
        .expect("insert session");
    startup_reseed(&pool, &config)
        .await
        .expect("startup reseed postgres");

    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM agents ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("list agent ids");
    assert_eq!(ids, vec!["maker".to_string()]);

    let (status, sprite_number, description, system_prompt): (
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, sprite_number, description, system_prompt
               FROM agents
              WHERE id = $1",
    )
    .bind("maker")
    .fetch_one(&pool)
    .await
    .expect("load canonical agent");
    assert_eq!(status, "working");
    assert_eq!(sprite_number, Some(7));
    assert_eq!(description.as_deref(), Some("legacy-desc"));
    assert_eq!(system_prompt.as_deref(), Some("legacy-prompt"));

    let github_default: Option<String> =
        sqlx::query_scalar("SELECT default_agent_id FROM github_repos WHERE id = $1")
            .bind("owner/repo")
            .fetch_one(&pool)
            .await
            .expect("load github repo default agent");
    assert_eq!(github_default.as_deref(), Some("maker"));

    let card_agent: Option<String> =
        sqlx::query_scalar("SELECT assigned_agent_id FROM kanban_cards WHERE id = $1")
            .bind("card-1")
            .fetch_one(&pool)
            .await
            .expect("load card agent");
    assert_eq!(card_agent.as_deref(), Some("maker"));

    let session_agent: Option<String> =
        sqlx::query_scalar("SELECT agent_id FROM sessions WHERE session_key = $1")
            .bind("sess-1")
            .fetch_one(&pool)
            .await
            .expect("load session agent");
    assert_eq!(session_agent.as_deref(), Some("maker"));
    close_test_pool(pool, "db::postgres legacy reseed test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}
#[tokio::test]
async fn postgres_startup_reseed_preserves_configured_legacy_openclaw_agent_id() {
    let test_db = TestDatabase::create().await;
    let mut config = postgres_test_config(&test_db);
    config.agents = vec![
        crate::config::AgentDef {
            id: "maker".to_string(),
            name: "Maker".to_string(),
            name_ko: None,
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels::new()
                .with("codex", crate::config::AgentChannel::from("maker-cdx")),
            peer_keywords: Vec::new(),
            peer_directory: true,
            keywords: Vec::new(),
            department: Some("engineering".to_string()),
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
        },
        crate::config::AgentDef {
            id: "openclaw-maker".to_string(),
            name: "Legacy Maker".to_string(),
            name_ko: None,
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels::new()
                .with("codex", crate::config::AgentChannel::from("legacy-cdx")),
            peer_keywords: Vec::new(),
            peer_directory: true,
            keywords: Vec::new(),
            department: Some("legacy".to_string()),
            avatar_emoji: None,
            preferred_intake_node_labels: None,
            memory_group: None,
        },
    ];

    let pool = connect_test_pool_and_migrate_config(
        &config,
        "db::postgres configured legacy reseed test pool",
    )
    .await
    .expect("connect and migrate postgres")
    .expect("postgres pool");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status)
             VALUES ($1, 'Configured Legacy Maker', 'codex', 'working')
             ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 provider = EXCLUDED.provider,
                 status = EXCLUDED.status",
    )
    .bind("openclaw-maker")
    .execute(&pool)
    .await
    .expect("insert configured legacy agent");
    sqlx::query(
        "INSERT INTO github_repos (id, default_agent_id) VALUES ($1, $2)
             ON CONFLICT (id) DO UPDATE SET default_agent_id = EXCLUDED.default_agent_id",
    )
    .bind("owner/repo")
    .bind("openclaw-maker")
    .execute(&pool)
    .await
    .expect("insert github repo");

    startup_reseed(&pool, &config)
        .await
        .expect("startup reseed postgres");

    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM agents ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("list agent ids");
    assert_eq!(ids, vec!["maker".to_string(), "openclaw-maker".to_string()]);

    let status: String = sqlx::query_scalar("SELECT status FROM agents WHERE id = $1")
        .bind("openclaw-maker")
        .fetch_one(&pool)
        .await
        .expect("load configured legacy agent");
    assert_eq!(status, "working");

    let github_default: Option<String> =
        sqlx::query_scalar("SELECT default_agent_id FROM github_repos WHERE id = $1")
            .bind("owner/repo")
            .fetch_one(&pool)
            .await
            .expect("load github repo default agent");
    assert_eq!(github_default.as_deref(), Some("openclaw-maker"));

    close_test_pool(pool, "db::postgres configured legacy reseed test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_startup_reseed_clears_session_fk_for_removed_agent() {
    // Regression: a sessions row on a stale agent used to fail startup with a
    // `sessions_agent_id_fkey` violation, because the sync never cleared sessions.agent_id.
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool = connect_test_pool_and_migrate_config(
        &config,
        "db::postgres stale agent session fk test pool",
    )
    .await
    .expect("connect and migrate postgres")
    .expect("postgres pool");

    sqlx::query(
        "INSERT INTO agents (
                id, name, provider, status, sprite_number, description, system_prompt, pipeline_config
             ) VALUES ($1, 'Stale E2E', 'codex', 'idle', 1, '', '', '{}')",
    )
    .bind("adk-dashboard-e2e")
    .execute(&pool)
    .await
    .expect("insert stale agent");

    sqlx::query("INSERT INTO sessions (session_key, agent_id, status) VALUES ($1, $2, $3)")
        .bind("stale-sess-1")
        .bind("adk-dashboard-e2e")
        .bind("disconnected")
        .execute(&pool)
        .await
        .expect("insert session referencing stale agent");

    startup_reseed(&pool, &config)
        .await
        .expect("startup reseed must succeed even when sessions reference the removed agent");

    let stale_agent_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = $1")
        .bind("adk-dashboard-e2e")
        .fetch_one(&pool)
        .await
        .expect("count stale agents");
    assert_eq!(
        stale_agent_count, 0,
        "stale agent must be deleted by reseed"
    );

    let session_agent: Option<String> =
        sqlx::query_scalar("SELECT agent_id FROM sessions WHERE session_key = $1")
            .bind("stale-sess-1")
            .fetch_one(&pool)
            .await
            .expect("load session agent after reseed");
    assert!(
        session_agent.is_none(),
        "sessions.agent_id must be nulled when the referenced agent is removed"
    );

    close_test_pool(pool, "db::postgres stale agent session fk test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

async fn insert_stale_agent_for_sync(pool: &PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO agents (
                id, name, provider, status, sprite_number, description, system_prompt, pipeline_config
             ) VALUES ($1, $1, 'codex', 'idle', 1, '', '', '{}')",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("insert stale agent");
}

async fn agent_ids_after_sync(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT id FROM agents ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("list agents")
}

#[tokio::test]
async fn agent_sync_keeps_a_removed_agent_that_an_agent_scheduled_message_needs() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);
    let pool = connect_test_pool_and_migrate_config(&config, "db::postgres smsg fk test pool")
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");
    insert_stale_agent_for_sync(&pool, "gone-agent").await;
    insert_stale_agent_for_sync(&pool, "gone-push").await;
    for (id, kind, agent) in [
        ("m-agent", "agent", "gone-agent"),
        ("m-push", "push", "gone-push"),
    ] {
        sqlx::query(
            "INSERT INTO scheduled_messages
                     (id, content, target_channel_id, delivery_kind, agent_id, scheduled_at)
                 VALUES ($1, 'hi', '123', $2, $3, NOW() + INTERVAL '1 day')",
        )
        .bind(id)
        .bind(kind)
        .bind(agent)
        .execute(&pool)
        .await
        .expect("insert scheduled message");
    }

    sync_agents_from_config_pg(&pool, &config.agents)
        .await
        .expect("an agent-kind scheduled message must not fail the boot sync");

    assert_eq!(
        agent_ids_after_sync(&pool).await,
        ["gone-agent", "pg-agent"]
    );
    let refs: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, agent_id FROM scheduled_messages ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("list scheduled messages");
    assert_eq!(
        refs,
        [
            ("m-agent".to_string(), Some("gone-agent".to_string())),
            ("m-push".to_string(), None),
        ]
    );

    close_test_pool(pool, "db::postgres smsg fk test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn a_failed_agent_sync_leaves_no_partial_roster() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);
    let pool = connect_test_pool_and_migrate_config(&config, "db::postgres sync tx test pool")
        .await
        .expect("connect and migrate postgres")
        .expect("postgres pool");
    insert_stale_agent_for_sync(&pool, "gone-agent").await;
    sqlx::raw_sql("CREATE FUNCTION refuse_agent_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected agent delete fault'; END $$; CREATE TRIGGER refuse_agent_delete BEFORE DELETE ON agents FOR EACH ROW EXECUTE FUNCTION refuse_agent_delete()")
        .execute(&pool)
        .await
        .expect("install delete fault");

    let error = sync_agents_from_config_pg(&pool, &config.agents)
        .await
        .expect_err("the injected delete fault must fail the sync");

    assert!(error.contains("injected agent delete fault"), "{error}");
    assert_eq!(
        agent_ids_after_sync(&pool).await,
        ["gone-agent"],
        "the configured agent's upsert must roll back with the failed sync"
    );

    close_test_pool(pool, "db::postgres sync tx test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_advisory_lock_lease_allows_only_one_live_holder() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool =
        connect_test_pool_and_migrate_config(&config, "db::postgres advisory singleton test pool")
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

    let mut first = AdvisoryLockLease::try_acquire(&pool, 91_001, "test advisory lock")
        .await
        .expect("acquire first advisory lock")
        .expect("first advisory lock holder");
    first
        .keepalive()
        .await
        .expect("first advisory lock keepalive");

    let second = AdvisoryLockLease::try_acquire(&pool, 91_001, "test advisory lock")
        .await
        .expect("acquire second advisory lock");
    assert!(second.is_none(), "second holder must be denied");

    first.unlock().await.expect("unlock first advisory lock");

    let third = AdvisoryLockLease::try_acquire(&pool, 91_001, "test advisory lock")
        .await
        .expect("acquire third advisory lock")
        .expect("third advisory lock holder after unlock");
    third.unlock().await.expect("unlock third advisory lock");

    close_test_pool(pool, "db::postgres advisory lock test pool")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_advisory_lock_lease_does_not_exhaust_shared_pool() {
    let test_db = TestDatabase::create().await;
    let mut config = postgres_test_config(&test_db);
    config.database.pool_max = 1;

    let pool = connect_test_pool_and_migrate_config(
        &config,
        "db::postgres advisory pool exhaustion test pool",
    )
    .await
    .expect("connect and migrate postgres")
    .expect("postgres pool");

    let lease = AdvisoryLockLease::try_acquire(&pool, 91_003, "test advisory lock")
        .await
        .expect("acquire advisory lock")
        .expect("advisory lock holder");

    health_check(&pool)
        .await
        .expect("shared pool remains usable while advisory lock is held");

    lease.unlock().await.expect("unlock advisory lock");
    close_test_pool(pool, "db::postgres advisory pool exhaustion test")
        .await
        .expect("close postgres pool");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_advisory_lock_lease_releases_on_drop_across_separate_pools() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool_a =
        connect_test_pool_and_migrate_config(&config, "db::postgres advisory drop test pool A")
            .await
            .expect("connect and migrate postgres pool A")
            .expect("postgres pool A");
    let pool_b =
        connect_test_pool_and_migrate_config(&config, "db::postgres advisory drop test pool B")
            .await
            .expect("connect and migrate postgres pool B")
            .expect("postgres pool B");

    let holder_a = AdvisoryLockLease::try_acquire(&pool_a, 91_002, "test advisory lock")
        .await
        .expect("acquire advisory lock on pool A")
        .expect("pool A advisory lock holder");

    let denied_b = AdvisoryLockLease::try_acquire(&pool_b, 91_002, "test advisory lock")
        .await
        .expect("attempt advisory lock on pool B");
    assert!(
        denied_b.is_none(),
        "pool B must be fenced while pool A holds lock"
    );

    drop(holder_a);

    let holder_b = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(holder) =
                AdvisoryLockLease::try_acquire(&pool_b, 91_002, "test advisory lock")
                    .await
                    .expect("acquire advisory lock on pool B after drop")
            {
                break holder;
            }

            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("pool B advisory lock holder after drop");
    holder_b
        .unlock()
        .await
        .expect("unlock advisory lock on pool B");

    close_test_pool(pool_b, "db::postgres advisory lock pool B")
        .await
        .expect("close pool B");
    close_test_pool(pool_a, "db::postgres advisory lock pool A")
        .await
        .expect("close pool A");
    test_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_startup_advisory_lock_serializes_concurrent_startup_sections() {
    let test_db = TestDatabase::create().await;
    let config = postgres_test_config(&test_db);

    let pool_a =
        connect_test_pool_and_migrate_config(&config, "db::postgres startup lock test pool A")
            .await
            .expect("connect and migrate postgres pool A")
            .expect("postgres pool A");
    let pool_b =
        connect_test_pool_and_migrate_config(&config, "db::postgres startup lock test pool B")
            .await
            .expect("connect and migrate postgres pool B")
            .expect("postgres pool B");

    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let first_entered = Arc::new(Notify::new());
    let release_first = Arc::new(Notify::new());

    let pool_a_task = pool_a.clone();
    let active_a = Arc::clone(&active);
    let max_active_a = Arc::clone(&max_active);
    let events_a = Arc::clone(&events);
    let first_entered_a = Arc::clone(&first_entered);
    let release_first_a = Arc::clone(&release_first);
    let config_a = config.clone();
    let first = tokio::spawn(async move {
        with_startup_advisory_lock(&pool_a_task, || async {
            let now = active_a.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_a.fetch_max(now, Ordering::SeqCst);
            events_a.lock().await.push("first-enter");
            first_entered_a.notify_one();
            release_first_a.notified().await;
            startup_reseed(&pool_a_task, &config_a).await?;
            events_a.lock().await.push("first-exit");
            active_a.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
        .await
    });

    first_entered.notified().await;

    let pool_b_task = pool_b.clone();
    let active_b = Arc::clone(&active);
    let max_active_b = Arc::clone(&max_active);
    let events_b = Arc::clone(&events);
    let config_b = config.clone();
    let second = tokio::spawn(async move {
        with_startup_advisory_lock(&pool_b_task, || async {
            let now = active_b.fetch_add(1, Ordering::SeqCst) + 1;
            max_active_b.fetch_max(now, Ordering::SeqCst);
            events_b.lock().await.push("second-enter");
            startup_reseed(&pool_b_task, &config_b).await?;
            active_b.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
        .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        active.load(Ordering::SeqCst),
        1,
        "the second startup section must wait while the first lock holder is active"
    );

    release_first.notify_one();
    first
        .await
        .expect("first startup task joins")
        .expect("first startup lock section");
    second
        .await
        .expect("second startup task joins")
        .expect("second startup lock section");

    assert_eq!(
        max_active.load(Ordering::SeqCst),
        1,
        "startup lock must prevent overlapping startup mutation sections"
    );
    assert_eq!(
        events.lock().await.as_slice(),
        ["first-enter", "first-exit", "second-enter"]
    );

    close_test_pool(pool_b, "db::postgres startup lock pool B")
        .await
        .expect("close pool B");
    close_test_pool(pool_a, "db::postgres startup lock pool A")
        .await
        .expect("close pool A");
    test_db.drop().await;
}

#[tokio::test]
async fn postgres_test_sqlx_timeout_wrapper_fails_fast() {
    let err = run_test_postgres_sqlx_op_with_timeout(
        Duration::from_millis(20),
        "synthetic postgres timeout",
        async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok::<(), sqlx::Error>(())
        },
    )
    .await
    .expect_err("sleep should time out");

    assert!(
        err.contains("timed out"),
        "expected timeout wording, got {err}"
    );
}

/// The `E -> P` hierarchy has to fail loudly from the wrong side.
///
/// `RequirePgEnvGuard::set` above shows the canonical order: env lock, then
/// lifecycle lock. Reversed, the two mutexes form an ABBA cycle with any
/// concurrently in-flight `E -> P` test, and because both are
/// `std::sync::Mutex` the whole run parks with no timeout, no deadlock
/// detection, and no frame naming the offender. This asserts the
/// replacement behaviour: a P holder reaching for E panics immediately, and
/// the panic names the offending thread — which under libtest is the
/// offending test.
///
/// Runs on a spawned thread with a bounded wait for the same reason the
/// sibling re-entry proof in `config.rs` does: if the tripwire ever stops
/// firing, this test must go red rather than hang the suite it protects.
/// Acquiring P itself is deliberately *not* on the short timer — a real
/// `_pg` test may legitimately hold it right now, and that wait is not what
/// is being measured.
#[test]
fn env_lock_after_lifecycle_lock_trips_the_order_tripwire() {
    let (armed_tx, armed_rx) = std::sync::mpsc::channel();
    let (verdict_tx, verdict_rx) = std::sync::mpsc::channel();
    let offender = "pg_lifecycle_then_env_lock";
    let handle = std::thread::Builder::new()
        .name(offender.to_string())
        .spawn(move || {
            let _lifecycle = super::lock_test_lifecycle();
            armed_tx.send(()).expect("signal that the P lock is held");
            let outcome = std::panic::catch_unwind(|| {
                let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
            });
            let message = outcome.err().map(|payload| {
                payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        payload
                            .downcast_ref::<&str>()
                            .map(|text| (*text).to_string())
                    })
                    .unwrap_or_default()
            });
            verdict_tx.send(message).expect("send inversion verdict");
        })
        .expect("spawn the lock-order probe thread");

    armed_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("probe thread should take the postgres lifecycle lock");
    let message = verdict_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("E-after-P must panic before it can block on the env mutex")
        .expect("acquiring the env lock while holding the lifecycle lock must panic");

    assert!(
        message.contains(offender),
        "the tripwire must name the offending thread; got: {message}"
    );
    assert!(
        message.contains("E -> P"),
        "the tripwire must state the canonical hierarchy; got: {message}"
    );
    join_within(handle, WAIT_CAP);
}
