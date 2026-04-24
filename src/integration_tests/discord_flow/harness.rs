//! [`TestHarness`] — the glue that lets a scenario reach a fresh mock
//! Discord transport, an isolated `AGENTDESK_ROOT_DIR`, and (optionally) an
//! ephemeral Postgres database in a single one-liner.
//!
//! Isolation guarantees (per DoD):
//! - **Mock Discord HTTP layer** — see [`super::mock_discord::MockDiscord`].
//! - **Ephemeral Postgres DB** — [`TestHarness::with_postgres`] creates a
//!   unique database via the existing
//!   [`crate::db::postgres::create_test_database`] helper. Skipped with a
//!   clear message when no local Postgres is available.
//! - **Separate tmux namespace** — each harness gets a private
//!   `$TMUX_TMPDIR` rooted at `/tmp/r4-tmux-<uuid>`. Even when several
//!   scenarios run in parallel they cannot collide on the default socket.
//! - **Runtime root isolation** — `AGENTDESK_ROOT_DIR` is pointed at a
//!   scenario-local tempdir so inflight-state reads/writes do not bleed
//!   between scenarios.
//!
//! The harness owns its env-var overrides; dropping it clears them. The
//! existing `crate::config::shared_test_env_lock()` mutex is held for the
//! lifetime of the harness so we never race with other `#[cfg(test)]` code
//! that also mutates process-wide env vars.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::MutexGuard;

use tempfile::TempDir;

use super::mock_discord::MockDiscord;

/// Guard owning all process-wide state the harness mutates. Drop order
/// matters: env vars are cleared, then the tempdirs/Postgres handle release
/// their resources, then the shared env mutex is released.
pub(crate) struct TestHarness {
    /// Recorder for outbound Discord calls.
    pub(crate) mock_discord: MockDiscord,
    /// Tempdir backing `AGENTDESK_ROOT_DIR`.
    runtime_root: TempDir,
    /// Tempdir backing `$TMUX_TMPDIR = /tmp/r4-tmux-<uuid>`.
    tmux_tmpdir: PathBuf,
    /// Optional ephemeral PG database (only present on `with_postgres`).
    postgres: Option<EphemeralPostgres>,
    /// Previous env values, restored on drop.
    previous_root: Option<OsString>,
    previous_tmux_tmpdir: Option<OsString>,
    /// Shared env-mutex guard — held for the harness lifetime so env-var
    /// mutations don't race.
    _env_guard: MutexGuard<'static, ()>,
}

impl TestHarness {
    /// Build a harness with the mock Discord transport and a scenario-local
    /// runtime root + tmux namespace. No Postgres.
    pub(crate) fn new() -> Self {
        let env_guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let runtime_root = TempDir::new().expect("create scenario runtime_root tempdir");
        let tmux_tmpdir = PathBuf::from(format!("/tmp/r4-tmux-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmux_tmpdir).expect("create ephemeral tmux namespace");

        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_tmux_tmpdir = std::env::var_os("TMUX_TMPDIR");
        // SAFETY: env mutations are serialized by the shared env lock held
        // for the harness lifetime.
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", runtime_root.path());
            std::env::set_var("TMUX_TMPDIR", &tmux_tmpdir);
        }

        Self {
            mock_discord: MockDiscord::new(),
            runtime_root,
            tmux_tmpdir,
            postgres: None,
            previous_root,
            previous_tmux_tmpdir,
            _env_guard: env_guard,
        }
    }

    /// Build a harness and provision an ephemeral Postgres database. Returns
    /// `None` when Postgres is not reachable — scenario tests should call
    /// [`requires_pg`] and skip gracefully in that case.
    pub(crate) async fn with_postgres(scenario_label: &str) -> Option<Self> {
        let mut harness = Self::new();
        let ephemeral = EphemeralPostgres::create(scenario_label).await?;
        harness.postgres = Some(ephemeral);
        Some(harness)
    }

    pub(crate) fn runtime_root(&self) -> &Path {
        self.runtime_root.path()
    }

    #[allow(dead_code)]
    pub(crate) fn tmux_tmpdir(&self) -> &Path {
        &self.tmux_tmpdir
    }

    #[allow(dead_code)]
    pub(crate) fn postgres_database_url(&self) -> Option<&str> {
        self.postgres.as_ref().map(|pg| pg.database_url.as_str())
    }

    /// Drop the harness and asynchronously tear down the Postgres database
    /// (if any). Call this at the end of scenarios that allocate a DB so
    /// the ephemeral instance is cleaned up even when the outer test panics
    /// elsewhere — [`Drop`] cannot run async teardown.
    pub(crate) async fn teardown(mut self) {
        if let Some(ephemeral) = self.postgres.take() {
            ephemeral.drop_async().await;
        }
        // The rest runs through the `Drop` impl below.
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // SAFETY: shared env lock is still held via `_env_guard`.
        unsafe {
            match self.previous_root.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
            match self.previous_tmux_tmpdir.take() {
                Some(value) => std::env::set_var("TMUX_TMPDIR", value),
                None => std::env::remove_var("TMUX_TMPDIR"),
            }
        }
        // Best-effort cleanup for the ephemeral tmux namespace; on failure
        // we leave the directory behind rather than panicking inside Drop.
        let _ = std::fs::remove_dir_all(&self.tmux_tmpdir);
        if let Some(pg) = self.postgres.take() {
            // Sync Drop — spawn a blocking best-effort drop. Scenarios that
            // want deterministic teardown should call `teardown().await`.
            pg.leak_on_sync_drop();
        }
    }
}

/// Decide whether a scenario that *needs* Postgres should run. Returns
/// `true` when a Postgres base URL is available.
pub(crate) fn postgres_available() -> bool {
    if std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
    {
        return true;
    }
    // Default assumption: if `PGHOST` or `USER` is set we *might* reach
    // a local cluster. Scenarios call [`TestHarness::with_postgres`] which
    // actually attempts the connect and returns `None` on failure; this
    // helper is advisory only.
    std::env::var("PGHOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_some()
        || std::env::var("USER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some()
}

struct EphemeralPostgres {
    admin_url: String,
    database_name: String,
    database_url: String,
    label: String,
    _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
}

impl EphemeralPostgres {
    async fn create(scenario_label: &str) -> Option<Self> {
        let lifecycle = crate::db::postgres::lock_test_lifecycle();
        let base = postgres_base_database_url();
        let admin_url = postgres_admin_database_url(&base);
        let database_name = format!(
            "agentdesk_flow_{}_{}",
            sanitize_label(scenario_label),
            uuid::Uuid::new_v4().simple()
        );
        let label = format!("discord_flow {scenario_label}");
        crate::db::postgres::create_test_database(&admin_url, &database_name, &label)
            .await
            .ok()?;
        let database_url = format!("{base}/{database_name}");
        Some(Self {
            admin_url,
            database_name,
            database_url,
            label,
            _lifecycle: lifecycle,
        })
    }

    async fn drop_async(self) {
        let _ = crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            &self.label,
        )
        .await;
    }

    fn leak_on_sync_drop(self) {
        // Synchronous Drop cannot await; leak the database name so a later
        // `drop_test_database` (manual or via admin script) cleans it. The
        // lifecycle guard is released as `self` falls out of scope.
        tracing::debug!(
            database = %self.database_name,
            "discord_flow harness dropped without explicit teardown; leaking ephemeral PG db"
        );
    }
}

fn sanitize_label(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn postgres_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }
    let user = std::env::var("PGUSER")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "postgres".to_string());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());
    match std::env::var("PGPASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(pw) => format!("postgresql://{user}:{pw}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

fn postgres_admin_database_url(base: &str) -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{base}/{admin_db}")
}
