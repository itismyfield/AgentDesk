//! #4379 — bounded exponential backoff + PG-independent Discord alert for the
//! dcserver PostgreSQL bootstrap.
//!
//! Before this module, `dcserver` called `crate::db::postgres::connect(..)`
//! exactly once at boot and `std::process::exit(1)` on the first failure. Under
//! launchd (`KeepAlive=true`, `ThrottleInterval=5`) a transient DB blip (tunnel
//! reconnect, PG restart, failover) became a ~8s tight crash loop that flooded
//! the stderr log (3.2 MB of one line on 2026-07-09) and left Discord relay
//! silently dead for ~30 minutes with zero operator signal.
//!
//! This module adds two boot-only behaviours, both deliberately minimal (no
//! "degraded startup" that would boot the gateway without a PG pool — that is a
//! `run_bot(pg_pool: Some(..))` signature overhaul tracked separately):
//!
//! 1. [`connect_with_backoff`] wraps the connect attempt in a bounded
//!    exponential backoff (1 initial attempt + up to [`MAX_RETRIES`] retries,
//!    delays `1→2→4→8→16s` capped at [`BACKOFF_CAP_SECS`]). This alone removes
//!    the tight launchd loop: each attempt runs real migration/startup work on
//!    an eager pool with a 10s deadline, then activates the eager runtime pool
//!    with its fast 3s acquire timeout. A PG that recovers mid-backoff boots
//!    cleanly; failures in either phase exhaust through the same alert path.
//!    Pool-acquire timeouts are logged per attempt with an RFC3339 timestamp and
//!    the caller-provided source label so future incidents identify the failing
//!    bootstrap stage instead of collapsing into an anonymous SQLx error.
//! 2. [`notify_pg_unavailable`] fires a single Discord alert on retry
//!    exhaustion, using bot tokens + `human_alert_channel_id` already loaded
//!    into memory *before* the PG connect (so no PG is required to alert — see
//!    [`send_pg_alert`]). The alert pipeline ([`attempt_alert_with_deadline`])
//!    is ordered so that no failure mode can either spam the channel or
//!    silently disarm the signal:
//!    1. read the rate-limit state file — suppressed while a prior alert is
//!       within [`ALERT_RATELIMIT_SECS`];
//!    2. write the attempt-stamp **before** sending (a crash mid-send or a
//!       success followed by a failed write must not re-spam every boot); a
//!       failed write logs a WARN and **still sends** — fail-open, because this
//!       issue exists to abolish silence, and repeated alerts under a broken
//!       state file are themselves the signal of a second failure;
//!    3. try each candidate token sequentially until one delivers
//!       ([`candidate_alert_tokens`] covers both configured bots and the
//!       CLI/env single-token boot mode);
//!    4. if every token fails (or the boundary deadline fires), roll the
//!       attempt-stamp back so the *next* bounded boot retry cycle tries again — an
//!       undelivered alert must not consume the 900s suppression window;
//!    5. on success the stamp stays, giving the normal 900s suppression.
//!
//!    The whole pipeline is bounded by [`ALERT_SEND_TIMEOUT_SECS`] at this
//!    call boundary — not inside the shared `discord_io`/transport client —
//!    following the #4391 lesson: a deadline composed at the boundary stays
//!    true for this path regardless of how the shared client evolves, and the
//!    shared client keeps serving callers with different latency budgets.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::db::postgres::{PgConnectFailure, PgConnectFailureKind};

/// Number of retries after the initial connect attempt. Total attempts =
/// `1 + MAX_RETRIES` = 6, with retry delays `1,2,4,8,16s`.
pub(crate) const MAX_RETRIES: u32 = 5;
/// Base delay for the first retry, in seconds. Doubles each subsequent retry.
pub(crate) const BACKOFF_BASE_SECS: u64 = 1;
/// Upper bound on any single backoff delay, in seconds. This is the guard the
/// #4379 mutation test targets: removing the `.min(BACKOFF_CAP_SECS)` clamp in
/// [`backoff_delay`] must make [`backoff_delay`]'s cap assertion FAIL.
pub(crate) const BACKOFF_CAP_SECS: u64 = 30;
/// Minimum interval between two PG-unavailable Discord alerts. Persisted across
/// process restarts via the state file so a launchd crash loop cannot re-spam.
pub(crate) const ALERT_RATELIMIT_SECS: u64 = 900;
/// Basename of the rate-limit state file under `<runtime_root>/logs/`.
const ALERT_STATE_FILE: &str = "dcserver-pg-alert.state";
/// Upper bound on the whole alert pipeline (all token attempts combined),
/// enforced at this call boundary via `tokio::time::timeout` in
/// [`attempt_alert_with_deadline`]. Without it a hung Discord REST/DNS/TCP
/// exchange would stall `notify_pg_unavailable().await` forever and break the
/// launchd restart cycle right when the operator most needs it.
pub(crate) const ALERT_SEND_TIMEOUT_SECS: u64 = 15;
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
/// error and the total attempt count.
pub(crate) async fn connect_with_backoff<T, C, CFut, S, SFut>(
    mut connect: C,
    mut sleep: S,
    source: &str,
) -> Result<T, PgBootstrapFailure>
where
    C: FnMut() -> CFut,
    CFut: Future<Output = Result<Option<T>, PgConnectFailure>>,
    S: FnMut(Duration) -> SFut,
    SFut: Future<Output = ()>,
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
    })
}

/// Run the bounded bootstrap loop and notify exactly once on exhaustion.
///
/// Keeping notification in this wrapper prevents callers from adding an
/// unalerted exit path after a startup connection or migration failure.
pub(crate) async fn connect_with_backoff_and_notify<T, C, CFut, S, SFut, N, NFut>(
    connect: C,
    sleep: S,
    source: &str,
    notify: N,
) -> Result<T, PgBootstrapFailure>
where
    C: FnMut() -> CFut,
    CFut: Future<Output = Result<Option<T>, PgConnectFailure>>,
    S: FnMut(Duration) -> SFut,
    SFut: Future<Output = ()>,
    N: FnOnce(PgBootstrapFailure) -> NFut,
    NFut: Future<Output = ()>,
{
    match connect_with_backoff(connect, sleep, source).await {
        Ok(value) => Ok(value),
        Err(failure) => {
            notify(failure.clone()).await;
            Err(failure)
        }
    }
}

/// Run migration, config reconciliation, and reseeding on the eager startup
/// pool. The caller places this whole operation inside the retry/alert envelope.
pub(crate) async fn initialize_postgres_for_bootstrap(
    pool: &sqlx::PgPool,
    mut config: crate::config::Config,
    runtime_root: Option<&Path>,
    legacy_scan: &crate::services::discord_config_audit::LegacySourceScan,
) -> Result<crate::config::Config, PgConnectFailure> {
    crate::db::postgres::with_startup_advisory_lock(pool, || async {
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
    .await
    .map_err(|error| {
        PgConnectFailure::other(format!("postgres startup initialization: {error}"))
    })?;
    Ok(config)
}

/// Pure rate-limit predicate: should an alert be sent `now` given the
/// `last_sent` timestamp and the minimum `interval`?
///
/// `None` (never sent) always allows. Otherwise the alert is allowed only once
/// `interval` has fully elapsed. A `last_sent` in the future (clock skew /
/// corrupt state) is treated as "allow" so a bad state file cannot wedge alerts
/// off forever.
///
/// The `elapsed >= interval` comparison is the guard the #4379 rate-limit
/// mutation test targets: weakening it to always-true must make the
/// "second call within window is suppressed" assertion FAIL.
pub(crate) fn should_send_alert(
    now: SystemTime,
    last_sent: Option<SystemTime>,
    interval: Duration,
) -> bool {
    match last_sent {
        None => true,
        Some(prev) => match now.duration_since(prev) {
            Ok(elapsed) => elapsed >= interval,
            Err(_) => true,
        },
    }
}

/// Read the last-alert timestamp from `path`, if the file exists and parses.
fn read_last_alert(path: &Path) -> Option<SystemTime> {
    let contents = std::fs::read_to_string(path).ok()?;
    let secs: u64 = contents.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Persist `now` as the last-alert timestamp, creating parent dirs as needed.
fn write_last_alert(path: &Path, now: SystemTime) -> std::io::Result<()> {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, secs.to_string())
}

/// Build the ordered, deduplicated list of candidate bot tokens for the
/// DB-down alert: every configured launch-config token, then the CLI/env
/// single token (`agentdesk dcserver <TOKEN>` / `AGENTDESK_TOKEN`), which is
/// the *only* token present in single-token boot mode. Blank entries are
/// dropped; order is preserved on dedupe. The alert path tries these
/// sequentially until one delivers, so a first token without access to the
/// alert channel no longer kills the whole notification.
pub(crate) fn candidate_alert_tokens<'a, I>(
    launch_config_tokens: I,
    single_token: Option<&'a str>,
) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut out: Vec<String> = Vec::new();
    for token in launch_config_tokens.into_iter().chain(single_token) {
        let token = token.trim();
        if token.is_empty() || out.iter().any(|existing| existing == token) {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

/// Outcome of one run of the alert pipeline. `PartialEq` so tests assert on it
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlertAttempt {
    /// A prior alert is still within the rate-limit window — nothing sent.
    Suppressed,
    /// Delivered with this token; the attempt-stamp stays to arm suppression.
    Sent { token: String },
    /// Every candidate token failed; the attempt-stamp was rolled back so the
    /// next boot retries.
    AllTokensFailed { last_error: String },
    /// The boundary deadline fired mid-pipeline; the attempt-stamp was rolled
    /// back so the next boot retries.
    TimedOut,
}

/// Core alert pipeline with an injected `send` seam (real caller sends over
/// Discord REST; tests inject recorders/failures/pending futures).
///
/// Step order is the load-bearing design (defects 3+4 of the #4379 review are
/// in tension, so sequencing resolves them):
/// 1. rate-limit read — suppressed inside the window, stop;
/// 2. attempt-stamp written **before** the send, so a crash mid-send or a
///    post-success write failure can never re-spam every boot. If the write
///    itself fails we WARN and **send anyway** (fail-open): this issue exists
///    to abolish silent DB outages, and an alert repeating each bounded boot cycle
///    under a broken state file is itself the visible signal of that second
///    failure — silence would hide both;
/// 3. sequential sends across the candidate tokens until one succeeds;
/// 4. on total failure, roll the stamp back (best-effort delete) so the next
///    boot retries — a failed send must not consume the 900s window. Repeated
///    send failures then retry per boot cycle, but zero messages actually
///    reach Discord, so this is not user-visible spam;
/// 5. on success, keep the stamp → normal 900s suppression.
pub(crate) async fn attempt_alert<S, SFut>(
    state_path: Option<&Path>,
    now: SystemTime,
    interval: Duration,
    tokens: &[String],
    mut send: S,
) -> AlertAttempt
where
    S: FnMut(String) -> SFut,
    SFut: Future<Output = Result<(), String>>,
{
    if let Some(path) = state_path {
        if !should_send_alert(now, read_last_alert(path), interval) {
            return AlertAttempt::Suppressed;
        }
        // Attempt-stamp BEFORE the send (see step 2 in the doc above). The
        // fail-open on write error is the guard the #4379 fail-open mutation
        // test targets: bailing out here instead of sending must make that
        // test's "send was attempted" assertion FAIL.
        if let Err(error) = write_last_alert(path, now) {
            eprintln!(
                "  ⚠ [pg-bootstrap] alert state persistence failing ({error}) — alert may repeat every boot; sending anyway (fail-open: silence would hide both failures)"
            );
        }
    }

    let mut last_error = "no candidate bot token".to_string();
    for token in tokens {
        match send(token.clone()).await {
            Ok(()) => {
                return AlertAttempt::Sent {
                    token: token.clone(),
                };
            }
            Err(error) => {
                eprintln!(
                    "  ⚠ [pg-bootstrap] DB-down alert send failed with one token: {error} — trying next candidate"
                );
                last_error = error;
            }
        }
    }

    // Rollback (step 4): an undelivered alert must not arm the suppression
    // window. This rollback is the guard the #4379 rollback mutation test
    // targets: removing it leaves the stamp in place and makes the "retry
    // after failure is allowed" assertion FAIL.
    if let Some(path) = state_path {
        rollback_attempt_stamp(path);
    }
    AlertAttempt::AllTokensFailed { last_error }
}

/// [`attempt_alert`] bounded by [`ALERT_SEND_TIMEOUT_SECS`] at this call
/// boundary. Deliberately *not* implemented inside `discord_io`/the shared
/// transport (they serve many callers with different budgets and are
/// off-limits progress tracks): per the #4391 lesson, a deadline enforced at
/// the boundary is compositionally true — it holds no matter how the inner
/// client behaves or evolves. On timeout the attempt-stamp is rolled back
/// (the inner future is dropped mid-flight and cannot do it itself) so the
/// next boot retries.
pub(crate) async fn attempt_alert_with_deadline<S, SFut>(
    state_path: Option<&Path>,
    now: SystemTime,
    interval: Duration,
    tokens: &[String],
    send: S,
) -> AlertAttempt
where
    S: FnMut(String) -> SFut,
    SFut: Future<Output = Result<(), String>>,
{
    match tokio::time::timeout(
        Duration::from_secs(ALERT_SEND_TIMEOUT_SECS),
        attempt_alert(state_path, now, interval, tokens, send),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            if let Some(path) = state_path {
                rollback_attempt_stamp(path);
            }
            AlertAttempt::TimedOut
        }
    }
}

/// Roll back the attempt-stamp so an undelivered alert cannot consume the
/// suppression window (codex #4395 r2: a silently failed rollback left the
/// stamp armed and disarmed the next boot's alert for 900s with no trace).
/// Escalation ladder:
/// 1. delete the file — needs DIRECTORY write permission;
/// 2. on delete failure, overwrite it with a non-numeric sentinel — needs
///    only FILE write permission, and `read_last_alert` fails to parse it,
///    which is the fail-open path (next boot alerts);
/// 3. only when both fail, WARN: the undelivered alert may stay suppressed
///    for up to [`ALERT_RATELIMIT_SECS`] and silence would hide that.
fn rollback_attempt_stamp(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        // Nothing to roll back (e.g. the attempt-stamp write itself already
        // failed and we proceeded fail-open) — do not manufacture a sentinel.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(remove_error) => {
            if let Err(write_error) = std::fs::write(path, "rolled-back") {
                eprintln!(
                    "  ⚠ [pg-bootstrap] alert stamp rollback failed (remove: {remove_error}; \
                     overwrite: {write_error}) — an UNDELIVERED DB-down alert may be \
                     suppressed for up to {ALERT_RATELIMIT_SECS}s"
                );
            }
        }
    }
}

/// Absolute path to the alert rate-limit state file under the runtime root's
/// `logs/` dir (e.g. `~/.adk/release/logs/dcserver-pg-alert.state`). `None` when
/// no runtime root can be resolved (e.g. exotic test envs) — the caller then
/// skips rate-limiting and logs only.
fn alert_state_path() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join("logs").join(ALERT_STATE_FILE))
}

/// Send a single PG-unavailable alert to a Discord channel over the REST API.
///
/// This path is deliberately **PG-independent**: `send_message_to_channel`
/// POSTs directly to Discord's REST API with the already-loaded bot token, so
/// it works even while PostgreSQL is completely unreachable. That is the whole
/// point — the operator gets a signal precisely when the DB is down.
async fn send_pg_alert(bot_token: &str, channel_id: u64, message: &str) -> Result<(), String> {
    crate::services::discord::send_message_to_channel(bot_token, channel_id, message)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Fire the one-shot "PG unavailable" operator alert after retry exhaustion,
/// then return so the caller can `exit(1)`.
///
/// `candidate_tokens` (see [`candidate_alert_tokens`]) and
/// `human_alert_channel_id` are both resolved by the caller from state loaded
/// *before* the PG connect (bot tokens via `HealthRegistry::init_bot_tokens` /
/// `load_discord_bot_launch_configs` / the CLI/env single token, the channel
/// from `config.kanban.human_alert_channel_id`). When either is missing we log
/// and return quietly — an unconfigured `human_alert_channel_id` is the
/// intentional "alerting off" switch (matches the existing kanban-alert
/// convention) and must not spam undeployed environments.
///
/// The whole send pipeline (rate-limit + all token attempts) is bounded by
/// [`ALERT_SEND_TIMEOUT_SECS`] inside [`attempt_alert_with_deadline`], so a
/// hung Discord REST call cannot stall this pre-exit path indefinitely.
pub async fn notify_pg_unavailable(
    candidate_tokens: Vec<String>,
    human_alert_channel_id: Option<&str>,
    last_error: &str,
) {
    let Some(channel_raw) = human_alert_channel_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        eprintln!(
            "  ⚠ [pg-bootstrap] human_alert_channel_id unset — DB-down alert logged only, no Discord notification"
        );
        return;
    };
    let Ok(channel_id) = channel_raw.parse::<u64>() else {
        eprintln!(
            "  ⚠ [pg-bootstrap] human_alert_channel_id ({channel_raw}) is not a valid channel id — skipping Discord alert"
        );
        return;
    };
    if candidate_tokens.is_empty() {
        eprintln!(
            "  ⚠ [pg-bootstrap] no usable Discord bot token loaded — cannot send DB-down alert (log only)"
        );
        return;
    }

    let message = format!(
        "⚠ DB 연결 불가 — 릴레이 중단, 재시도 중. dcserver가 PostgreSQL에 붙지 못했습니다 (마지막 오류: {last_error}). 복구되면 자동 정상화됩니다."
    );
    // If no state path can be resolved we run un-rate-limited (a single alert
    // is better than silence; the deadline still bounds us).
    let state_path = alert_state_path();
    let outcome = attempt_alert_with_deadline(
        state_path.as_deref(),
        SystemTime::now(),
        Duration::from_secs(ALERT_RATELIMIT_SECS),
        &candidate_tokens,
        |token| {
            let msg = message.clone();
            async move { send_pg_alert(&token, channel_id, &msg).await }
        },
    )
    .await;

    match outcome {
        AlertAttempt::Sent { .. } => {
            eprintln!("  ▸ [pg-bootstrap] DB-down alert sent to channel {channel_id}")
        }
        AlertAttempt::Suppressed => eprintln!(
            "  ⚠ [pg-bootstrap] DB-down alert suppressed (sent within last {ALERT_RATELIMIT_SECS}s)"
        ),
        AlertAttempt::AllTokensFailed { last_error } => eprintln!(
            "  ⚠ [pg-bootstrap] DB-down alert failed with all {} candidate token(s): {last_error} — stamp rolled back, next boot retries",
            candidate_tokens.len()
        ),
        AlertAttempt::TimedOut => eprintln!(
            "  ⚠ [pg-bootstrap] DB-down alert timed out after {ALERT_SEND_TIMEOUT_SECS}s — stamp rolled back, next boot retries"
        ),
    }
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
            "cli::dcserver_pg_bootstrap::tests",
        );
        let result = futures::executor::block_on(fut);
        let slept_vec = slept.borrow().clone();
        let call_count = *calls.borrow();
        (result, slept_vec, call_count)
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
    fn slow_startup_timeout_exhausts_retries_then_notifies() {
        let calls = Rc::new(RefCell::new(0usize));
        let notifications: Rc<RefCell<Vec<PgBootstrapFailure>>> = Rc::new(RefCell::new(Vec::new()));
        let calls_c = calls.clone();
        let notifications_c = notifications.clone();

        let result = futures::executor::block_on(connect_with_backoff_and_notify(
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
            "cli::dcserver::postgres_startup_and_runtime",
            move |failure| {
                notifications_c.borrow_mut().push(failure);
                async {}
            },
        ));

        assert_eq!(
            *calls.borrow(),
            6,
            "slow startup uses the full retry budget"
        );
        assert!(result.is_err());
        assert_eq!(
            notifications.borrow().as_slice(),
            &[result.unwrap_err()],
            "retry exhaustion reaches the operator-alert callback exactly once"
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

    #[test]
    fn should_send_alert_allows_when_never_sent() {
        assert!(should_send_alert(
            UNIX_EPOCH + Duration::from_secs(1000),
            None,
            Duration::from_secs(900)
        ));
    }

    #[test]
    fn should_send_alert_suppresses_within_window() {
        let last = UNIX_EPOCH + Duration::from_secs(1000);
        // 100s later, window is 900s → suppressed.
        let now = UNIX_EPOCH + Duration::from_secs(1100);
        // MUTATION GUARD (#4379): the `elapsed >= interval` comparison in
        // `should_send_alert` is what makes this `false`. Weakening the guard
        // to always-true makes this assert FAIL by its own assertion.
        assert!(!should_send_alert(
            now,
            Some(last),
            Duration::from_secs(900)
        ));
    }

    #[test]
    fn should_send_alert_allows_after_window() {
        let last = UNIX_EPOCH + Duration::from_secs(1000);
        let now = UNIX_EPOCH + Duration::from_secs(1000 + 900);
        assert!(should_send_alert(now, Some(last), Duration::from_secs(900)));
    }

    #[test]
    fn should_send_alert_allows_on_future_last_sent() {
        // Corrupt/future timestamp must not wedge alerting off.
        let last = UNIX_EPOCH + Duration::from_secs(5000);
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        assert!(should_send_alert(now, Some(last), Duration::from_secs(900)));
    }

    /// Unique scratch dir per test so parallel test threads never share state.
    fn scratch_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "adk-pg-alert-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const WINDOW: Duration = Duration::from_secs(900);

    #[test]
    fn candidate_tokens_cover_single_token_boot_and_dedupe() {
        // Single-token boot (`agentdesk dcserver <TOKEN>` / AGENTDESK_TOKEN):
        // launch_configs is empty, so the CLI token must be the candidate —
        // this is the review defect where the alert silently skipped.
        assert_eq!(
            candidate_alert_tokens(std::iter::empty(), Some(" tok-cli ")),
            vec!["tok-cli".to_string()]
        );
        // Configured bots first, CLI token appended, dedupe preserves order,
        // blanks dropped.
        assert_eq!(
            candidate_alert_tokens(vec!["a", "b", "", "a"], Some("b")),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(candidate_alert_tokens(std::iter::empty(), None).is_empty());
    }

    #[test]
    fn single_token_boot_sends_with_that_token() {
        let calls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let calls_c = calls.clone();
        let tokens = candidate_alert_tokens(std::iter::empty(), Some("tok-cli"));
        let outcome = futures::executor::block_on(attempt_alert(
            None,
            UNIX_EPOCH + Duration::from_secs(1000),
            WINDOW,
            &tokens,
            move |token| {
                calls_c.borrow_mut().push(token);
                async { Ok(()) }
            },
        ));
        assert_eq!(
            outcome,
            AlertAttempt::Sent {
                token: "tok-cli".to_string()
            }
        );
        assert_eq!(*calls.borrow(), vec!["tok-cli".to_string()]);
    }

    #[test]
    fn multi_candidate_falls_through_to_second_token() {
        let calls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let calls_c = calls.clone();
        let tokens = vec!["tok-a".to_string(), "tok-b".to_string()];
        let outcome = futures::executor::block_on(attempt_alert(
            None,
            UNIX_EPOCH + Duration::from_secs(1000),
            WINDOW,
            &tokens,
            move |token| {
                calls_c.borrow_mut().push(token.clone());
                async move {
                    if token == "tok-a" {
                        Err("403 missing access".to_string())
                    } else {
                        Ok(())
                    }
                }
            },
        ));
        assert_eq!(
            outcome,
            AlertAttempt::Sent {
                token: "tok-b".to_string()
            }
        );
        assert_eq!(
            *calls.borrow(),
            vec!["tok-a".to_string(), "tok-b".to_string()],
            "tokens tried sequentially in order"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn boundary_deadline_cuts_off_hung_send() {
        // A send that never resolves (hung Discord REST/DNS/TCP). The paused
        // tokio clock auto-advances to the next armed timer, so the 15s
        // boundary deadline fires virtually.
        let tokens = vec!["tok".to_string()];
        let fut = attempt_alert_with_deadline(None, UNIX_EPOCH, WINDOW, &tokens, |_token| {
            std::future::pending::<Result<(), String>>()
        });
        // MUTATION GUARD (#4379): removing the `tokio::time::timeout` boundary
        // in `attempt_alert_with_deadline` leaves no 15s timer — the paused
        // clock then jumps to this outer 1h guard, which errors and makes this
        // `expect` FAIL by its own assertion (not a hang, not a compile error).
        let outcome = tokio::time::timeout(Duration::from_secs(3600), fut)
            .await
            .expect("boundary deadline must bound a hung send well before 1h");
        assert_eq!(outcome, AlertAttempt::TimedOut);
    }

    #[test]
    fn failed_send_rolls_back_stamp_so_next_boot_retries() {
        let dir = scratch_dir("rollback");
        let state = dir.join("dcserver-pg-alert.state");
        let tokens = vec!["tok".to_string()];

        let t0 = UNIX_EPOCH + Duration::from_secs(10_000);
        let outcome = futures::executor::block_on(attempt_alert(
            Some(&state),
            t0,
            WINDOW,
            &tokens,
            |_token| async { Err("network unreachable".to_string()) },
        ));
        assert_eq!(
            outcome,
            AlertAttempt::AllTokensFailed {
                last_error: "network unreachable".to_string()
            }
        );

        // MUTATION GUARD (#4379): the rollback delete in `attempt_alert` is
        // what makes the next boot's attempt allowed. Removing the rollback
        // leaves the attempt-stamp armed, the next call returns Suppressed,
        // and both asserts below FAIL by their own assertions.
        let calls: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
        let calls_c = calls.clone();
        let t1 = t0 + Duration::from_secs(45); // next launchd boot, well inside 900s
        let retry = futures::executor::block_on(attempt_alert(
            Some(&state),
            t1,
            WINDOW,
            &tokens,
            move |_token| {
                *calls_c.borrow_mut() += 1;
                async { Ok(()) }
            },
        ));
        assert_ne!(
            retry,
            AlertAttempt::Suppressed,
            "a failed send must not consume the suppression window"
        );
        assert_eq!(*calls.borrow(), 1, "the retry boot actually sends");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successful_send_keeps_stamp_and_suppresses_next_boot() {
        let dir = scratch_dir("suppress");
        let state = dir.join("dcserver-pg-alert.state");
        let tokens = vec!["tok".to_string()];

        let t0 = UNIX_EPOCH + Duration::from_secs(10_000);
        let outcome = futures::executor::block_on(attempt_alert(
            Some(&state),
            t0,
            WINDOW,
            &tokens,
            |_token| async { Ok(()) },
        ));
        assert!(matches!(outcome, AlertAttempt::Sent { .. }));
        assert!(state.exists(), "stamp kept after successful delivery");

        // Next boot inside the window: suppressed, and send is never invoked.
        let calls: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
        let calls_c = calls.clone();
        let t1 = t0 + Duration::from_secs(45);
        let second = futures::executor::block_on(attempt_alert(
            Some(&state),
            t1,
            WINDOW,
            &tokens,
            move |_token| {
                *calls_c.borrow_mut() += 1;
                async { Ok(()) }
            },
        ));
        assert_eq!(second, AlertAttempt::Suppressed);
        assert_eq!(*calls.borrow(), 0, "suppressed boot must not send");

        // After the window elapses the alert re-arms.
        let t2 = t0 + WINDOW + Duration::from_secs(1);
        let third = futures::executor::block_on(attempt_alert(
            Some(&state),
            t2,
            WINDOW,
            &tokens,
            |_token| async { Ok(()) },
        ));
        assert!(matches!(third, AlertAttempt::Sent { .. }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MUTATION GUARD (codex #4395 r2): when the stamp delete fails (a
    /// read-only state DIR — unlink needs directory write permission), the
    /// rollback must still disarm the suppression window by overwriting the
    /// still-writable stamp FILE with a non-numeric sentinel that
    /// `read_last_alert` refuses to parse (= fail-open next boot). Reverting
    /// `rollback_attempt_stamp` to a bare `remove_file` leaves the stamp
    /// parseable and the assert below FAILs.
    #[cfg(unix)]
    #[test]
    fn rollback_falls_back_to_sentinel_when_delete_is_blocked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("rollback-sentinel");
        let state = dir.join("dcserver-pg-alert.state");
        std::fs::write(&state, "12345").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        rollback_attempt_stamp(&state);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            read_last_alert(&state).is_none(),
            "undelivered alert left a parseable stamp: the next boot would be suppressed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rollback with no stamp present stays a no-op (the stamp write itself
    /// may already have failed fail-open) — it must not manufacture a file.
    #[test]
    fn rollback_without_stamp_is_a_noop() {
        let dir = scratch_dir("rollback-noop");
        let state = dir.join("dcserver-pg-alert.state");
        rollback_attempt_stamp(&state);
        assert!(
            !state.exists(),
            "rollback manufactured a state file from nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamp_write_failure_still_sends_fail_open() {
        // Make `write_last_alert` fail deterministically: the state path's
        // parent is an existing *file*, so `create_dir_all` errors.
        let dir = scratch_dir("failopen");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let state = blocker.join("dcserver-pg-alert.state");

        let calls: Rc<RefCell<usize>> = Rc::new(RefCell::new(0));
        let calls_c = calls.clone();
        let tokens = vec!["tok".to_string()];
        let outcome = futures::executor::block_on(attempt_alert(
            Some(&state),
            UNIX_EPOCH + Duration::from_secs(10_000),
            WINDOW,
            &tokens,
            move |_token| {
                *calls_c.borrow_mut() += 1;
                async { Ok(()) }
            },
        ));

        // MUTATION GUARD (#4379): the fail-open in `attempt_alert` (WARN +
        // continue on a stamp-write error) is what lets this send proceed.
        // Mutating it to bail out on write failure makes both asserts below
        // FAIL by their own assertions.
        assert!(
            matches!(outcome, AlertAttempt::Sent { .. }),
            "stamp persistence failure must not silence the alert, got {outcome:?}"
        );
        assert_eq!(*calls.borrow(), 1, "send attempted despite write failure");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
