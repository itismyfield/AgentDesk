//! #4379 — bounded exponential backoff for the dcserver PostgreSQL bootstrap.
//!
//! Before this module, `dcserver` called `crate::db::postgres::connect(..)`
//! exactly once at boot and `std::process::exit(1)` on the first failure. Under
//! launchd (`KeepAlive=true`, `ThrottleInterval=5`) a transient DB blip (tunnel
//! reconnect, PG restart, failover) became a ~8s tight crash loop that flooded
//! the stderr log (3.2 MB of one line on 2026-07-09) and left Discord relay
//! silently dead for ~30 minutes with zero operator signal.
//!
//! [`connect_with_backoff`] wraps the connect attempt in a bounded exponential
//! backoff (1 initial attempt + up to [`MAX_RETRIES`] retries, delays
//! `1→2→4→8→16s` capped at [`BACKOFF_CAP_SECS`]). This removes the tight
//! launchd loop: each attempt runs real migration/startup work on an eager pool
//! with a 10s deadline, then activates the eager runtime pool with its fast 3s
//! acquire timeout. A PG that recovers mid-backoff boots cleanly. Pool-acquire
//! timeouts are logged per attempt with an RFC3339 timestamp and the
//! caller-provided source label so incidents identify the failing bootstrap
//! stage instead of collapsing into an anonymous SQLx error.
//!
//! #5993: the Discord DB-down alert that used to fire on exhaustion (sent to
//! the retired kanban human-alert channel) is gone. The DB-down signals are
//! the stderr line [`PgBootstrapFailure::exhaustion_line`] written right
//! before `exit(1)`. A database already ahead of this binary is not retried:
//! dcserver holds ([`hold_while_schema_ahead`]) until its install path holds a new file.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::db::postgres::{MigrationStatus, PgConnectFailure, PgConnectFailureKind};

/// Number of retries after the initial connect attempt. Total attempts =
/// `1 + MAX_RETRIES` = 6, with retry delays `1,2,4,8,16s`.
pub(crate) const MAX_RETRIES: u32 = 5;
/// Base delay for the first retry, in seconds. Doubles each subsequent retry.
pub(crate) const BACKOFF_BASE_SECS: u64 = 1;
/// Upper bound on any single backoff delay, in seconds. This is the guard the
/// #4379 mutation test targets: removing the `.min(BACKOFF_CAP_SECS)` clamp in
/// [`backoff_delay`] must make [`backoff_delay`]'s cap assertion FAIL.
pub(crate) const BACKOFF_CAP_SECS: u64 = 30;
/// `0` restores the plain retry-then-exit path for a schema-ahead database.
pub(crate) const SCHEMA_AHEAD_HOLD_ENV: &str = "AGENTDESK_SCHEMA_AHEAD_HOLD";
const SCHEMA_AHEAD_POLL: Duration = Duration::from_secs(30);
/// Polls between reminder lines while holding (10 minutes).
const SCHEMA_AHEAD_REMINDER_POLLS: u64 = 20;
fn pool_acquire_timeout_diagnostic(
    timestamp: &str,
    source: &str,
    attempt: Option<u32>,
    error: &PgConnectFailure,
) -> Option<String> {
    (error.kind() == PgConnectFailureKind::PoolTimedOut).then(|| {
        format!(
            "[{timestamp}] level=ERROR event=postgres_pool_acquire_timeout source={source} attempt={} error={error}",
            attempt
                .map(|value| value.to_string())
                .unwrap_or_else(|| "n/a".to_string())
        )
    })
}

fn log_pool_acquire_timeout(source: &str, attempt: Option<u32>, error: &PgConnectFailure) {
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Some(diagnostic) = pool_acquire_timeout_diagnostic(&timestamp, source, attempt, error) {
        eprintln!("  ✖ {diagnostic}");
    }
}

/// Outcome of an exhausted [`connect_with_backoff`] loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgBootstrapFailure {
    /// Human-readable description of the final failure (connect error text, or
    /// the "PostgreSQL is required" message for an `Ok(None)`).
    pub last_error: String,
    /// Total number of connect attempts made (`1 + MAX_RETRIES` when exhausted).
    pub attempts: u32,
    /// The loop stopped early because the database is ahead of this binary.
    pub schema_ahead: bool,
}

impl PgBootstrapFailure {
    /// The one stderr line dcserver writes before `exit(1)` once the retry
    /// budget is spent. Operators and log scans key on this prefix, so keep the
    /// wording stable.
    pub(crate) fn exhaustion_line(&self) -> String {
        format!(
            "PostgreSQL connect failed after {} attempt(s): {}",
            self.attempts, self.last_error
        )
    }
}

/// Exponential backoff delay for the given 1-based `retry` number, capped at
/// [`BACKOFF_CAP_SECS`].
///
/// `retry = 1 → 1s`, `2 → 2s`, `3 → 4s`, `4 → 8s`, `5 → 16s`, and any `retry`
/// whose doubled base would exceed the cap saturates at [`BACKOFF_CAP_SECS`]
/// (e.g. `retry = 6 → 30s`, not 32s). Pure and total — the `<< 63` clamp and
/// the `saturating_mul` prevent overflow for absurd inputs.
pub(crate) fn backoff_delay(retry: u32) -> Duration {
    let shift = retry.saturating_sub(1).min(63);
    let raw = BACKOFF_BASE_SECS.saturating_mul(1u64 << shift);
    Duration::from_secs(raw.min(BACKOFF_CAP_SECS))
}

/// Connect to PostgreSQL with bounded exponential backoff.
///
/// `connect` is invoked once per attempt (the seam under test — the real caller
/// passes a closure over `crate::db::postgres::connect`). `Ok(Some(pool))`
/// returns immediately; `Ok(None)` (PG disabled/misconfigured) and `Err(..)`
/// (connect/health-check failure) both trigger a retry until the budget is
/// exhausted. `sleep` is the injectable delay (real caller passes
/// `tokio::time::sleep`; tests pass a recorder that advances no clock).
///
/// On exhaustion returns [`PgBootstrapFailure`] carrying the last observed
/// error and the total attempt count. A schema-ahead failure returns at once
/// when `hold_on_schema_ahead` (asked only on that failure) says so.
pub(crate) async fn connect_with_backoff<T, C, CFut, S, SFut, H>(
    mut connect: C,
    mut sleep: S,
    mut hold_on_schema_ahead: H,
    source: &str,
) -> Result<T, PgBootstrapFailure>
where
    C: FnMut() -> CFut,
    CFut: Future<Output = Result<Option<T>, PgConnectFailure>>,
    S: FnMut(Duration) -> SFut,
    SFut: Future<Output = ()>,
    H: FnMut() -> bool,
{
    let mut last_error = String::new();
    for attempt in 0..=MAX_RETRIES {
        match connect().await {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {
                last_error = "PostgreSQL is required for Discord HTTP runtime".to_string();
            }
            Err(error) => {
                log_pool_acquire_timeout(source, Some(attempt + 1), &error);
                last_error = error.to_string();
                if error.kind() == PgConnectFailureKind::SchemaAhead && hold_on_schema_ahead() {
                    return Err(PgBootstrapFailure {
                        last_error,
                        attempts: attempt + 1,
                        schema_ahead: true,
                    });
                }
            }
        }
        // Sleep only *between* attempts — never after the final one, so we do
        // not burn a pointless 16s before exiting.
        if attempt < MAX_RETRIES {
            sleep(backoff_delay(attempt + 1)).await;
        }
    }
    Err(PgBootstrapFailure {
        last_error,
        attempts: MAX_RETRIES + 1,
        schema_ahead: false,
    })
}

/// Whether the launchd environment leaves the schema-ahead hold on (default on).
pub(crate) fn schema_ahead_hold_enabled() -> bool {
    std::env::var(SCHEMA_AHEAD_HOLD_ENV)
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

/// Describes a database whose applied migrations are all newer than this
/// binary's latest; a gap below that latest is drift, not "ahead".
fn schema_ahead_detail(status: &MigrationStatus) -> Option<String> {
    let latest = *status.resolved_versions.iter().max()?;
    let ahead = &status.missing_from_resolved;
    if ahead.is_empty() || ahead.iter().any(|version| *version <= latest) {
        return None;
    }
    Some(format!(
        "database has migration(s) {ahead:?} newer than this binary's latest embedded migration {latest}"
    ))
}

/// What a file looked like; a rename over the path yields a different value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BinaryIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl BinaryIdentity {
    pub(crate) fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        })
    }
}

/// The install path this process started from and its identity at startup.
#[derive(Clone, Debug)]
pub(crate) struct BinaryWatch {
    path: PathBuf,
    identity: BinaryIdentity,
}

impl BinaryWatch {
    /// Taken at startup so a replacement before the first bootstrap failure still counts.
    pub(crate) fn capture_current_exe() -> Option<Self> {
        Self::capture(std::env::current_exe().ok()?)
    }

    pub(crate) fn capture(path: PathBuf) -> Option<Self> {
        let identity = BinaryIdentity::of(&path).ok()?;
        Some(Self { path, identity })
    }
}

/// Holds a dcserver that cannot boot on the shared schema until a different file is
/// renamed over its install path; a failed stat is not a replacement.
pub(crate) async fn hold_while_schema_ahead<P, S, SFut>(
    watch: &BinaryWatch,
    detail: &str,
    mut probe: P,
    mut sleep: S,
) where
    P: FnMut(&Path) -> std::io::Result<BinaryIdentity>,
    S: FnMut(Duration) -> SFut,
    SFut: Future<Output = ()>,
{
    eprintln!(
        "  ✖ schema ahead of binary: {detail}; holding without exit until {} is replaced ({SCHEMA_AHEAD_HOLD_ENV}=0 in the launchd environment exits instead after a restart)",
        watch.path.display()
    );
    let mut polls: u64 = 0;
    loop {
        sleep(SCHEMA_AHEAD_POLL).await;
        polls += 1;
        let remind = polls % SCHEMA_AHEAD_REMINDER_POLLS == 0;
        match probe(&watch.path) {
            Ok(identity) if identity != watch.identity => {
                eprintln!(
                    "  ▸ {} was replaced; exiting so launchd starts the new binary",
                    watch.path.display()
                );
                return;
            }
            Ok(_) if remind => eprintln!("  ✖ still holding: schema ahead of binary: {detail}"),
            Err(error) if remind => eprintln!(
                "  ⚠ cannot stat {} ({error}); still holding",
                watch.path.display()
            ),
            _ => {}
        }
    }
}

/// Run migration, config reconciliation, and reseeding on the eager startup
/// pool. The caller places this whole operation inside the retry envelope.
pub(crate) async fn initialize_postgres_for_bootstrap(
    pool: &sqlx::PgPool,
    mut config: crate::config::Config,
    runtime_root: Option<&Path>,
    legacy_scan: &crate::services::discord_config_audit::LegacySourceScan,
) -> Result<crate::config::Config, PgConnectFailure> {
    let initialized = crate::db::postgres::with_startup_advisory_lock(pool, || async {
        crate::db::postgres::migrate(pool).await?;
        if let Some(root) = runtime_root {
            let loaded = crate::services::discord_config_audit::load_runtime_config(root)?;
            config = crate::services::discord_config_audit::audit_and_reconcile_config_only(
                root,
                loaded.config,
                loaded.path,
                loaded.existed,
                legacy_scan,
                false,
            )?
            .config;
        }
        crate::db::postgres::startup_reseed_with_warmup_pool(pool, &config).await
    })
    .await;
    if let Err(error) = initialized {
        // Only a failure is classified, so a healthy boot runs no extra query.
        if let Ok(status) = crate::db::postgres::migration_status(pool).await {
            if let Some(detail) = schema_ahead_detail(&status) {
                return Err(PgConnectFailure::schema_ahead(format!(
                    "postgres startup initialization: {detail} ({error})"
                )));
            }
        }
        return Err(PgConnectFailure::other(format!(
            "postgres startup initialization: {error}"
        )));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn backoff_delay_follows_exponential_schedule() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(8));
        assert_eq!(backoff_delay(5), Duration::from_secs(16));
    }

    #[test]
    fn backoff_delay_saturates_at_cap() {
        // MUTATION GUARD (#4379): `retry = 6` doubles the base to 32s, which the
        // `.min(BACKOFF_CAP_SECS)` clamp must pull down to 30s. Deleting that
        // clamp makes this assert observe 32s and FAIL — the guard is proven by
        // this test's own assertion, not by a compile error.
        assert_eq!(backoff_delay(6), Duration::from_secs(BACKOFF_CAP_SECS));
        assert_eq!(backoff_delay(20), Duration::from_secs(BACKOFF_CAP_SECS));
        // Absurd input must not panic (overflow guard).
        assert_eq!(
            backoff_delay(u32::MAX),
            Duration::from_secs(BACKOFF_CAP_SECS)
        );
    }

    /// Drives `connect_with_backoff` with fully synchronous fakes: a scripted
    /// connect and a sleep recorder, so no real clock or runtime is involved.
    fn run_backoff<T: Clone + 'static>(
        results: Vec<Result<Option<T>, PgConnectFailure>>,
    ) -> (Result<T, PgBootstrapFailure>, Vec<Duration>, usize) {
        let (result, slept, calls, hold_asks) = run_backoff_with_hold(results, true);
        assert_eq!(
            hold_asks, 0,
            "only a schema-ahead failure asks about the hold"
        );
        (result, slept, calls)
    }

    /// Like `run_backoff`, with the schema-ahead hold answer and the number of
    /// times the loop asked for it.
    fn run_backoff_with_hold<T: Clone + 'static>(
        results: Vec<Result<Option<T>, PgConnectFailure>>,
        hold: bool,
    ) -> (Result<T, PgBootstrapFailure>, Vec<Duration>, usize, usize) {
        let hold_asks = Rc::new(RefCell::new(0usize));
        let hold_asks_c = hold_asks.clone();
        let script = Rc::new(RefCell::new(results.into_iter()));
        let calls = Rc::new(RefCell::new(0usize));
        let slept: Rc<RefCell<Vec<Duration>>> = Rc::new(RefCell::new(Vec::new()));

        let script_c = script.clone();
        let calls_c = calls.clone();
        let slept_c = slept.clone();

        // The loop is a pure state machine over `.await` points that never
        // yield to a real reactor (the fakes are ready immediately), so
        // `now_or_never` resolves it synchronously.
        let fut = connect_with_backoff(
            move || {
                *calls_c.borrow_mut() += 1;
                let next = script_c
                    .borrow_mut()
                    .next()
                    .unwrap_or_else(|| Err(PgConnectFailure::other("script exhausted")));
                async move { next }
            },
            move |d: Duration| {
                slept_c.borrow_mut().push(d);
                async move {}
            },
            move || {
                *hold_asks_c.borrow_mut() += 1;
                hold
            },
            "cli::dcserver_pg_bootstrap::tests",
        );
        let result = futures::executor::block_on(fut);
        let slept_vec = slept.borrow().clone();
        let call_count = *calls.borrow();
        let asks = *hold_asks.borrow();
        (result, slept_vec, call_count, asks)
    }

    #[test]
    fn connect_returns_immediately_on_first_success() {
        let (result, slept, calls) = run_backoff(vec![Ok(Some(42u32))]);
        assert_eq!(result, Ok(42));
        assert!(slept.is_empty(), "no backoff sleep on immediate success");
        assert_eq!(calls, 1);
    }

    #[test]
    fn connect_retries_then_succeeds_recording_backoff() {
        // Fail (Err), fail (Ok(None)), then succeed on the 3rd attempt.
        let (result, slept, calls) = run_backoff(vec![
            Err(PgConnectFailure::other("pool timed out")),
            Ok(None),
            Ok(Some(7u32)),
        ]);
        assert_eq!(result, Ok(7));
        assert_eq!(calls, 3);
        // Two sleeps preceded attempts 2 and 3: 1s then 2s.
        assert_eq!(slept, vec![Duration::from_secs(1), Duration::from_secs(2)]);
    }

    #[test]
    fn connect_exhausts_budget_and_reports_last_error() {
        // Always fail: 6 attempts total, 5 backoff sleeps 1→2→4→8→16.
        let (result, slept, calls) = run_backoff::<u32>(vec![
            Err(PgConnectFailure::other("e1")),
            Err(PgConnectFailure::other("e2")),
            Err(PgConnectFailure::other("e3")),
            Err(PgConnectFailure::other("e4")),
            Err(PgConnectFailure::other("e5")),
            Err(PgConnectFailure::other("final boom")),
        ]);
        assert_eq!(
            result,
            Err(PgBootstrapFailure {
                last_error: "final boom".to_string(),
                attempts: 6,
                schema_ahead: false,
            })
        );
        assert_eq!(calls, 6, "1 initial + MAX_RETRIES attempts");
        assert_eq!(
            slept,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
            ]
        );
    }

    #[test]
    fn slow_startup_timeout_exhausts_retries_and_reports_the_exit_line() {
        // #5993: with the Discord DB-down alert retired, the exhaustion line is
        // the dcserver-side DB-down signal. It must name the attempt count and
        // carry the final error so the stderr log alone explains the exit.
        let calls = Rc::new(RefCell::new(0usize));
        let calls_c = calls.clone();

        let failure = futures::executor::block_on(connect_with_backoff(
            move || {
                *calls_c.borrow_mut() += 1;
                async {
                    Err::<Option<u32>, _>(PgConnectFailure::from_sqlx(
                        "connect postgres startup/migrate pool",
                        sqlx::Error::PoolTimedOut,
                    ))
                }
            },
            |_delay| async {},
            || true,
            "cli::dcserver::postgres_startup_and_runtime",
        ))
        .unwrap_err();

        assert_eq!(
            *calls.borrow(),
            6,
            "slow startup uses the full retry budget"
        );
        let line = failure.exhaustion_line();
        assert!(
            line.starts_with("PostgreSQL connect failed after 6 attempt(s): "),
            "stable exit-line prefix, got: {line}"
        );
        assert!(
            line.contains("connect postgres startup/migrate pool"),
            "exit line carries the final error, got: {line}"
        );
    }

    #[test]
    fn pool_timeout_diagnostic_includes_timestamp_source_and_attempt() {
        let pool_timeout =
            PgConnectFailure::from_sqlx("connect postgres", sqlx::Error::PoolTimedOut);
        let diagnostic = pool_acquire_timeout_diagnostic(
            "2026-07-14T12:34:56.789Z",
            "cli::dcserver::postgres_startup_and_runtime",
            Some(3),
            &pool_timeout,
        )
        .expect("pool timeout diagnostic");

        assert!(diagnostic.contains("[2026-07-14T12:34:56.789Z]"));
        assert!(diagnostic.contains("event=postgres_pool_acquire_timeout"));
        assert!(diagnostic.contains("source=cli::dcserver::postgres_startup_and_runtime"));
        assert!(diagnostic.contains("attempt=3"));
        assert!(
            pool_acquire_timeout_diagnostic(
                "2026-07-14T12:34:56.789Z",
                "cli::dcserver::postgres_startup_and_runtime",
                Some(1),
                &PgConnectFailure::other("connect postgres: connection refused")
            )
            .is_none()
        );
    }

    #[test]
    fn exhausted_ok_none_reports_required_message() {
        let (result, _slept, _calls) = run_backoff::<u32>(vec![
            Ok(None),
            Ok(None),
            Ok(None),
            Ok(None),
            Ok(None),
            Ok(None),
        ]);
        let failure = result.unwrap_err();
        assert_eq!(failure.attempts, 6);
        assert!(
            failure.last_error.contains("PostgreSQL is required"),
            "Ok(None) exhaustion surfaces the required-message, got: {}",
            failure.last_error
        );
    }

    fn schema_ahead_failure() -> PgConnectFailure {
        PgConnectFailure::schema_ahead("database has migration(s) [136] newer than 135")
    }

    /// A database ahead of the binary cannot become bootable by retrying, so the
    /// loop stops at once while the hold is on and keeps the old schedule when off.
    #[test]
    fn a_schema_ahead_failure_stops_at_once_only_while_the_hold_is_on() {
        let (result, slept, calls, asks) =
            run_backoff_with_hold::<u32>(vec![Err(schema_ahead_failure())], true);
        let failure = result.unwrap_err();
        assert!(failure.schema_ahead, "hold on: {failure:?}");
        assert_eq!((calls, asks, failure.attempts), (1, 1, 1));
        assert!(slept.is_empty(), "no backoff before holding: {slept:?}");

        let (result, slept, calls, _) =
            run_backoff_with_hold::<u32>(vec![Err(schema_ahead_failure()); 6], false);
        let failure = result.unwrap_err();
        assert!(!failure.schema_ahead, "hold off: {failure:?}");
        assert_eq!(calls, 6, "hold off keeps every retry");
        assert_eq!(
            slept,
            [1, 2, 4, 8, 16].map(Duration::from_secs).to_vec(),
            "hold off keeps the backoff schedule"
        );
    }

    fn status(resolved: &[i64], missing: &[i64]) -> MigrationStatus {
        MigrationStatus {
            applied: Vec::new(),
            resolved_versions: resolved.to_vec(),
            missing_from_resolved: missing.to_vec(),
            pending_versions: Vec::new(),
        }
    }

    /// Only versions entirely past the binary's latest are "ahead"; a gap below
    /// it is drift that a newer deploy does not explain.
    #[test]
    fn schema_ahead_needs_every_unknown_version_past_the_latest() {
        assert!(schema_ahead_detail(&status(&[1, 2, 3], &[4, 5])).is_some());
        assert!(schema_ahead_detail(&status(&[1, 2, 3], &[0, 4])).is_none());
        assert!(schema_ahead_detail(&status(&[1, 2, 3], &[])).is_none());
        assert!(schema_ahead_detail(&status(&[], &[4])).is_none());
    }

    /// Writes `bytes` beside `path` and renames it over, as the deploy does.
    fn rename_over(path: &Path, bytes: &[u8]) {
        let staged = path.with_extension("staged");
        std::fs::write(&staged, bytes).expect("write staged binary");
        std::fs::rename(&staged, path).expect("rename staged binary over the install path");
    }

    async fn hold_returns_within_an_hour(watch: &BinaryWatch) -> bool {
        tokio::time::timeout(
            Duration::from_secs(3600),
            hold_while_schema_ahead(watch, "ahead", BinaryIdentity::of, tokio::time::sleep),
        )
        .await
        .is_ok()
    }

    /// The hold survives an unchanged file and a missing path, and ends when a
    /// same-sized file is renamed over the install path.
    #[tokio::test(start_paused = true)]
    async fn hold_ends_only_when_the_install_path_holds_a_new_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agentdesk");
        std::fs::write(&path, b"old-binary").expect("write binary");
        let watch = BinaryWatch::capture(path.clone()).expect("capture");

        assert!(
            !hold_returns_within_an_hour(&watch).await,
            "unchanged file must keep holding"
        );

        std::fs::remove_file(&path).expect("remove binary");
        assert!(
            !hold_returns_within_an_hour(&watch).await,
            "a missing path is not a replacement"
        );

        rename_over(&path, b"new-binary");
        assert!(
            hold_returns_within_an_hour(&watch).await,
            "a renamed-in binary ends the hold"
        );
    }

    /// The baseline is the file at startup, so a binary installed before the
    /// first bootstrap failure still ends the hold.
    #[tokio::test(start_paused = true)]
    async fn hold_ends_for_a_binary_replaced_before_it_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agentdesk");
        std::fs::write(&path, b"old-binary").expect("write binary");
        let watch = BinaryWatch::capture(path.clone()).expect("capture");
        rename_over(&path, b"new-binary");
        assert!(hold_returns_within_an_hour(&watch).await);
    }

    /// A migration newer than this binary is SchemaAhead; an unknown version below
    /// the latest stays an ordinary failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_initialization_reports_a_database_ahead_of_the_binary_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let latest = crate::db::postgres::migration_status(&pool)
            .await
            .expect("migration status")
            .resolved_versions
            .into_iter()
            .max()
            .expect("embedded migrations");

        let initialize_with_unknown = |version: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "DELETE FROM _sqlx_migrations WHERE description = 'unknown to this binary'",
                )
                .execute(&pool)
                .await
                .expect("clear unknown row");
                sqlx::query(
                    "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                     VALUES ($1, 'unknown to this binary', TRUE, '\\x00'::bytea, 0)",
                )
                .bind(version)
                .execute(&pool)
                .await
                .expect("insert unknown migration row");
                initialize_postgres_for_bootstrap(
                    &pool,
                    crate::config::Config::default(),
                    None,
                    &crate::services::discord_config_audit::LegacySourceScan::default(),
                )
                .await
                .expect_err("an unknown applied migration must fail startup")
            }
        };

        let ahead = initialize_with_unknown(latest + 1).await;
        assert_eq!(ahead.kind(), PgConnectFailureKind::SchemaAhead, "{ahead}");
        assert!(
            ahead.to_string().contains(&format!("[{}]", latest + 1)),
            "{ahead}"
        );

        let gap = initialize_with_unknown(0).await;
        assert_eq!(gap.kind(), PgConnectFailureKind::Other, "{gap}");

        pool.close().await;
        pg_db.drop().await;
    }
}
