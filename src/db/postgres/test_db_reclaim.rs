//! Reclaims PostgreSQL test databases orphaned by killed test processes.
//!
//! Drop-based fixture cleanup cannot run after SIGKILL, so the first fixture
//! CREATE of every test process sweeps what earlier processes left behind.
//! A fixture is created under a reserved pending name, marked by COMMENT, then
//! renamed, so a kill between any two steps leaves an identifiable database.
//! Caller-chosen names prove nothing on their own: they are free-form and
//! PostgreSQL truncates them at 63 bytes.
//!
//! Liveness is not proven: a candidate only has to be `RECLAIM_MIN_AGE` old by
//! the server clock and have no session, so no live fixture may outlive that age.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::PgPool;

/// Contains no regex metacharacters, so it is also used verbatim in the SQL pattern.
const MARKER_PREFIX: &str = "agentdesk-test-fixture created_at_unix=";
/// Reserved for in-flight creates; the name carries the server epoch of the CREATE.
const PENDING_PREFIX: &str = "agentdesk_pending_";

/// Operating assumption, not a guarantee: no live fixture is older than this.
pub(super) const RECLAIM_MIN_AGE: Duration = Duration::from_secs(6 * 60 * 60);

static SWEPT_THIS_PROCESS: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Stops `create_marked` right after CREATE, the state a SIGKILL there leaves.
    static KILL_AFTER_CREATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Server clock, so hosts sharing one test server agree on every database's age.
async fn server_now_unix(admin_pool: &PgPool) -> Result<i64, String> {
    super::run_test_postgres_sqlx_op(
        "read postgres server clock",
        sqlx::query_scalar::<_, i64>("SELECT extract(epoch FROM clock_timestamp())::bigint")
            .fetch_one(admin_pool),
    )
    .await
}

fn pending_database_name(created_at_unix: i64) -> String {
    format!(
        "{PENDING_PREFIX}{created_at_unix}_{}",
        uuid::Uuid::new_v4().simple()
    )
}

pub(super) async fn mark_test_database(
    admin_pool: &PgPool,
    database_name: &str,
    created_at_unix: i64,
    label: &str,
) -> Result<(), String> {
    super::run_test_postgres_sqlx_op(
        &format!("{label} mark postgres test db {database_name}"),
        sqlx::query(&format!(
            "COMMENT ON DATABASE \"{database_name}\" IS '{MARKER_PREFIX}{created_at_unix}'"
        ))
        .execute(admin_pool),
    )
    .await
    .map(|_| ())
}

/// CREATE never runs under `database_name`, so no step leaves an unmarked fixture.
pub(super) async fn create_marked(
    admin_pool: &PgPool,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    let created_at = server_now_unix(admin_pool).await?;
    let pending = pending_database_name(created_at);
    super::run_test_postgres_sqlx_op(
        &format!("{label} create postgres test db {database_name}"),
        sqlx::query(&format!("CREATE DATABASE \"{pending}\"")).execute(admin_pool),
    )
    .await?;
    if KILL_AFTER_CREATE.with(std::cell::Cell::get) {
        return Err(format!("simulated kill after CREATE DATABASE {pending}"));
    }
    let finished = async {
        mark_test_database(admin_pool, &pending, created_at, label).await?;
        super::run_test_postgres_sqlx_op(
            &format!("{label} rename postgres test db {database_name}"),
            sqlx::query(&format!(
                "ALTER DATABASE \"{pending}\" RENAME TO \"{database_name}\""
            ))
            .execute(admin_pool),
        )
        .await
        .map(|_| ())
    }
    .await;
    if finished.is_err() {
        // Only the pending name is ours; a RENAME collision must not touch `database_name`.
        let dropped = super::run_test_postgres_sqlx_op(
            &format!("{label} drop pending postgres test db {pending}"),
            sqlx::query(&format!("DROP DATABASE IF EXISTS \"{pending}\"")).execute(admin_pool),
        )
        .await;
        if let Err(error) = dropped {
            tracing::warn!(label, error, "left pending postgres test db for the sweep");
        }
    }
    finished
}

/// Fixture databases at least `min_age` old by the server clock with no connected session.
pub(super) async fn stale_test_databases(
    admin_pool: &PgPool,
    min_age: Duration,
) -> Result<Vec<String>, String> {
    super::run_test_postgres_sqlx_op(
        "list stale postgres test dbs",
        sqlx::query_scalar::<_, String>(
            "SELECT d.datname::text
             FROM pg_database d
             WHERE coalesce(
                     substring(shobj_description(d.oid, 'pg_database') FROM $1),
                     substring(d.datname FROM $2)
                   )::bigint <= extract(epoch FROM clock_timestamp())::bigint - $3
               AND NOT EXISTS (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)
             ORDER BY d.datname",
        )
        .bind(format!("^{MARKER_PREFIX}([0-9]{{1,12}})$"))
        .bind(format!("^{PENDING_PREFIX}([0-9]{{1,12}})_[0-9a-f]{{32}}$"))
        .bind(i64::try_from(min_age.as_secs()).unwrap_or(i64::MAX))
        .fetch_all(admin_pool),
    )
    .await
}

pub(super) async fn reclaim_stale_test_databases(
    admin_pool: &PgPool,
    min_age: Duration,
    label: &str,
) -> Result<Vec<String>, String> {
    let mut dropped = Vec::new();
    for database_name in stale_test_databases(admin_pool, min_age).await? {
        if !super::is_safe_test_database_name(&database_name) {
            continue;
        }
        // No FORCE: a database that gained a session after the scan fails the
        // DROP and is skipped instead of having that session killed.
        let result = super::run_test_postgres_sqlx_op(
            &format!("{label} reclaim postgres test db {database_name}"),
            sqlx::query(&format!("DROP DATABASE IF EXISTS \"{database_name}\""))
                .execute(admin_pool),
        )
        .await;
        match result {
            Ok(_) => dropped.push(database_name),
            Err(error) => tracing::warn!(label, error, "skipped stale postgres test db"),
        }
    }
    Ok(dropped)
}

/// Best-effort: a failed sweep never fails the fixture that triggered it.
pub(super) async fn reclaim_once_per_process(admin_pool: &PgPool, label: &str) {
    if SWEPT_THIS_PROCESS.swap(true, Ordering::SeqCst) {
        return;
    }
    match reclaim_stale_test_databases(admin_pool, RECLAIM_MIN_AGE, label).await {
        Ok(dropped) if !dropped.is_empty() => {
            eprintln!(
                "reclaimed {} orphaned postgres test databases: {}",
                dropped.len(),
                dropped.join(", ")
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(label, error, "postgres test db reclaim sweep failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KILL_AFTER_CREATE, MARKER_PREFIX, PENDING_PREFIX, RECLAIM_MIN_AGE, create_marked,
        mark_test_database, pending_database_name, reclaim_stale_test_databases, server_now_unix,
        stale_test_databases,
    };
    use sqlx::PgPool;

    const LABEL: &str = "db::postgres reclaim tests";
    const CHILD_ENV: &str = "AGENTDESK_TEST_RECLAIM_FRESH_PROCESS_CHILD";
    const CHILD_TEST: &str = "db::postgres::test_db_reclaim::tests::pg_reclaim_fresh_process_child";

    struct Fixture {
        admin_url: String,
        base: String,
        admin_pool: PgPool,
    }

    async fn fixture() -> Option<Fixture> {
        let base = crate::db::postgres::postgres_test_database_url_base()?;
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        let admin_url = format!("{base}/{admin_db}");
        let admin_pool = crate::db::postgres::connect_test_pool(&admin_url, LABEL)
            .await
            .expect("connect admin pool");
        Some(Fixture {
            admin_url,
            base,
            admin_pool,
        })
    }

    fn fresh_name(tag: &str) -> String {
        format!("agentdesk_reclaim_{tag}_{}", uuid::Uuid::new_v4().simple())
    }

    async fn create_fixture(fx: &Fixture, tag: &str) -> String {
        let name = fresh_name(tag);
        crate::db::postgres::create_test_database(&fx.admin_url, &name, LABEL)
            .await
            .expect("create fixture db");
        name
    }

    /// Discards the in-process ownership token, the state a killed process loses.
    fn forget_ownership(fx: &Fixture, name: &str) {
        let options = crate::db::postgres::parse_test_postgres_options(&fx.admin_url, LABEL)
            .expect("parse admin url");
        assert!(crate::db::postgres::take_test_database_ownership(&options, name).is_some());
    }

    async fn server_now(fx: &Fixture) -> i64 {
        server_now_unix(&fx.admin_pool).await.expect("server clock")
    }

    async fn old_stamp(fx: &Fixture) -> i64 {
        server_now(fx).await
            - i64::try_from(RECLAIM_MIN_AGE.as_secs()).expect("min age fits i64")
            - 60
    }

    async fn backdate(fx: &Fixture, name: &str) {
        let created = old_stamp(fx).await;
        mark_test_database(&fx.admin_pool, name, created, LABEL)
            .await
            .expect("backdate marker");
    }

    async fn marker(fx: &Fixture, name: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = $1",
        )
        .bind(name)
        .fetch_one(&fx.admin_pool)
        .await
        .expect("query marker")
    }

    async fn exists(fx: &Fixture, name: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)",
        )
        .bind(name)
        .fetch_one(&fx.admin_pool)
        .await
        .expect("query pg_database")
    }

    async fn raw_drop(fx: &Fixture, name: &str) {
        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .execute(&fx.admin_pool)
            .await
            .expect("drop test db");
    }

    #[tokio::test]
    async fn pg_reclaim_drops_orphaned_fixture_database() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        let name = create_fixture(&fx, "orphan").await;
        forget_ownership(&fx, &name);
        // The sweep can only find what the fixture itself marked.
        let written = marker(&fx, &name).await;
        let created_at: i64 = written
            .as_deref()
            .and_then(|comment| comment.strip_prefix(MARKER_PREFIX))
            .and_then(|stamp| stamp.parse().ok())
            .unwrap_or_else(|| panic!("fixture {name} left no marker: {written:?}"));
        assert!(
            server_now(&fx).await.abs_diff(created_at) <= 60,
            "marker {written:?}"
        );
        backdate(&fx, &name).await;

        let dropped = reclaim_stale_test_databases(&fx.admin_pool, RECLAIM_MIN_AGE, LABEL)
            .await
            .expect("reclaim");

        // Another process's first-fixture sweep may win the DROP, so pin the outcome only.
        assert!(
            !exists(&fx, &name).await,
            "orphan {name} not reclaimed: {dropped:?}"
        );
    }

    #[tokio::test]
    async fn pg_reclaim_skips_database_with_active_session() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        let name = create_fixture(&fx, "active").await;
        let session = crate::db::postgres::connect_test_pool(&format!("{}/{name}", fx.base), LABEL)
            .await
            .expect("connect fixture db");
        sqlx::query("SELECT 1")
            .execute(&session)
            .await
            .expect("open session");
        backdate(&fx, &name).await;

        let stale = stale_test_databases(&fx.admin_pool, RECLAIM_MIN_AGE)
            .await
            .expect("list stale");
        assert!(!stale.contains(&name), "active {name} became a candidate");
        let dropped = reclaim_stale_test_databases(&fx.admin_pool, RECLAIM_MIN_AGE, LABEL)
            .await
            .expect("reclaim");

        assert!(!dropped.contains(&name));
        assert!(exists(&fx, &name).await);
        session.close().await;
        crate::db::postgres::drop_test_database(&fx.admin_url, &name, LABEL)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    async fn pg_reclaim_skips_database_younger_than_min_age() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        let name = create_fixture(&fx, "young").await;
        forget_ownership(&fx, &name);

        let dropped = reclaim_stale_test_databases(&fx.admin_pool, RECLAIM_MIN_AGE, LABEL)
            .await
            .expect("reclaim");

        assert!(!dropped.contains(&name));
        assert!(exists(&fx, &name).await);
        raw_drop(&fx, &name).await;
    }

    #[tokio::test]
    async fn pg_reclaim_never_targets_unmarked_databases() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        // Fixture-shaped names whose comments are absent, foreign, or malformed.
        let old = old_stamp(&fx).await;
        let cases = [
            (fresh_name("unmarked"), None),
            (format!("{PENDING_PREFIX}{old}_notuuid"), None),
            (fresh_name("foreign"), Some("production data".to_string())),
            (
                fresh_name("malformed"),
                Some(format!(
                    "agentdesk-test-fixture created_at_unix={old} extra"
                )),
            ),
            (
                fresh_name("prefixed"),
                Some(format!("x agentdesk-test-fixture created_at_unix={old}")),
            ),
        ];
        for (name, comment) in &cases {
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&fx.admin_pool)
                .await
                .expect("create unmarked db");
            if let Some(comment) = comment {
                sqlx::query(&format!("COMMENT ON DATABASE \"{name}\" IS '{comment}'"))
                    .execute(&fx.admin_pool)
                    .await
                    .expect("comment db");
            }
        }

        let stale = stale_test_databases(&fx.admin_pool, std::time::Duration::ZERO)
            .await
            .expect("list stale");

        for (name, _) in &cases {
            assert!(!stale.contains(name), "unmarked {name} became a candidate");
            raw_drop(&fx, name).await;
        }
    }

    #[tokio::test]
    async fn pg_reclaim_finds_database_killed_before_marker() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        KILL_AFTER_CREATE.with(|kill| kill.set(true));
        let killed = create_marked(&fx.admin_pool, &fresh_name("killed"), LABEL).await;
        KILL_AFTER_CREATE.with(|kill| kill.set(false));
        let error = killed.expect_err("create must stop after CREATE");
        let left = error.rsplit(' ').next().unwrap_or_default().to_string();
        let comment = marker(&fx, &left).await;

        // Listing only: a zero age would also list other processes' live fixtures.
        let stale = stale_test_databases(&fx.admin_pool, std::time::Duration::ZERO)
            .await
            .expect("list stale");
        raw_drop(&fx, &left).await;

        assert_eq!(comment, None, "{error}");
        assert!(
            stale.contains(&left),
            "unmarked {left} is invisible to the sweep"
        );
    }

    #[tokio::test]
    async fn pg_reclaim_drops_old_database_created_but_never_marked() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        let name = pending_database_name(old_stamp(&fx).await);
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&fx.admin_pool)
            .await
            .expect("create unmarked db");

        let dropped = reclaim_stale_test_databases(&fx.admin_pool, RECLAIM_MIN_AGE, LABEL)
            .await
            .expect("reclaim");

        let leaked = exists(&fx, &name).await;
        raw_drop(&fx, &name).await;
        assert!(!leaked, "unmarked {name} not reclaimed: {dropped:?}");
    }

    /// The sweep must run from `create_test_database` itself in every new process.
    #[tokio::test]
    async fn pg_reclaim_runs_on_first_fixture_of_fresh_process() {
        let _lifecycle = crate::db::postgres::lock_test_lifecycle();
        let Some(fx) = fixture().await else { return };
        let marked = create_fixture(&fx, "wired").await;
        forget_ownership(&fx, &marked);
        backdate(&fx, &marked).await;
        let unmarked = pending_database_name(old_stamp(&fx).await);
        sqlx::query(&format!("CREATE DATABASE \"{unmarked}\""))
            .execute(&fx.admin_pool)
            .await
            .expect("create unmarked db");

        // Captured, so the child's libtest summary never reaches this lane's stdout.
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--ignored", "--exact", CHILD_TEST, "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("run fresh test process");
        let mut leaked = Vec::new();
        for name in [&marked, &unmarked] {
            if exists(&fx, name).await {
                raw_drop(&fx, name).await;
                leaked.push(name.clone());
            }
        }

        let output =
            String::from_utf8_lossy(&child.stdout) + String::from_utf8_lossy(&child.stderr);
        let transcript = output
            .lines()
            .filter(|line| !line.starts_with("test result:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            child.status.success(),
            "fresh process failed:\n{transcript}"
        );
        assert!(
            transcript.contains(&format!("test {CHILD_TEST} ... ok")),
            "child test did not run:\n{transcript}"
        );
        assert!(leaked.is_empty(), "fresh process left orphans {leaked:?}");
    }

    #[tokio::test]
    #[ignore = "helper subprocess: one fixture create in a fresh process"]
    async fn pg_reclaim_fresh_process_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let fx = fixture().await.expect("fixture base reaches the child");
        let name = create_fixture(&fx, "child").await;
        crate::db::postgres::drop_test_database(&fx.admin_url, &name, LABEL)
            .await
            .expect("drop child fixture");
    }

    /// Lists what the sweep would reclaim on the configured fixture server; drops nothing.
    /// `cargo test --lib pg_reclaim_dry_run -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn pg_reclaim_dry_run() {
        let Some(fx) = fixture().await else {
            println!("POSTGRES_TEST_DATABASE_URL_BASE unset");
            return;
        };
        let stale = stale_test_databases(&fx.admin_pool, RECLAIM_MIN_AGE)
            .await
            .expect("list stale");
        let options = crate::db::postgres::parse_test_postgres_options(&fx.admin_url, LABEL)
            .expect("parse admin url");
        println!(
            "reclaim dry-run on {}: {} candidates",
            crate::db::fixture_target::server_identity(&options),
            stale.len()
        );
        for name in stale {
            println!("  would drop {name}");
        }
    }
}
