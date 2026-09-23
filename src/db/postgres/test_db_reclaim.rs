//! Reclaims PostgreSQL test databases orphaned by killed test processes.
//!
//! Drop-based fixture cleanup cannot run after SIGKILL, so the first fixture
//! CREATE of every test process sweeps what earlier processes left behind.
//! Provenance comes from a COMMENT marker that only `create_test_database`
//! writes; fixture names are free-form and PostgreSQL truncates them at 63
//! bytes, so a name pattern alone cannot prove a database is a fixture.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::PgPool;

/// Contains no regex metacharacters, so it is also used verbatim in the SQL pattern.
const MARKER_PREFIX: &str = "agentdesk-test-fixture created_at_unix=";

/// Far beyond any single test's lifetime, so live runs in other processes are never swept.
pub(super) const RECLAIM_MIN_AGE: Duration = Duration::from_secs(6 * 60 * 60);

static SWEPT_THIS_PROCESS: AtomicBool = AtomicBool::new(false);

pub(super) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub(super) async fn mark_test_database(
    admin_pool: &PgPool,
    database_name: &str,
    created_at_unix: u64,
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

/// Marks a just-created fixture, then closes the admin pool even if marking failed.
pub(super) async fn mark_created(
    admin_pool: PgPool,
    database_name: &str,
    label: &str,
) -> Result<(), String> {
    let marked = mark_test_database(&admin_pool, database_name, now_unix(), label).await;
    let closed = super::close_test_pool(admin_pool, &format!("{label} admin")).await;
    marked.and(closed)
}

/// Marked fixture databases older than `min_age` with no connected session.
pub(super) async fn stale_test_databases(
    admin_pool: &PgPool,
    min_age: Duration,
) -> Result<Vec<String>, String> {
    let cutoff = now_unix().saturating_sub(min_age.as_secs());
    super::run_test_postgres_sqlx_op(
        "list stale postgres test dbs",
        sqlx::query_scalar::<_, String>(
            "SELECT d.datname::text
             FROM pg_database d
             WHERE substring(shobj_description(d.oid, 'pg_database') FROM $1)::bigint < $2
               AND NOT EXISTS (SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname)
             ORDER BY d.datname",
        )
        .bind(format!("^{MARKER_PREFIX}([0-9]{{1,12}})$"))
        .bind(i64::try_from(cutoff).unwrap_or(0))
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
        MARKER_PREFIX, RECLAIM_MIN_AGE, mark_test_database, now_unix, reclaim_stale_test_databases,
        stale_test_databases,
    };
    use sqlx::PgPool;

    const LABEL: &str = "db::postgres reclaim tests";

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

    async fn create_marked(fx: &Fixture, tag: &str) -> String {
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

    async fn backdate(fx: &Fixture, name: &str) {
        let created = now_unix() - RECLAIM_MIN_AGE.as_secs() - 60;
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
        let name = create_marked(&fx, "orphan").await;
        forget_ownership(&fx, &name);
        // The sweep can only find what the fixture itself marked.
        let written = marker(&fx, &name).await;
        let created_at: u64 = written
            .as_deref()
            .and_then(|comment| comment.strip_prefix(MARKER_PREFIX))
            .and_then(|stamp| stamp.parse().ok())
            .unwrap_or_else(|| panic!("fixture {name} left no marker: {written:?}"));
        assert!(now_unix().abs_diff(created_at) <= 60, "marker {written:?}");
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
        let name = create_marked(&fx, "active").await;
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
        let name = create_marked(&fx, "young").await;
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
        let old = now_unix() - RECLAIM_MIN_AGE.as_secs() - 60;
        let cases = [
            (fresh_name("unmarked"), None),
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
