//! Rate-limit-aware dispatch gate (feature: rate-limit-aware-dispatch-gate).
//!
//! AgentDesk caches per-provider rate-limit telemetry in the `rate_limit_cache`
//! Postgres table (refreshed every ~120s by `rate_limit_sync_loop`). Before this
//! module, that pressure was observational only: auto-queue activation could
//! still claim work for a provider that is known to be rate-limited, producing
//! doomed turns and wasted recovery cycles.
//!
//! This module adds a **pure, lock-free, in-memory** provider-pressure
//! evaluator plus the process-wide snapshots it reads from. The auto-queue hot
//! dispatch path calls [`evaluate_agent_provider_pressure`] which performs
//! **zero database queries** — it only reads two `RwLock`-guarded snapshots that
//! are refreshed off the hot path by the rate-limit sync loop:
//!
//! * [`PROVIDER_PRESSURE`] — provider -> [`ProviderPressureSnapshot`]
//! * [`AGENT_PROVIDER`]     — agent_id -> provider string
//!
//! Design constraints honored (see PRD/spec + adversarial review):
//! * O(1) in-memory cache read on the dispatch path, no DB round-trip, no
//!   blocking I/O, no probe spawning.
//! * Stale / missing / malformed / unsupported telemetry degrades to
//!   `UnknownAllow` (never blocks unrelated providers, never panics).
//! * Gemini's unknown utilization (used==0, no reset) degrades to
//!   `UnknownAllow`.
//! * The gate enable flag and the gate-specific danger threshold are read live
//!   on every activation. The activation path resolves them from the **persisted
//!   runtime-config** (`kv_meta`, written by `PUT /api/settings/runtime-config`)
//!   first, then falls back to the `config_live_reload::current()` YAML snapshot,
//!   then to the compiled-in defaults. This is what makes the dashboard/API
//!   rollback switch actually take effect at runtime (the persisted toggle never
//!   reaches the YAML live snapshot — see [`evaluate_agent_provider_pressure_with_overrides`]).
//! * The gate uses its OWN danger threshold (`dispatch_rate_limit_gate_danger_pct`,
//!   default 100 — defer only when a provider is fully rate-limited), which is
//!   independent of the dashboard's `rate_limit_danger_pct` (default 95).
//! * Output is a serializable [`ProviderPressureDecision`] with stable reason
//!   codes — never string parsing in route handlers, never the terminal
//!   auto-queue `skipped` status.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use serde::Serialize;
use serde_json::Value;

/// Default GATE danger threshold (utilization %) used when no runtime override
/// is configured. The dispatch gate defers ONLY when a provider is fully
/// rate-limited (utilization at/above 100), so this is intentionally distinct
/// from the dashboard `rateLimitDangerPct` default (95): the gate has its own
/// `dispatch_rate_limit_gate_danger_pct` knob and never reuses
/// `rate_limit_danger_pct`, leaving other consumers of that field unaffected.
pub const DEFAULT_DANGER_PCT: u64 = 100;

/// Default staleness window (seconds). A cache row older than this is treated as
/// stale and degrades to `UnknownAllow`. Mirrors the dashboard
/// `rateLimitStaleSec` default (600).
pub const DEFAULT_STALE_SEC: i64 = 600;

/// Stable reason codes for a gate decision. Serialized as snake_case strings in
/// the additive `/dispatch-next` and `/api/health/detail` payloads. Additive
/// only: never remove or retype an existing variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureReasonCode {
    /// Gate is disabled by runtime config; always allow.
    GateDisabled,
    /// Fresh telemetry shows utilization below the danger threshold.
    BelowDanger,
    /// Fresh telemetry shows utilization at or above the danger threshold.
    DangerThreshold,
    /// No cache row for the provider; allow (fail-open).
    NoTelemetry,
    /// Cache row is older than the stale window; allow (fail-open).
    StaleTelemetry,
    /// Cache row is malformed / unparseable; allow (fail-open).
    MalformedTelemetry,
    /// Provider has no rate-limit telemetry source (OpenCode/Qwen); allow.
    UnsupportedProvider,
    /// Provider utilization is structurally unknown (e.g. Gemini); allow.
    UnknownUtilization,
    /// Could not resolve a provider for the agent; allow (fail-open).
    ProviderUnresolved,
}

/// Coarse classification of a decision. `allow` -> dispatch may proceed;
/// `defer` -> dispatch is held (entry stays pending).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureVerdict {
    Allow,
    Defer,
}

impl PressureVerdict {
    pub fn is_defer(self) -> bool {
        matches!(self, PressureVerdict::Defer)
    }
}

/// Serializable decision returned by the evaluator. Stable contract consumed by
/// the auto-queue activation loop and the additive API payloads.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderPressureDecision {
    pub verdict: PressureVerdict,
    pub reason_code: PressureReasonCode,
    /// Resolved provider, when known (None when unresolved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Observed utilization percent that drove a `defer`, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization_pct: Option<u64>,
    /// Danger threshold that was applied.
    pub danger_pct: u64,
    /// Age of the telemetry row in seconds at evaluation time, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_age_sec: Option<i64>,
    /// Best-effort reset timestamp hint (unix seconds) for the busy bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_reset_at: Option<i64>,
}

impl ProviderPressureDecision {
    fn allow(reason_code: PressureReasonCode, provider: Option<String>, danger_pct: u64) -> Self {
        Self {
            verdict: PressureVerdict::Allow,
            reason_code,
            provider,
            utilization_pct: None,
            danger_pct,
            cache_age_sec: None,
            estimated_reset_at: None,
        }
    }
}

/// Snapshot of one provider's pressure, derived from a `rate_limit_cache` row.
/// Stored in-memory so the dispatch path never touches Postgres.
#[derive(Debug, Clone)]
pub struct PressureBucketSnapshot {
    pub utilization_pct: u64,
    pub reset_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ProviderPressureSnapshot {
    /// Highest utilization percent across the provider's buckets, when known.
    pub max_utilization_pct: Option<u64>,
    /// Latest reset timestamp (unix seconds) among the bucket(s) with the
    /// highest utilization, when known.
    pub busy_reset_at: Option<i64>,
    /// Per-bucket utilization/reset facts used by the evaluator. Keeping this
    /// lets a runtime danger-threshold change evaluate every dangerous bucket
    /// rather than only the single max-utilization bucket cached at sync time.
    pub pressure_buckets: Vec<PressureBucketSnapshot>,
    /// Unix-seconds timestamp the underlying cache row was fetched.
    pub fetched_at: i64,
    /// Provider has no telemetry source (OpenCode/Qwen) — always allow.
    pub unsupported: bool,
    /// Row existed but no parseable utilization could be derived (e.g. Gemini
    /// quota-only buckets) — degrade to UnknownAllow.
    pub malformed: bool,
}

/// Process-wide provider -> pressure snapshot, refreshed off the hot path by
/// `rate_limit_sync_loop`. Keys are lowercased provider names.
static PROVIDER_PRESSURE: OnceLock<RwLock<HashMap<String, ProviderPressureSnapshot>>> =
    OnceLock::new();

/// Process-wide agent_id -> provider snapshot, refreshed off the hot path by
/// `rate_limit_sync_loop`. Keys are agent ids; values are lowercased provider
/// names resolved from agent/channel bindings.
static AGENT_PROVIDER: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

fn pressure_map() -> &'static RwLock<HashMap<String, ProviderPressureSnapshot>> {
    PROVIDER_PRESSURE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn agent_provider_map() -> &'static RwLock<HashMap<String, String>> {
    AGENT_PROVIDER.get_or_init(|| RwLock::new(HashMap::new()))
}

// ── In-memory diagnostics counters (no DB persistence in P0) ────────────────

static GATE_EVALUATIONS: AtomicU64 = AtomicU64::new(0);
static GATE_DEFERRALS: AtomicU64 = AtomicU64::new(0);
static GATE_BYPASSES: AtomicU64 = AtomicU64::new(0);
/// Unix-seconds timestamp of the last `defer` decision (0 == never).
static LAST_DEFER_AT: AtomicU64 = AtomicU64::new(0);
/// Unix-seconds timestamp of the last process-local snapshot refresh (0 ==
/// never). Used by [`refresh_snapshots_if_stale`] to throttle the on-activation
/// refresh that keeps NON-leader serving nodes populated (the leader-only
/// `rate_limit_sync_loop` does not run on followers).
static LAST_SNAPSHOT_REFRESH_AT: AtomicU64 = AtomicU64::new(0);

/// Minimum interval (seconds) between process-local snapshot refreshes triggered
/// off the activation path. The leader's `rate_limit_sync_loop` already refreshes
/// every ~120s; this matches that cadence so a follower's lazy refresh reads the
/// shared DB cache (no provider credentials) at most once per window and never
/// adds per-entry DB cost to the dispatch loop.
pub const SNAPSHOT_REFRESH_MIN_INTERVAL_SEC: i64 = 120;

/// Aggregate, credential-free diagnostics for `/api/health/detail`. Counts are
/// process-lifetime, in-memory only (no DB, no token data).
#[derive(Debug, Clone, Serialize)]
pub struct DispatchGateDiagnostics {
    pub enabled: bool,
    pub danger_pct: u64,
    pub evaluations: u64,
    pub deferrals: u64,
    pub bypasses: u64,
    /// Number of providers currently held in the in-memory pressure snapshot.
    pub providers_tracked: usize,
    /// Unix-seconds of the most recent defer decision, when one has occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_defer_at: Option<i64>,
}

/// Replace the in-memory provider-pressure snapshot. Called off the hot path by
/// the rate-limit sync loop after it refreshes the Postgres cache.
pub fn set_provider_pressure_snapshot(snapshot: HashMap<String, ProviderPressureSnapshot>) {
    let lock = pressure_map();
    *lock.write().unwrap_or_else(|p| p.into_inner()) = snapshot;
}

/// Replace the in-memory agent_id -> provider snapshot. Called off the hot path
/// by the rate-limit sync loop.
pub fn set_agent_provider_snapshot(snapshot: HashMap<String, String>) {
    let lock = agent_provider_map();
    *lock.write().unwrap_or_else(|p| p.into_inner()) = snapshot;
}

/// Clear the in-memory agent_id -> provider snapshot. Called when an
/// agent->provider refresh fails after a previous successful refresh so the gate
/// fails OPEN (unresolved provider -> allow) instead of gating on a stale
/// mapping. (P2 review fix — server/mod.rs:1047.)
pub fn clear_agent_provider_snapshot() {
    let lock = agent_provider_map();
    lock.write().unwrap_or_else(|p| p.into_inner()).clear();
}

/// Rebuild the in-memory pressure + agent->provider snapshots from the shared
/// `rate_limit_cache` DB rows and the current agent/channel bindings.
///
/// This reads ONLY the shared DB cache (no provider credentials, no live API
/// calls), so it is safe to run on EVERY serving node — not just the leader.
/// The leader's `rate_limit_sync_loop` calls it after refreshing the cache; the
/// activation path calls it (throttled, via [`refresh_snapshots_if_stale`]) so
/// non-leader nodes — where `RateLimitSync` never runs — still have populated
/// snapshots and the gate is not silently a no-op there. (P2 review fix —
/// server/mod.rs:1018.)
///
/// Fails open on every error: a payload/binding load failure leaves the gate
/// allowing dispatch rather than deferring on stale state. In particular, when
/// the agent->provider refresh fails after a previous success, the agent
/// snapshot is CLEARED so an agent moved off a pressured provider is not held by
/// a stale mapping. (P2 review fix — server/mod.rs:1047.)
pub async fn refresh_snapshots_from_db(pg_pool: &sqlx::PgPool, now: i64) {
    let payloads =
        crate::services::analytics::build_rate_limit_provider_payloads_pg(pg_pool, now).await;
    let pressure = pressure_snapshot_from_payloads(&payloads);
    set_provider_pressure_snapshot(pressure);

    refresh_agent_provider_snapshot_from_db(pg_pool).await;
}

/// Rebuild only the agent_id -> provider snapshot from current bindings.
///
/// The binding map changes independently from the shared rate-limit cache, so
/// activation refreshes it even when the provider-pressure cache is still inside
/// its throttle window. This keeps agent rebinds from being held by a stale
/// provider mapping for up to the rate-limit polling interval.
pub async fn refresh_agent_provider_snapshot_from_db(pg_pool: &sqlx::PgPool) {
    match crate::db::agents::load_all_agent_channel_bindings_pg(pg_pool).await {
        Ok(bindings) => {
            let mut agent_provider = HashMap::new();
            for (agent_id, binding) in bindings {
                if let Some(provider) = binding.resolved_primary_provider_kind() {
                    agent_provider.insert(agent_id, provider.as_str().to_string());
                }
            }
            set_agent_provider_snapshot(agent_provider);
        }
        Err(error) => {
            // Fail open: drop the (now possibly stale) agent->provider mapping so
            // a moved agent is treated as unresolved (allow) instead of gated on
            // an outdated provider alongside a freshly-replaced pressure snapshot.
            clear_agent_provider_snapshot();
            tracing::warn!(
                "[dispatch-gate] failed to refresh agent->provider snapshot; cleared stale mapping to fail open: {error}"
            );
        }
    }
}

/// Refresh the process-local snapshots from the shared DB cache iff the last
/// refresh was more than [`SNAPSHOT_REFRESH_MIN_INTERVAL_SEC`] ago (or has never
/// run). Returns `true` when a refresh was performed.
///
/// Called from the auto-queue activation path on whichever node serves
/// `POST /api/queue/dispatch-next`. On the leader this is a cheap no-op (the sync
/// loop already refreshed within the window); on a follower it populates the
/// snapshots the gate reads. The throttle keeps the dispatch loop DB-free in the
/// common case. `now` is unix seconds.
pub async fn refresh_snapshots_if_stale(pg_pool: &sqlx::PgPool, now: i64) -> bool {
    let last = LAST_SNAPSHOT_REFRESH_AT.load(Ordering::Relaxed) as i64;
    if last != 0 && now.saturating_sub(last) < SNAPSHOT_REFRESH_MIN_INTERVAL_SEC {
        refresh_agent_provider_snapshot_from_db(pg_pool).await;
        return false;
    }
    refresh_snapshots_from_db(pg_pool, now).await;
    // Publish freshness only after the snapshots have been loaded. Publishing
    // before the await lets a concurrent activation skip refresh and evaluate
    // against an old/empty map while this task is still reading the DB.
    mark_snapshots_refreshed(now);
    true
}

/// Mark the process-local snapshots as freshly refreshed as of `now` (unix
/// seconds) without doing any DB work. The leader's `rate_limit_sync_loop` calls
/// this after it refreshes the snapshots so the activation-path throttle in
/// [`refresh_snapshots_if_stale`] does not redundantly re-refresh on the leader.
pub fn mark_snapshots_refreshed(now: i64) {
    if now > 0 {
        LAST_SNAPSHOT_REFRESH_AT.store(now as u64, Ordering::Relaxed);
    }
}

/// Build a [`ProviderPressureSnapshot`] from a single provider's cache-payload
/// JSON `Value` (the same `{provider, buckets, fetched_at, stale, unsupported}`
/// shape produced by `build_rate_limit_provider_payloads_pg`). Pure helper so it
/// is unit-testable without a database.
pub fn snapshot_from_provider_payload(
    payload: &Value,
) -> Option<(String, ProviderPressureSnapshot)> {
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)?
        .to_lowercase();
    let fetched_at = payload
        .get("fetched_at")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let unsupported = payload
        .get("unsupported")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if unsupported {
        return Some((
            provider,
            ProviderPressureSnapshot {
                max_utilization_pct: None,
                busy_reset_at: None,
                pressure_buckets: Vec::new(),
                fetched_at,
                unsupported: true,
                malformed: false,
            },
        ));
    }

    let buckets = payload.get("buckets").and_then(Value::as_array);
    let Some(buckets) = buckets else {
        return Some((
            provider,
            ProviderPressureSnapshot {
                max_utilization_pct: None,
                busy_reset_at: None,
                pressure_buckets: Vec::new(),
                fetched_at,
                unsupported: false,
                malformed: true,
            },
        ));
    };

    // Derive utilization% from each bucket. Buckets are
    // `{name, limit, used, remaining, reset}`. Claude/Codex encode utilization
    // as `used` with `limit == 100`. Gemini encodes quota-only buckets with
    // `used == 0` and a non-100 `limit`, which is NOT a real utilization signal,
    // so those are treated as unknown (malformed) and fail open.
    let mut max_util: Option<u64> = None;
    let mut busy_reset: Option<i64> = None;
    let mut pressure_buckets = Vec::new();
    let mut had_util_bucket = false;
    for bucket in buckets {
        let limit = bucket.get("limit").and_then(Value::as_i64);
        let used = bucket.get("used").and_then(Value::as_i64);
        let remaining = bucket.get("remaining").and_then(Value::as_i64);
        let reset = bucket
            .get("reset")
            .and_then(Value::as_i64)
            .filter(|reset| *reset > 0);
        let util = match (limit, used) {
            // Prefer precise utilization when cache writers provide it. Floor
            // fractional values so 99.5% is still below the default 100% gate;
            // exhaustion is represented by 100% or remaining==0.
            _ if bucket.get("utilization").and_then(Value::as_f64).is_some() => bucket
                .get("utilization")
                .and_then(Value::as_f64)
                .map(|value| value.floor().clamp(0.0, 100.0) as u64),
            // Percent-encoded bucket (Claude/Codex): limit == 100, used is %.
            (Some(100), Some(used)) => {
                if used >= 100 && remaining.is_some_and(|value| value > 0) {
                    Some(99)
                } else {
                    Some(used.clamp(0, 100) as u64)
                }
            }
            // Generic ratio bucket: derive percent from used/limit.
            (Some(limit), Some(used)) if limit > 0 && used > 0 => {
                let percent = ((used as f64 / limit as f64) * 100.0)
                    .floor()
                    .clamp(0.0, 100.0) as u64;
                Some(if remaining.is_some_and(|value| value == 0) {
                    100
                } else {
                    percent
                })
            }
            _ => None,
        };
        if let Some(util) = util {
            had_util_bucket = true;
            pressure_buckets.push(PressureBucketSnapshot {
                utilization_pct: util,
                reset_at: reset,
            });
            if max_util.is_none_or(|current| util > current) {
                max_util = Some(util);
                busy_reset = reset;
            } else if max_util == Some(util) {
                busy_reset = match (busy_reset, reset) {
                    (Some(current), Some(candidate)) => Some(current.max(candidate)),
                    (None, Some(candidate)) => Some(candidate),
                    (current, None) => current,
                };
            }
        }
    }

    Some((
        provider,
        ProviderPressureSnapshot {
            max_utilization_pct: max_util,
            busy_reset_at: busy_reset,
            pressure_buckets,
            fetched_at,
            unsupported: false,
            // No bucket carried a usable utilization signal -> unknown.
            malformed: !had_util_bucket,
        },
    ))
}

/// Convert a list of provider payloads (from
/// `build_rate_limit_provider_payloads_pg`) into the in-memory pressure map.
pub fn pressure_snapshot_from_payloads(
    payloads: &[Value],
) -> HashMap<String, ProviderPressureSnapshot> {
    let mut map = HashMap::new();
    for payload in payloads {
        if let Some((provider, snapshot)) = snapshot_from_provider_payload(payload) {
            map.insert(provider, snapshot);
        }
    }
    map
}

/// Read the gate-enabled flag from the YAML live config snapshot. Defaults to
/// `true` (gate ON) when the live config snapshot is unavailable or the field is
/// unset (`None`). Reading the in-memory `config_live_reload` snapshot is
/// lock-cheap and does NOT touch the database, so this is safe on the hot path.
///
/// NOTE: this reads ONLY the YAML live snapshot — it does NOT see a toggle
/// persisted via `PUT /api/settings/runtime-config` (which lands in `kv_meta`,
/// never the YAML snapshot). The activation path resolves the persisted value
/// first and passes it via [`evaluate_agent_provider_pressure_with_overrides`];
/// this function is the YAML fallback used when no persisted override exists and
/// by the diagnostics block.
pub fn gate_enabled() -> bool {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.dispatch_rate_limit_gate_enabled)
        .unwrap_or(true)
}

/// Read the gate-specific danger threshold (utilization %) from the YAML live
/// config snapshot. Falls back to [`DEFAULT_DANGER_PCT`] (100) when unset. Uses
/// the dedicated `dispatch_rate_limit_gate_danger_pct` knob — NOT
/// `rate_limit_danger_pct` — so the dashboard's 95% danger coloring and any
/// other `rate_limit_danger_pct` consumer are unaffected by the gate threshold.
///
/// Like [`gate_enabled`], this reads ONLY the YAML snapshot; the persisted
/// runtime-config override is resolved on the activation path and passed
/// explicitly via [`evaluate_agent_provider_pressure_with_overrides`].
pub fn danger_pct() -> u64 {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.dispatch_rate_limit_gate_danger_pct)
        .map(u64::from)
        .unwrap_or(DEFAULT_DANGER_PCT)
}

/// Read the live staleness window (seconds). Falls back to
/// [`DEFAULT_STALE_SEC`] when unset.
pub fn stale_sec() -> i64 {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.rate_limit_stale_sec)
        .map(|value| value as i64)
        .unwrap_or(DEFAULT_STALE_SEC)
}

/// Pure evaluator: given a (possibly absent) provider pressure snapshot and the
/// effective thresholds, decide whether to allow or defer. No I/O, no globals —
/// fully unit-testable. `now` is unix seconds.
pub fn evaluate_provider_pressure(
    provider: &str,
    snapshot: Option<&ProviderPressureSnapshot>,
    danger_pct: u64,
    stale_sec: i64,
    now: i64,
) -> ProviderPressureDecision {
    let provider_owned = Some(provider.to_string());
    let Some(snapshot) = snapshot else {
        return ProviderPressureDecision::allow(
            PressureReasonCode::NoTelemetry,
            provider_owned,
            danger_pct,
        );
    };

    if snapshot.unsupported {
        return ProviderPressureDecision::allow(
            PressureReasonCode::UnsupportedProvider,
            provider_owned,
            danger_pct,
        );
    }

    let cache_age = now.saturating_sub(snapshot.fetched_at);
    if cache_age > stale_sec {
        let mut decision = ProviderPressureDecision::allow(
            PressureReasonCode::StaleTelemetry,
            provider_owned,
            danger_pct,
        );
        decision.cache_age_sec = Some(cache_age);
        return decision;
    }

    if snapshot.malformed {
        return ProviderPressureDecision::allow(
            PressureReasonCode::MalformedTelemetry,
            provider_owned,
            danger_pct,
        );
    }

    let Some(util) = snapshot.max_utilization_pct else {
        return ProviderPressureDecision::allow(
            PressureReasonCode::UnknownUtilization,
            provider_owned,
            danger_pct,
        );
    };

    let fallback_bucket;
    let pressure_buckets = if snapshot.pressure_buckets.is_empty() {
        fallback_bucket = [PressureBucketSnapshot {
            utilization_pct: util,
            reset_at: snapshot.busy_reset_at,
        }];
        fallback_bucket.as_slice()
    } else {
        snapshot.pressure_buckets.as_slice()
    };

    let dangerous_buckets = pressure_buckets
        .iter()
        .filter(|bucket| bucket.utilization_pct >= danger_pct)
        .collect::<Vec<_>>();

    if !dangerous_buckets.is_empty() {
        let dangerous_util = dangerous_buckets
            .iter()
            .map(|bucket| bucket.utilization_pct)
            .max()
            .unwrap_or(util);
        let latest_reset = dangerous_buckets
            .iter()
            .filter_map(|bucket| bucket.reset_at)
            .max();
        let latest_future_reset = dangerous_buckets
            .iter()
            .filter_map(|bucket| bucket.reset_at.filter(|reset_at| *reset_at > now))
            .max();

        // Every dangerous bucket has an expired reset timestamp: the pressure
        // windows the cached utilization describes have expired. If any
        // dangerous bucket has no reset, keep deferring because expiration cannot
        // be proven.
        if dangerous_buckets
            .iter()
            .all(|bucket| bucket.reset_at.is_some_and(|reset_at| reset_at <= now))
        {
            let mut decision = ProviderPressureDecision::allow(
                PressureReasonCode::StaleTelemetry,
                provider_owned,
                danger_pct,
            );
            decision.utilization_pct = Some(dangerous_util);
            decision.cache_age_sec = Some(cache_age);
            decision.estimated_reset_at = latest_reset;
            return decision;
        }
        ProviderPressureDecision {
            verdict: PressureVerdict::Defer,
            reason_code: PressureReasonCode::DangerThreshold,
            provider: provider_owned,
            utilization_pct: Some(dangerous_util),
            danger_pct,
            cache_age_sec: Some(cache_age),
            estimated_reset_at: latest_future_reset.or(latest_reset),
        }
    } else {
        let mut decision = ProviderPressureDecision::allow(
            PressureReasonCode::BelowDanger,
            provider_owned,
            danger_pct,
        );
        decision.utilization_pct = Some(util);
        decision.cache_age_sec = Some(cache_age);
        decision
    }
}

/// Resolve the provider for `agent_id` from the in-memory snapshot. Returns
/// `None` when the snapshot has not been populated yet or the agent is unknown
/// (caller fails open). No DB query.
pub fn resolve_agent_provider(agent_id: &str) -> Option<String> {
    let lock = agent_provider_map();
    lock.read()
        .unwrap_or_else(|p| p.into_inner())
        .get(agent_id)
        .cloned()
}

/// Hot-path entry point used by auto-queue activation. Performs an O(1),
/// lock-cheap, DB-free evaluation for the given agent and records diagnostics.
///
/// Resolves the enable flag and gate danger threshold from the YAML live config
/// snapshot only. Prefer [`evaluate_agent_provider_pressure_with_overrides`] on
/// the activation path so a toggle persisted via `PUT
/// /api/settings/runtime-config` (which lands in `kv_meta`, not the YAML
/// snapshot) is honored at runtime.
///
/// Fails open on every ambiguity: gate disabled, unresolved provider, missing /
/// stale / malformed / unsupported / unknown telemetry all return `Allow`.
///
/// The activation path now calls
/// [`evaluate_agent_provider_pressure_with_overrides`] directly (to honor the
/// persisted runtime-config). This YAML-only convenience entry point is retained
/// as the stable public API used by the unit tests and any future caller that
/// has no persisted overrides to pass.
#[allow(dead_code)]
pub fn evaluate_agent_provider_pressure(agent_id: &str, now: i64) -> ProviderPressureDecision {
    evaluate_agent_provider_pressure_with_overrides(agent_id, now, None, None, None)
}

/// Hot-path entry point with explicit, caller-resolved overrides for the
/// gate-enabled flag and the gate danger threshold.
///
/// The activation path passes the values it read from the **persisted
/// runtime-config** (`kv_meta`) so the dashboard/API rollback switch (and a
/// persisted danger-threshold change) actually take effect at runtime — the
/// persisted value never reaches `config_live_reload::current()`, so reading the
/// YAML snapshot alone would silently ignore an API toggle. Each override falls
/// back to the YAML snapshot value (then the compiled default) when `None`.
pub fn evaluate_agent_provider_pressure_with_overrides(
    agent_id: &str,
    now: i64,
    enabled_override: Option<bool>,
    danger_override: Option<u64>,
    stale_override: Option<i64>,
) -> ProviderPressureDecision {
    GATE_EVALUATIONS.fetch_add(1, Ordering::Relaxed);

    let danger = danger_override.unwrap_or_else(danger_pct);
    let enabled = enabled_override.unwrap_or_else(gate_enabled);

    if !enabled {
        return ProviderPressureDecision::allow(
            PressureReasonCode::GateDisabled,
            resolve_agent_provider(agent_id),
            danger,
        );
    }

    let Some(provider) = resolve_agent_provider(agent_id) else {
        return ProviderPressureDecision::allow(
            PressureReasonCode::ProviderUnresolved,
            None,
            danger,
        );
    };

    let decision = {
        let lock = pressure_map();
        let map = lock.read().unwrap_or_else(|p| p.into_inner());
        evaluate_provider_pressure(
            &provider,
            map.get(&provider),
            danger,
            stale_override.unwrap_or_else(stale_sec),
            now,
        )
    };

    if decision.verdict.is_defer() {
        GATE_DEFERRALS.fetch_add(1, Ordering::Relaxed);
        if now > 0 {
            LAST_DEFER_AT.store(now as u64, Ordering::Relaxed);
        }
    }

    decision
}

/// Resolve the gate-enabled flag and gate danger threshold from the persisted
/// runtime-config (`kv_meta` `runtime-config` JSON, written by
/// `PUT /api/settings/runtime-config`). Returns `(enabled_override,
/// danger_override)` where each is `Some` only when the operator has persisted an
/// explicit value; `None` lets the caller fall back to the YAML live snapshot
/// and then the compiled defaults.
///
/// This is the bridge that makes the runtime rollback switch work: the persisted
/// toggle never reaches `config_live_reload::current()`, so the gate must read it
/// from `kv_meta` (mirroring `effective_max_entry_retries`). Runs off the
/// activation path's existing pool read; on any parse/DB error it returns
/// `(None, None)` (fail back to YAML/defaults, never blocks).
pub fn persisted_runtime_overrides(
    runtime_config: Option<&Value>,
) -> (Option<bool>, Option<u64>, Option<i64>) {
    let Some(value) = runtime_config else {
        return (None, None, None);
    };
    let enabled = value
        .get("dispatchRateLimitGateEnabled")
        .and_then(Value::as_bool)
        .filter(|enabled| *enabled != true);
    let danger = value
        .get("dispatchRateLimitGateDangerPct")
        .and_then(Value::as_u64)
        .filter(|danger| *danger != DEFAULT_DANGER_PCT);
    let stale = value
        .get("rateLimitStaleSec")
        .and_then(Value::as_i64)
        .filter(|stale| *stale != DEFAULT_STALE_SEC);
    (enabled, danger, stale)
}

/// Record that a manual/force dispatch path intentionally bypassed the gate
/// (REQ-005). In-memory counter only; never blocks the bypass.
///
/// In P0 the gate is wired ONLY into auto-queue activation, so manual and
/// direct dispatch paths bypass it implicitly by never calling the evaluator —
/// their behavior is unchanged (REQ-005). This function is the stable
/// accounting surface a later opt-in hard-block PR will call from those paths to
/// annotate an explicit bypass; it is exposed and tested now so the diagnostics
/// contract is stable. Intentionally unwired in P0 (mirrors the codebase's
/// existing intentional-but-unwired infra convention).
#[allow(dead_code)]
pub fn record_gate_bypass() {
    GATE_BYPASSES.fetch_add(1, Ordering::Relaxed);
}

/// Build the credential-free diagnostics block for `/api/health/detail`.
///
/// `enabled_override` / `danger_override` let the caller report the EFFECTIVE
/// values the gate actually uses on the activation path — i.e. the persisted
/// runtime-config (`kv_meta`) values, which never reach the YAML live snapshot.
/// Pass `None` for each to fall back to the YAML snapshot then the compiled
/// defaults (the same precedence the gate uses).
pub fn diagnostics_with_overrides(
    enabled_override: Option<bool>,
    danger_override: Option<u64>,
) -> DispatchGateDiagnostics {
    let providers_tracked = pressure_map()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    let last_defer_at = match LAST_DEFER_AT.load(Ordering::Relaxed) {
        0 => None,
        value => Some(value as i64),
    };
    DispatchGateDiagnostics {
        enabled: enabled_override.unwrap_or_else(gate_enabled),
        danger_pct: danger_override.unwrap_or_else(danger_pct),
        evaluations: GATE_EVALUATIONS.load(Ordering::Relaxed),
        deferrals: GATE_DEFERRALS.load(Ordering::Relaxed),
        bypasses: GATE_BYPASSES.load(Ordering::Relaxed),
        providers_tracked,
        last_defer_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(
        max_util: Option<u64>,
        reset: Option<i64>,
        fetched_at: i64,
        unsupported: bool,
        malformed: bool,
    ) -> ProviderPressureSnapshot {
        ProviderPressureSnapshot {
            max_utilization_pct: max_util,
            busy_reset_at: reset,
            pressure_buckets: max_util
                .map(|utilization_pct| {
                    vec![PressureBucketSnapshot {
                        utilization_pct,
                        reset_at: reset,
                    }]
                })
                .unwrap_or_default(),
            fetched_at,
            unsupported,
            malformed,
        }
    }

    // TEST-001: pure evaluator reason codes ---------------------------------

    #[test]
    fn missing_row_allows_no_telemetry() {
        let decision = evaluate_provider_pressure("claude", None, 95, 600, 1_000);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::NoTelemetry);
    }

    #[test]
    fn fresh_below_danger_allows() {
        let s = snap(Some(40), Some(2_000), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::BelowDanger);
        assert_eq!(decision.utilization_pct, Some(40));
    }

    #[test]
    fn fresh_danger_defers_with_reset_hint() {
        let s = snap(Some(97), Some(5_555), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
        assert_eq!(decision.reason_code, PressureReasonCode::DangerThreshold);
        assert_eq!(decision.utilization_pct, Some(97));
        assert_eq!(decision.estimated_reset_at, Some(5_555));
    }

    #[test]
    fn at_threshold_defers() {
        let s = snap(Some(95), None, 1_000, false, false);
        let decision = evaluate_provider_pressure("codex", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
    }

    #[test]
    fn stale_row_allows_even_if_over_danger() {
        // fetched far in the past -> stale -> fail open.
        let s = snap(Some(99), Some(2_000), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 5_000);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::StaleTelemetry);
        assert_eq!(decision.cache_age_sec, Some(4_000));
    }

    #[test]
    fn unsupported_provider_allows() {
        let s = snap(None, None, 1_000, true, false);
        let decision = evaluate_provider_pressure("opencode", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(
            decision.reason_code,
            PressureReasonCode::UnsupportedProvider
        );
    }

    #[test]
    fn malformed_row_allows() {
        let s = snap(None, None, 1_000, false, true);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::MalformedTelemetry);
    }

    #[test]
    fn unknown_utilization_allows() {
        // Row is fresh & well-formed but carries no utilization (Gemini-like).
        let s = snap(None, None, 1_000, false, false);
        let decision = evaluate_provider_pressure("gemini", Some(&s), 95, 600, 1_050);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::UnknownUtilization);
    }

    // TEST-001: payload -> snapshot derivation ------------------------------

    #[test]
    fn snapshot_from_claude_percent_buckets() {
        let payload = json!({
            "provider": "claude",
            "fetched_at": 1_000,
            "unsupported": false,
            "buckets": [
                {"name": "5h", "limit": 100, "used": 30, "remaining": 70, "reset": 2_000},
                {"name": "7d", "limit": 100, "used": 97, "remaining": 3, "reset": 9_000},
            ],
        });
        let (provider, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(provider, "claude");
        assert_eq!(snapshot.max_utilization_pct, Some(97));
        assert_eq!(snapshot.busy_reset_at, Some(9_000));
        assert_eq!(snapshot.pressure_buckets.len(), 2);
        assert!(!snapshot.malformed);
    }

    #[test]
    fn snapshot_keeps_latest_reset_for_tied_max_buckets() {
        let payload = json!({
            "provider": "claude",
            "fetched_at": 1_000,
            "unsupported": false,
            "buckets": [
                {"name": "5h", "limit": 100, "used": 100, "remaining": 0, "reset": 2_000},
                {"name": "7d", "limit": 100, "used": 100, "remaining": 0, "reset": 9_000},
            ],
        });
        let (_, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(snapshot.max_utilization_pct, Some(100));
        assert_eq!(snapshot.busy_reset_at, Some(9_000));
    }

    #[test]
    fn snapshot_floors_fractional_utilization_below_full() {
        let payload = json!({
            "provider": "claude",
            "fetched_at": 1_000,
            "unsupported": false,
            "buckets": [
                {"name": "5h", "limit": 100, "used": 100, "remaining": 0, "utilization": 99.5, "reset": 2_000},
            ],
        });
        let (_, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(snapshot.max_utilization_pct, Some(99));
    }

    #[test]
    fn snapshot_from_gemini_quota_buckets_is_unknown() {
        // Gemini buckets are quota-only (used == 0, limit != 100) -> no usable
        // utilization signal -> malformed/unknown -> fail open.
        let payload = json!({
            "provider": "gemini",
            "fetched_at": 1_000,
            "unsupported": false,
            "buckets": [
                {"name": "rpm", "limit": 60, "used": 0, "remaining": 60, "reset": 0},
                {"name": "rpd", "limit": 1000, "used": 0, "remaining": 1000, "reset": 0},
            ],
        });
        let (provider, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(provider, "gemini");
        assert_eq!(snapshot.max_utilization_pct, None);
        assert!(snapshot.malformed);
    }

    #[test]
    fn snapshot_from_unsupported_provider() {
        let payload = json!({
            "provider": "opencode",
            "fetched_at": 1_000,
            "unsupported": true,
            "buckets": [],
        });
        let (provider, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert_eq!(provider, "opencode");
        assert!(snapshot.unsupported);
    }

    #[test]
    fn snapshot_missing_buckets_is_malformed() {
        let payload = json!({"provider": "claude", "fetched_at": 1_000, "unsupported": false});
        let (_, snapshot) = snapshot_from_provider_payload(&payload).unwrap();
        assert!(snapshot.malformed);
    }

    #[test]
    fn decision_serializes_with_stable_field_names() {
        let s = snap(Some(97), Some(5_555), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        let value = serde_json::to_value(&decision).unwrap();
        assert_eq!(value["verdict"], json!("defer"));
        assert_eq!(value["reason_code"], json!("danger_threshold"));
        assert_eq!(value["provider"], json!("claude"));
        assert_eq!(value["utilization_pct"], json!(97));
        assert_eq!(value["danger_pct"], json!(95));
        assert_eq!(value["estimated_reset_at"], json!(5_555));
    }

    // ── Hot-path + rollback/disable tests (REQ-002, REQ-003, REQ-005,
    //    REQ-006) using the process-global config + snapshots ──────────────

    /// Serializes tests that mutate the process-global `config_live_reload`
    /// snapshot and the dispatch-gate snapshots so they do not race the parallel
    /// test runner.
    fn global_gate_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Installs a live config snapshot for the gate tests. `danger` drives the
    /// GATE-specific threshold (`dispatch_rate_limit_gate_danger_pct`) — NOT the
    /// dashboard `rate_limit_danger_pct` — since that is what the gate reads.
    fn install_runtime(enabled: Option<bool>, danger: Option<u64>, stale: Option<u64>) {
        let mut config = crate::config::Config::default();
        config.runtime.dispatch_rate_limit_gate_enabled = enabled;
        config.runtime.dispatch_rate_limit_gate_danger_pct =
            danger.map(|value| value.min(u8::MAX as u64) as u8);
        config.runtime.rate_limit_stale_sec = stale;
        crate::config_live_reload::install(config);
    }

    #[test]
    fn gate_defers_when_provider_in_danger() {
        let _guard = global_gate_test_guard();
        install_runtime(Some(true), Some(95), Some(600));
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-x".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), Some(20_000), now - 10, false, false),
        )]));

        let decision = evaluate_agent_provider_pressure("agent-x", now);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
        assert_eq!(decision.reason_code, PressureReasonCode::DangerThreshold);
        assert_eq!(decision.provider.as_deref(), Some("claude"));
    }

    #[test]
    fn rollback_disabling_gate_resumes_dispatch_for_blocked_entry() {
        // REQ-003 / TSK-P0-007: a provider in danger that would defer with the
        // gate enabled must ALLOW the moment the gate is disabled — proving the
        // disable flag cleanly resumes dispatch without any partial-block state.
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-y".to_string(),
            "codex".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "codex".to_string(),
            snap(Some(99), Some(20_000), now - 10, false, false),
        )]));

        // Enabled -> defer.
        install_runtime(Some(true), Some(95), Some(600));
        assert_eq!(
            evaluate_agent_provider_pressure("agent-y", now).verdict,
            PressureVerdict::Defer
        );

        // Disabled (rollback) -> allow, with the gate_disabled reason code.
        install_runtime(Some(false), Some(95), Some(600));
        let resumed = evaluate_agent_provider_pressure("agent-y", now);
        assert_eq!(resumed.verdict, PressureVerdict::Allow);
        assert_eq!(resumed.reason_code, PressureReasonCode::GateDisabled);
    }

    #[test]
    fn lowering_danger_threshold_below_utilization_resumes_dispatch() {
        // REQ-003 / TSK-P0-007 alternate disable path: same blocked entry
        // resumes when the danger threshold is raised above the observed
        // utilization (here we lower utilization by raising the bar to 100 while
        // util is 99 -> below danger -> allow).
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-z".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), None, now - 10, false, false),
        )]));

        install_runtime(Some(true), Some(95), Some(600));
        assert_eq!(
            evaluate_agent_provider_pressure("agent-z", now).verdict,
            PressureVerdict::Defer
        );

        install_runtime(Some(true), Some(100), Some(600));
        let resumed = evaluate_agent_provider_pressure("agent-z", now);
        assert_eq!(resumed.verdict, PressureVerdict::Allow);
        assert_eq!(resumed.reason_code, PressureReasonCode::BelowDanger);
    }

    #[test]
    fn default_when_gate_flag_unset_is_enabled() {
        // REQ-003 + JOB-1: None gate flag defaults to ON (gate enabled) and the
        // gate danger threshold defaults to 100 (defer only when a provider is
        // fully rate-limited), so a fully-utilized provider still defers by
        // default.
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-d".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(100), None, now - 10, false, false),
        )]));
        install_runtime(None, None, None);
        assert!(gate_enabled());
        assert_eq!(danger_pct(), DEFAULT_DANGER_PCT);
        assert_eq!(DEFAULT_DANGER_PCT, 100);
        assert_eq!(
            evaluate_agent_provider_pressure("agent-d", now).verdict,
            PressureVerdict::Defer
        );
    }

    #[test]
    fn default_gate_threshold_does_not_defer_below_full() {
        // JOB-1: at the default gate threshold (100), a provider at 99% (which
        // the dashboard would color "danger" at 95) must NOT defer — the gate
        // only holds entries when the provider is fully rate-limited.
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-d99".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), None, now - 10, false, false),
        )]));
        install_runtime(Some(true), None, Some(600));
        let decision = evaluate_agent_provider_pressure("agent-d99", now);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::BelowDanger);
        assert_eq!(decision.danger_pct, 100);
    }

    #[test]
    fn unresolved_agent_provider_fails_open() {
        // REQ-006: an agent with no provider binding in the snapshot allows.
        let _guard = global_gate_test_guard();
        install_runtime(Some(true), Some(95), Some(600));
        set_agent_provider_snapshot(HashMap::new());
        let decision = evaluate_agent_provider_pressure("unknown-agent", 10_000);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::ProviderUnresolved);
    }

    #[test]
    fn bypass_counter_increments() {
        // REQ-005: explicit bypass accounting surface increments the counter
        // without affecting verdicts.
        let _guard = global_gate_test_guard();
        let before = GATE_BYPASSES.load(Ordering::Relaxed);
        record_gate_bypass();
        assert_eq!(GATE_BYPASSES.load(Ordering::Relaxed), before + 1);
    }

    // P2 review fix — expired reset window must not keep deferring on stale
    // utilization.
    #[test]
    fn over_danger_but_reset_in_past_allows_as_stale() {
        // Fresh row (within stale window), utilization above danger, but the
        // busiest bucket's reset timestamp is already in the past -> the pressure
        // window has expired -> allow (stale), do not hold the entry pending.
        let s = snap(Some(100), Some(900), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 100, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Allow);
        assert_eq!(decision.reason_code, PressureReasonCode::StaleTelemetry);
        assert_eq!(decision.utilization_pct, Some(100));
        assert_eq!(decision.estimated_reset_at, Some(900));
    }

    #[test]
    fn over_danger_with_reset_in_future_still_defers() {
        // Same as above but the reset is in the future -> the pressure window is
        // still open -> defer.
        let s = snap(Some(100), Some(5_000), 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 100, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
        assert_eq!(decision.reason_code, PressureReasonCode::DangerThreshold);
    }

    #[test]
    fn over_danger_without_reset_still_defers() {
        // No reset timestamp at all -> cannot prove the window expired -> defer.
        let s = snap(Some(100), None, 1_000, false, false);
        let decision = evaluate_provider_pressure("claude", Some(&s), 100, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
    }

    // P1 review fix — persisted runtime overrides honored.
    #[test]
    fn persisted_overrides_parse_enable_and_threshold() {
        let cfg = json!({
            "dispatchRateLimitGateEnabled": false,
            "dispatchRateLimitGateDangerPct": 80,
        });
        let (enabled, danger, stale) = persisted_runtime_overrides(Some(&cfg));
        assert_eq!(enabled, Some(false));
        assert_eq!(danger, Some(80));
        assert_eq!(stale, None);

        // Missing keys -> None (fall back to YAML/defaults).
        let (enabled, danger, stale) = persisted_runtime_overrides(Some(&json!({})));
        assert_eq!(enabled, None);
        assert_eq!(danger, None);
        assert_eq!(stale, None);

        let (enabled, danger, stale) = persisted_runtime_overrides(None);
        assert_eq!(enabled, None);
        assert_eq!(danger, None);
        assert_eq!(stale, None);
    }

    #[test]
    fn persisted_seed_defaults_fall_back_to_yaml_values() {
        let cfg = json!({
            "dispatchRateLimitGateEnabled": true,
            "dispatchRateLimitGateDangerPct": 100,
            "rateLimitStaleSec": 600,
        });
        let (enabled, danger, stale) = persisted_runtime_overrides(Some(&cfg));
        assert_eq!(enabled, None);
        assert_eq!(danger, None);
        assert_eq!(stale, None);
    }

    #[test]
    fn persisted_staleness_override_changes_defer_decision() {
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-stale".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(100), Some(20_000), now - 700, false, false),
        )]));
        install_runtime(Some(true), Some(100), Some(600));

        assert_eq!(
            evaluate_agent_provider_pressure_with_overrides("agent-stale", now, None, None, None)
                .reason_code,
            PressureReasonCode::StaleTelemetry
        );

        let gated = evaluate_agent_provider_pressure_with_overrides(
            "agent-stale",
            now,
            None,
            None,
            Some(900),
        );
        assert_eq!(gated.verdict, PressureVerdict::Defer);
    }

    #[test]
    fn persisted_disable_override_resumes_dispatch_even_when_yaml_enables() {
        // The persisted runtime toggle (kv_meta) MUST win over the YAML live
        // snapshot: YAML says enabled, the persisted override says disabled ->
        // the gate must allow (rollback switch works at runtime).
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-p".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(100), Some(20_000), now - 10, false, false),
        )]));
        install_runtime(Some(true), Some(100), Some(600));

        // No override -> YAML enables -> defer.
        assert_eq!(
            evaluate_agent_provider_pressure_with_overrides("agent-p", now, None, None, None)
                .verdict,
            PressureVerdict::Defer
        );
        // Persisted disable override -> allow (gate_disabled).
        let resumed = evaluate_agent_provider_pressure_with_overrides(
            "agent-p",
            now,
            Some(false),
            None,
            None,
        );
        assert_eq!(resumed.verdict, PressureVerdict::Allow);
        assert_eq!(resumed.reason_code, PressureReasonCode::GateDisabled);
    }

    #[test]
    fn persisted_threshold_override_changes_defer_decision() {
        // The persisted gate danger threshold (kv_meta) MUST win over the YAML
        // snapshot: util 99 with persisted threshold 95 -> defer; with the
        // default 100 (no override) -> allow.
        let _guard = global_gate_test_guard();
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-t".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(99), Some(20_000), now - 10, false, false),
        )]));
        install_runtime(Some(true), None, Some(600));

        // Default threshold (100) -> 99 below danger -> allow.
        assert_eq!(
            evaluate_agent_provider_pressure_with_overrides("agent-t", now, None, None, None)
                .verdict,
            PressureVerdict::Allow
        );
        // Persisted threshold 95 -> 99 at/above danger -> defer.
        let gated =
            evaluate_agent_provider_pressure_with_overrides("agent-t", now, None, Some(95), None);
        assert_eq!(gated.verdict, PressureVerdict::Defer);
        assert_eq!(gated.danger_pct, 95);
    }

    #[test]
    fn future_reset_on_any_dangerous_bucket_keeps_deferring() {
        let s = ProviderPressureSnapshot {
            max_utilization_pct: Some(100),
            busy_reset_at: Some(900),
            pressure_buckets: vec![
                PressureBucketSnapshot {
                    utilization_pct: 100,
                    reset_at: Some(900),
                },
                PressureBucketSnapshot {
                    utilization_pct: 96,
                    reset_at: Some(20_000),
                },
            ],
            fetched_at: 1_000,
            unsupported: false,
            malformed: false,
        };
        let decision = evaluate_provider_pressure("claude", Some(&s), 95, 600, 1_100);
        assert_eq!(decision.verdict, PressureVerdict::Defer);
        assert_eq!(decision.estimated_reset_at, Some(20_000));
    }

    // P2 review fix — fail open when the agent->provider snapshot is cleared.
    #[test]
    fn cleared_agent_provider_snapshot_fails_open() {
        let _guard = global_gate_test_guard();
        install_runtime(Some(true), Some(100), Some(600));
        let now = 10_000;
        set_agent_provider_snapshot(HashMap::from([(
            "agent-c".to_string(),
            "claude".to_string(),
        )]));
        set_provider_pressure_snapshot(HashMap::from([(
            "claude".to_string(),
            snap(Some(100), Some(20_000), now - 10, false, false),
        )]));
        // Populated mapping + full pressure -> defer.
        assert_eq!(
            evaluate_agent_provider_pressure("agent-c", now).verdict,
            PressureVerdict::Defer
        );
        // Simulate a failed agent->provider refresh clearing the stale mapping.
        clear_agent_provider_snapshot();
        let resumed = evaluate_agent_provider_pressure("agent-c", now);
        assert_eq!(resumed.verdict, PressureVerdict::Allow);
        assert_eq!(resumed.reason_code, PressureReasonCode::ProviderUnresolved);
    }
}
