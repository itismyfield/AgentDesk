//! Relay recovery dry-run planner and conservative auto-heal executor.
//!
//! This module is intentionally narrow: it turns the read-only relay health
//! classifier into an operator-facing decision, and only applies local,
//! idempotent cleanup when the evidence is strong enough.
//!
//! Known residual limitations for follow-up issues: committed-but-leaked and
//! stale foreign inflight rows are swept independently of TUI-direct pending-start
//! records, while retaining the same terminal/death-evidence and identity gates.
//! Rows whose `output_path` is missing
//! or points at a deleted file are permanently denied by the destructive cancel
//! gate because no frozen-capture or terminal-envelope evidence can be re-probed.
//! Stage-3 recovery where `watcher_attached=false` still relies on the
//! pending-start backstop trigger. Frozen-busy JSONL rows remain denied until
//! the output file has been quiescent for the conservative stale window and the
//! live pane itself reports ready for input; shorter freezes or busy panes are
//! intentionally residual. Committed rows coupled to a mismatched `rebind_origin`
//! are not independently healed here. The manual stale-mailbox repair route
//! additionally requires `unread_bytes == Some(0)` (parity with
//! ReattachWatcher, via `unread_tail_is_proven_drained`): a dead relay that
//! leaves capture bytes permanently ahead of the relay offset — or a tail that
//! cannot be measured against this row's frontier at all — keeps that manual
//! path blocked even when the pane is ready. Resolving such rows falls to the
//! destructive cancel gate / pending-start demote instead.
//! Do not broaden those paths inside the
//! #4030 watcher-cancel fix; they need separate design/review.

use std::path::Path;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
#[cfg(unix)]
use std::sync::{LazyLock, Mutex as StdMutex, mpsc};
use std::time::{Duration, SystemTime};
#[cfg(unix)]
use std::{collections::BTreeMap, fs, io::Write, path::PathBuf};

use poise::serenity_prelude::ChannelId;
use serde::Serialize;

use super::health::HealthRegistry;
use super::relay_health::{RelayActiveTurn, RelayHealthSnapshot, RelayStallState};
use super::{
    SharedData, clear_watchdog_deadline_override, destructive_cancel_gate, health, inflight,
    mailbox_clear_channel, mailbox_clear_recovery_marker, mailbox_finish_turn, mailbox_snapshot,
    recovery, saturating_decrement_global_active, stall_recovery, turn_finalizer,
};
#[cfg(unix)]
use crate::config::RelayAuthorityMode;
use crate::services::provider::ProviderKind;

#[path = "relay_recovery/apply.rs"]
mod apply;
#[path = "relay_recovery/authority_observation.rs"]
pub(crate) mod authority_observation;
#[path = "relay_recovery_auto_heal_apply.rs"]
mod auto_heal_apply;
#[path = "relay_recovery_auto_heal_attempts.rs"]
mod auto_heal_attempts;
#[path = "relay_recovery_auto_heal_confirm.rs"]
mod auto_heal_confirm;
#[path = "relay_recovery_circuit_breaker.rs"]
mod circuit_breaker;
#[path = "relay_recovery/cohort.rs"]
pub(crate) mod cohort;
#[path = "relay_recovery_completion_footer.rs"]
mod completion_footer;
#[path = "relay_recovery/decision.rs"]
mod decision;
#[path = "relay_recovery/idle_tmux.rs"]
mod idle_tmux;
#[path = "relay_recovery_leaked_row_sweep.rs"]
pub(super) mod leaked_row_sweep;
#[path = "relay_recovery_reattach_apply.rs"]
mod reattach_apply;
#[path = "relay_recovery_circuit_alert_producer.rs"]
mod relay_recovery_circuit_alert_producer;

pub(super) use apply::*;
pub(in crate::services::discord) use decision::*;
pub(crate) use idle_tmux::*;

use auto_heal_apply::apply_relay_recovery_plan;
#[cfg(test)]
use auto_heal_attempts::{
    AUTO_HEAL_DEAD_FRONTIER_REATTACH_MAX_ATTEMPTS_PER_WINDOW,
    AUTO_HEAL_DEFAULT_MAX_ATTEMPTS_PER_WINDOW, auto_heal_test_lock,
    clear_auto_heal_attempts_for_tests, reserve_auto_heal_attempt,
};
use auto_heal_attempts::{
    AUTO_HEAL_WINDOW_SECS, auto_heal_key, max_attempts_per_window_for_snapshot,
    remaining_auto_heal_attempts,
};

const FROZEN_BUSY_JSONL_READY_FALLBACK_AGE: Duration = Duration::from_secs(10 * 60);
/// Protect probe and manual cleanup across the #4569 incident window: mailbox
/// admission at 05:16:44.468 was misclassified at 05:16:47.320 (~2.9 seconds).
/// The 30-second margin plus the 30-second probe cadence reclaims a genuine
/// orphan on the first post-grace tick (normally within 60 seconds), not at the
/// grace boundary itself. Stall-watchdog cleanup is exempt because its caller
/// has already passed the independent death-evidence gate. A wall-clock rollback
/// extends this protection because age uses `saturating_sub` below.
const ORPHAN_PENDING_TOKEN_ADMISSION_GRACE: Duration = Duration::from_secs(30);

#[cfg(unix)]
const AXIS_B_SCHEMA: &str = "relay_authority.axis_b.v1";
#[cfg(unix)]
static AXIS_B_TRIAGE: LazyLock<StdMutex<BTreeMap<String, u64>>> = LazyLock::new(StdMutex::default);
#[cfg(unix)]
static AXIS_B_SINK: OnceLock<mpsc::SyncSender<AxisBWrite>> = OnceLock::new();
#[cfg(unix)]
static AXIS_B_WRITER: LazyLock<StdMutex<()>> = LazyLock::new(StdMutex::default);
#[cfg(unix)]
static AXIS_B_DROPPED_RECORDS: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
static AXIS_B_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
struct AxisBWrite {
    path: PathBuf,
    bytes: Vec<u8>,
}

#[cfg(unix)]
#[derive(Clone, Debug, Default, Serialize)]
pub(in crate::services::discord) struct AxisBObservationReport {
    counters: BTreeMap<String, u64>,
    dropped_records: u64,
    write_failures: u64,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum AxisBSite {
    RelayRecovery,
    WatchdogStaleIdle,
    WatchdogExplicitBackground,
    StaleTurnIntake,
    RelayDeadReattach,
    ProbeAutoHeal,
}

#[cfg(unix)]
impl AxisBSite {
    const ALL: [Self; 6] = [
        Self::RelayRecovery,
        Self::WatchdogStaleIdle,
        Self::WatchdogExplicitBackground,
        Self::StaleTurnIntake,
        Self::RelayDeadReattach,
        Self::ProbeAutoHeal,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::RelayRecovery => "relay_recovery",
            Self::WatchdogStaleIdle => "watchdog_stale_idle",
            Self::WatchdogExplicitBackground => "watchdog_explicit_background",
            Self::StaleTurnIntake => "stale_turn_intake",
            Self::RelayDeadReattach => "relay_dead_reattach",
            Self::ProbeAutoHeal => "probe_auto_heal",
        }
    }
}

#[cfg(unix)]
fn axis_b_site_for_apply(
    source: RelayRecoveryApplySource,
    action: RelayRecoveryActionKind,
) -> Option<AxisBSite> {
    match (source, action) {
        (RelayRecoveryApplySource::StallWatchdog, RelayRecoveryActionKind::ReattachWatcher) => {
            Some(AxisBSite::RelayDeadReattach)
        }
        (RelayRecoveryApplySource::ProbeAutoHeal, RelayRecoveryActionKind::ReattachWatcher) => {
            Some(AxisBSite::RelayRecovery)
        }
        (
            RelayRecoveryApplySource::ProbeAutoHeal | RelayRecoveryApplySource::StallWatchdog,
            RelayRecoveryActionKind::ClearOrphanPendingToken,
        ) => Some(AxisBSite::ProbeAutoHeal),
        _ => None,
    }
}

#[cfg(unix)]
#[derive(Serialize)]
struct AxisBStamp {
    ts: String,
    host: String,
    api_port: u16,
    process_generation: u64,
    runtime_ptr: String,
    cohort_fingerprint: String,
}

#[cfg(unix)]
#[derive(Serialize)]
struct AxisBRecord<'a> {
    schema: &'static str,
    #[serde(flatten)]
    stamp: AxisBStamp,
    provider: &'a str,
    channel_id: u64,
    site: &'static str,
    structural_action: &'static str,
    structural_eligible: bool,
    ledger_action: &'static str,
    ledger_eligible: bool,
    diff: AxisBDiff,
    #[serde(skip_serializing_if = "Option::is_none")]
    unknown_reason: Option<&'static str>,
    cleanup_delay_ms: i64,
}

#[cfg(unix)]
fn axis_b_dial() -> (RelayAuthorityMode, u8) {
    crate::config_live_reload::current()
        .map(|config| {
            (
                config.runtime.relay_authority_mode,
                config.runtime.relay_authority_cohort_percent,
            )
        })
        .unwrap_or_default()
}

#[cfg(unix)]
fn axis_b_stamp(shared: &SharedData, mode: RelayAuthorityMode, percent: u8) -> AxisBStamp {
    AxisBStamp {
        ts: chrono::Local::now().to_rfc3339(),
        host: std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
        api_port: shared.api_port,
        process_generation: super::runtime_store::process_generation(),
        runtime_ptr: format!("{:p}", std::ptr::from_ref(shared)),
        cohort_fingerprint: cohort::cohort_fingerprint(mode, percent),
    }
}

/// Shadow-plan one destructive candidate and append the comparison. The
/// structural decision remains the caller's only return value and authority.
#[cfg(unix)]
pub(in crate::services::discord) fn observe_axis_b_candidate(
    shared: &SharedData,
    provider: &ProviderKind,
    snapshot: &health::WatcherStateSnapshot,
    site: AxisBSite,
    structural_action: RelayRecoveryActionKind,
    structural_eligible: bool,
    applied_at_ms: i64,
) {
    observe_axis_b_candidate_with_dial(
        shared,
        provider,
        snapshot,
        site,
        structural_action,
        structural_eligible,
        applied_at_ms,
        axis_b_dial(),
    );
}

#[cfg(unix)]
fn observe_axis_b_candidate_with_dial(
    shared: &SharedData,
    provider: &ProviderKind,
    snapshot: &health::WatcherStateSnapshot,
    site: AxisBSite,
    structural_action: RelayRecoveryActionKind,
    structural_eligible: bool,
    applied_at_ms: i64,
    (mode, percent): (RelayAuthorityMode, u8),
) {
    let Some((verdict, observed_at_ms)) = snapshot.reachability_observation() else {
        return;
    };
    if !mode.records_authority_observations()
        || !cohort::admits(mode, percent, snapshot.relay_health.channel_id)
    {
        return;
    }
    let ledger = plan_relay_recovery_under_reachability(
        &snapshot.relay_health,
        snapshot.relay_stall_state,
        verdict,
        applied_at_ms,
    );
    record_axis_b(
        AxisBRecord {
            schema: AXIS_B_SCHEMA,
            stamp: axis_b_stamp(shared, mode, percent),
            provider: provider.as_str(),
            channel_id: snapshot.relay_health.channel_id,
            site: site.as_str(),
            structural_action: structural_action.as_str(),
            structural_eligible,
            ledger_action: ledger.action.as_str(),
            ledger_eligible: ledger.auto_heal.eligible,
            diff: AxisBDiff::from_outcomes(
                structural_action,
                structural_eligible,
                ledger.action,
                ledger.auto_heal.eligible,
            ),
            unknown_reason: reachability_unknown_reason_label(verdict),
            cleanup_delay_ms: applied_at_ms.saturating_sub(observed_at_ms).max(0),
        },
        site,
    );
}

#[cfg(unix)]
fn record_axis_b(record: AxisBRecord<'_>, site: AxisBSite) {
    let diff = record.diff;
    if let Ok(mut bytes) = serde_json::to_vec(&record) {
        bytes.push(b'\n');
        enqueue_axis_b_jsonl(bytes);
    }
    *AXIS_B_TRIAGE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(format!("{}:{diff:?}", site.as_str()))
        .or_default() += 1;
}

#[cfg(unix)]
fn axis_b_sink() -> &'static mpsc::SyncSender<AxisBWrite> {
    AXIS_B_SINK.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<AxisBWrite>(256);
        std::thread::Builder::new()
            .name("axis-b-jsonl".to_string())
            .spawn(move || {
                while let Ok(write) = receiver.recv() {
                    if append_axis_b_jsonl(&write.path, &write.bytes).is_err() {
                        AXIS_B_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
            .expect("spawn axis-B JSONL writer");
        sender
    })
}

#[cfg(unix)]
fn enqueue_axis_b_jsonl(bytes: Vec<u8>) {
    let Some(path) = axis_b_jsonl_path() else {
        return;
    };
    // Tests append synchronously for deterministic assertions; production uses
    // the bounded, non-blocking queue so recovery never waits on filesystem I/O.
    #[cfg(test)]
    {
        if append_axis_b_jsonl(&path, &bytes).is_err() {
            AXIS_B_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[cfg(not(test))]
    {
        let write = AxisBWrite { path, bytes };
        if axis_b_sink().try_send(write).is_err() {
            AXIS_B_DROPPED_RECORDS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(unix)]
fn axis_b_jsonl_path() -> Option<PathBuf> {
    super::runtime_store::agentdesk_root().map(|root| {
        root.join("relay_authority")
            .join(format!("{}.jsonl", chrono::Local::now().format("%Y-%m-%d")))
    })
}

#[cfg(unix)]
fn with_axis_b_writer<T>(write: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let _writer = AXIS_B_WRITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write()
}

#[cfg(unix)]
fn append_axis_b_jsonl(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    with_axis_b_writer(|| {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(bytes)
    })
}

#[cfg(unix)]
pub(in crate::services::discord) fn axis_b_observation_report() -> AxisBObservationReport {
    AxisBObservationReport {
        counters: AXIS_B_TRIAGE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        dropped_records: AXIS_B_DROPPED_RECORDS.load(Ordering::Relaxed),
        write_failures: AXIS_B_WRITE_FAILURES.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
type IdleTmuxReattachInflightCandidateHook =
    Arc<dyn Fn(&super::inflight::InflightTurnState) + Send + Sync + 'static>;
#[cfg(test)]
type DestructiveCancelPostGateHook = Arc<dyn Fn() + Send + Sync + 'static>;

#[cfg(test)]
static DESTRUCTIVE_CANCEL_POST_GATE_HOOK: OnceLock<Mutex<Option<DestructiveCancelPostGateHook>>> =
    OnceLock::new();
#[cfg(test)]
static IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK: OnceLock<
    Mutex<Option<IdleTmuxReattachInflightCandidateHook>>,
> = OnceLock::new();

#[cfg(test)]
fn destructive_cancel_post_gate_hook() -> &'static Mutex<Option<DestructiveCancelPostGateHook>> {
    DESTRUCTIVE_CANCEL_POST_GATE_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn run_destructive_cancel_post_gate_hook_for_tests() {
    let hook = destructive_cancel_post_gate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
struct DestructiveCancelPostGateHookGuard;

#[cfg(test)]
impl Drop for DestructiveCancelPostGateHookGuard {
    fn drop(&mut self) {
        *destructive_cancel_post_gate_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

#[cfg(test)]
fn set_destructive_cancel_post_gate_hook_for_tests(
    hook: DestructiveCancelPostGateHook,
) -> DestructiveCancelPostGateHookGuard {
    *destructive_cancel_post_gate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hook);
    DestructiveCancelPostGateHookGuard
}

#[cfg(test)]
fn idle_tmux_reattach_inflight_candidate_hook()
-> &'static Mutex<Option<IdleTmuxReattachInflightCandidateHook>> {
    IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
struct IdleTmuxReattachInflightCandidateHookGuard {
    previous: Option<IdleTmuxReattachInflightCandidateHook>,
}

#[cfg(test)]
impl Drop for IdleTmuxReattachInflightCandidateHookGuard {
    fn drop(&mut self) {
        let mut hook = idle_tmux_reattach_inflight_candidate_hook()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *hook = self.previous.take();
    }
}

#[cfg(test)]
fn set_idle_tmux_reattach_inflight_candidate_hook_for_tests(
    hook: IdleTmuxReattachInflightCandidateHook,
) -> IdleTmuxReattachInflightCandidateHookGuard {
    let mut slot = idle_tmux_reattach_inflight_candidate_hook()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let previous = slot.replace(hook);
    IdleTmuxReattachInflightCandidateHookGuard { previous }
}

pub(in crate::services::discord) async fn run_relay_recovery(
    registry: &HealthRegistry,
    provider_filter: Option<&str>,
    channel_id: u64,
    apply: bool,
) -> Result<RelayRecoveryResponse, RelayRecoveryError> {
    let parsed_provider = match provider_filter.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(provider) => Some(
            ProviderKind::from_str(provider)
                .ok_or_else(|| RelayRecoveryError::InvalidProvider(provider.to_string()))?,
        ),
        None => None,
    };

    let snapshot = match parsed_provider.as_ref() {
        Some(provider) => {
            registry
                .snapshot_watcher_state_for_provider(provider, channel_id)
                .await
        }
        None => registry.snapshot_watcher_state(channel_id).await,
    }
    .ok_or_else(|| RelayRecoveryError::SnapshotNotFound {
        channel_id,
        provider: provider_filter.map(str::to_string),
    })?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut decision =
        plan_relay_recovery(&snapshot.relay_health, snapshot.relay_stall_state, now_ms);
    decision.affected.finalizer_turn_id = snapshot.inflight_finalizer_turn_id;
    trace_relay_recovery_decision(&decision, apply);

    if !apply {
        return Ok(RelayRecoveryResponse {
            ok: true,
            mode: "dry_run",
            applied: false,
            skipped: false,
            decision,
            apply_result: None,
        });
    }

    let provider = ProviderKind::from_str(&decision.provider)
        .ok_or_else(|| RelayRecoveryError::InvalidProvider(decision.provider.clone()))?;
    // Channel-aware: multi-bot deployments register several runtimes per
    // provider, so a name-only lookup would auto-heal the wrong runtime's
    // relay state for this channel.
    let shared = resolve_recovery_shared(registry, &provider, &decision)
        .await
        .ok_or_else(|| RelayRecoveryError::ProviderUnavailable(decision.provider.clone()))?;
    #[cfg(unix)]
    observe_axis_b_candidate(
        &shared,
        &provider,
        &snapshot,
        AxisBSite::RelayRecovery,
        decision.action,
        decision.auto_heal.eligible,
        chrono::Utc::now().timestamp_millis(),
    );
    Ok(apply_relay_recovery_plan(
        registry,
        &shared,
        &provider,
        decision,
        now_ms,
        RelayRecoveryApplySource::Manual,
    )
    .await)
}

async fn resolve_recovery_shared(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    decision: &RelayRecoveryDecision,
) -> Option<Arc<SharedData>> {
    let channel = ChannelId::new(decision.channel_id);
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        registry.shared_for_provider_on_channel(provider, channel),
    )
    .await
    {
        Ok(Some(shared)) => Some(shared),
        Ok(None) => None,
        Err(_) => {
            tracing::warn!(
                provider = provider.as_str(),
                channel_id = decision.channel_id,
                "relay recovery provider/channel runtime resolve timed out; skipping channel-scoped recovery",
            );
            None
        }
    }
}

pub(in crate::services::discord) async fn auto_apply_relay_recovery_for_shared(
    registry: &HealthRegistry,
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    allowed_action: RelayRecoveryActionKind,
    source: RelayRecoveryApplySource,
) -> Result<RelayRecoveryResponse, RelayRecoveryError> {
    auto_apply_relay_recovery_for_shared_at(
        registry,
        shared,
        provider,
        channel_id,
        allowed_action,
        source,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
}

async fn auto_apply_relay_recovery_for_shared_at(
    registry: &HealthRegistry,
    shared: Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    allowed_action: RelayRecoveryActionKind,
    source: RelayRecoveryApplySource,
    now_ms: i64,
) -> Result<RelayRecoveryResponse, RelayRecoveryError> {
    let snapshot = registry
        .snapshot_watcher_state_for_shared(provider, shared.clone(), channel_id)
        .await
        .ok_or_else(|| RelayRecoveryError::SnapshotNotFound {
            channel_id,
            provider: Some(provider.as_str().to_string()),
        })?;

    let mut planning_health = snapshot.relay_health.clone();
    // The watchdog death-evidence exemption only applies when the caller is
    // requesting orphan-token cleanup. A StallWatchdog caller requesting
    // ReattachWatcher (relay_dead_reattach) must keep the real snapshot stall
    // state so `plan_relay_recovery` can return `ReattachWatcher`; forcing
    // `OrphanPendingToken` here would always mismatch `allowed_action` and
    // silently disable the relay-dead reattach lane (#4569 review regression).
    let planning_stall_state = if source == RelayRecoveryApplySource::StallWatchdog
        && allowed_action == RelayRecoveryActionKind::ClearOrphanPendingToken
    {
        // The watchdog caller reaches this source only after its independent
        // death-evidence gate authorizes cleanup. Plan against that committed
        // verdict without mutating the real watcher before mailbox reclaim is
        // known to have applied.
        planning_health.tmux_session = None;
        planning_health.tmux_alive = None;
        planning_health.watcher_attached = false;
        planning_health.watcher_attached_stale = false;
        planning_health.watcher_owner_channel_id = None;
        planning_health.watcher_owns_live_relay = false;
        RelayStallState::OrphanPendingToken
    } else {
        snapshot.relay_stall_state
    };
    let mut decision = plan_relay_recovery(&planning_health, planning_stall_state, now_ms);
    if source == RelayRecoveryApplySource::StallWatchdog
        && decision.relay_stall_state == RelayStallState::OrphanPendingToken
        && decision.auto_heal.skipped_reason == Some("orphan_token_within_admission_grace")
    {
        decision.auto_heal.eligible =
            eligible_orphan_pending_token_without_admission_grace(&planning_health);
        decision.auto_heal.skipped_reason = None;
    }
    decision.affected.finalizer_turn_id = snapshot.inflight_finalizer_turn_id;
    trace_relay_recovery_decision(&decision, true);
    #[cfg(unix)]
    if let Some(site) = axis_b_site_for_apply(source, allowed_action) {
        observe_axis_b_candidate(
            &shared,
            provider,
            &snapshot,
            site,
            decision.action,
            decision.auto_heal.eligible,
            chrono::Utc::now().timestamp_millis(),
        );
    }

    if decision.action != allowed_action {
        decision.auto_heal.skipped_reason = Some("auto_heal_action_not_allowed");
        trace_relay_recovery_skipped(&decision, decision.auto_heal.skipped_reason);
        return Ok(RelayRecoveryResponse {
            ok: false,
            mode: "apply",
            applied: false,
            skipped: true,
            decision,
            apply_result: None,
        });
    }

    Ok(apply_relay_recovery_plan(registry, &shared, provider, decision, now_ms, source).await)
}

/// Only statuses whose apply performed a real transition belong here. #5021:
/// `reuse_existing_live_watcher` used to be listed, but it reports that
/// `apply_rebind` left the watcher registry as it found it — nothing spawned,
/// nothing replaced — so counting it applied made every watchdog pass look like
/// a successful heal and the auto-heal budget never reached its failure backoff.
/// That status settles through the refund arm of `settle_auto_heal_confirmation`
/// instead. The watcher registry is all this status reports on: the rebind that
/// produced it still committed its episode side effects — `DiscordSession`
/// re-registration and the existing-inflight re-adoption in
/// `commit_episode_side_effects` — before the claim reused the incumbent.
fn relay_recovery_status_counts_as_applied(status: &'static str) -> bool {
    matches!(
        status,
        "applied"
            | "reattached_watcher"
            | "reattach_confirm_startup_grace"
            | "reattach_confirm_emission_in_flight"
            | "cleared_idle_tmux_stale_turn"
            | "scheduled_pending_queue_drain"
    )
}

/// #3277 verify-2: `rebind_inflight_for_channel` reports apply honestly through the claim
/// (`claim_or_reuse_watcher`, source `"recovery_restore_inflight"`), which
/// REPLACES a cancelled / heartbeat-stale / paused / output-path-changed
/// same-session incumbent (`find_watcher_by_tmux_session` folds
/// `heartbeat_stale()` into its replace predicate — see the lifecycle
/// truth-table test) but NEVER a genuinely-live fresh-heartbeat handle (no
/// duplicate-relay vector). When the claim reused such a live incumbent
/// (`watcher_spawned == false` — e.g. the heartbeat recovered between the
/// stale-handle decision and the apply, or a reused watcher owns the session
/// under another channel), say so instead of claiming "reattached_watcher".
fn reattach_apply_status(watcher_spawned: bool) -> &'static str {
    if watcher_spawned {
        "reattached_watcher"
    } else {
        "reuse_existing_live_watcher"
    }
}

/// #5021: the reuse no-op stopped counting as an applied heal so the auto-heal
/// budget can back off on a repeating no-transition. The relay-dead watchdog
/// asks a different question — did its reattach lane already run for this
/// channel on this tick — so give it a separate predicate instead of letting it
/// read `applied` for both. Derived from `reattach_apply_status` so the status
/// literal stays in one place.
pub(in crate::services::discord) fn relay_recovery_status_reused_live_watcher(
    status: &str,
) -> bool {
    status == reattach_apply_status(false)
}

fn relay_frontier_dead_reattach_owner(decision: &RelayRecoveryDecision) -> Option<ChannelId> {
    let evidence = &decision.evidence;
    // Destructive watcher cancel is reserved for the dead-frontier shape. Once
    // relay delivered any bytes (`last_relay_offset > 0`), the old recovery
    // invariant applies: keep the turn intact and let rebind restore watcher
    // coverage instead of cancelling a potentially-live CLI turn.
    if decision.relay_stall_state != RelayStallState::TmuxAliveRelayDead
        || !evidence.desynced
        || evidence.tmux_alive != Some(true)
        || !evidence.watcher_attached
        || !evidence.watcher_owns_live_relay
        || evidence.last_relay_offset != 0
    {
        return None;
    }
    Some(ChannelId::new(
        evidence
            .watcher_owner_channel_id
            .unwrap_or(decision.channel_id),
    ))
}

fn trace_relay_recovery_decision(decision: &RelayRecoveryDecision, apply_requested: bool) {
    tracing::info!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        relay_stall_state = decision.relay_stall_state.as_str(),
        action = decision.action.as_str(),
        auto_heal_eligible = decision.auto_heal.eligible,
        apply_requested,
        reason = decision.reason,
        "relay recovery decision"
    );
}

fn trace_relay_recovery_skipped(
    decision: &RelayRecoveryDecision,
    skipped_reason: Option<&'static str>,
) {
    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        provider = decision.provider.as_str(),
        channel_id = decision.channel_id,
        relay_stall_state = decision.relay_stall_state.as_str(),
        action = decision.action.as_str(),
        skipped_reason = skipped_reason.unwrap_or("unknown"),
        "relay recovery auto-heal skipped"
    );
}

#[cfg(all(test, unix))]
mod axis_b_tests {
    use super::*;

    fn quiet_snapshot(channel_id: u64) -> health::WatcherStateSnapshot {
        health::WatcherStateSnapshot {
            provider: "codex".to_string(),
            attached: false,
            tmux_session: None,
            watcher_owner_channel_id: None,
            last_relay_offset: 0,
            inflight_state_present: false,
            last_relay_ts_ms: 0,
            last_capture_offset: None,
            capture_coordinate: health::liveness_authority::CaptureCoordinateObservation::missing(
                None,
            ),
            unread_bytes: None,
            desynced: false,
            reconnect_count: 0,
            inflight_started_at: None,
            inflight_updated_at: None,
            inflight_user_msg_id: None,
            inflight_current_msg_id: None,
            tmux_session_alive: None,
            has_pending_queue: false,
            mailbox_active_user_msg_id: None,
            mailbox_active_turn_nonce: None,
            bound_output_path: None,
            bound_session_id: None,
            transcript_binding_stall: "none",
            inflight_terminal_delivery_committed: false,
            inflight_identity: None,
            inflight_finalizer_turn_id: None,
            inflight_output_path: None,
            #[cfg(unix)]
            reachability_observation: None,
            relay_stall_state: RelayStallState::Healthy,
            relay_health: RelayHealthSnapshot {
                provider: "codex".to_string(),
                channel_id,
                active_turn: RelayActiveTurn::None,
                tmux_session: None,
                tmux_alive: None,
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: None,
                watcher_owns_live_relay: false,
                bridge_inflight_present: false,
                bridge_current_msg_id: None,
                mailbox_has_cancel_token: false,
                mailbox_active_user_msg_id: None,
                mailbox_turn_started_at_ms: None,
                mailbox_turn_age_secs: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: None,
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_relay_age_secs: None,
                last_outbound_activity_ms: None,
                last_capture_offset: None,
                last_relay_offset: 0,
                unread_bytes: None,
                desynced: false,
                stale_thread_proof: false,
                unpaired_active_token_reconfirmed: false,
            },
        }
    }

    #[test]
    fn destructive_consumers_cannot_read_reachability_for_routing() {
        let snapshot = include_str!("health/snapshot.rs");
        let production_snapshot = snapshot
            .split("#[cfg(test)]")
            .next()
            .expect("watcher snapshot production section");
        assert_eq!(
            production_snapshot
                .matches("reachability_observation:")
                .count(),
            1,
            "the snapshot has exactly one reachability observation field"
        );
        assert_eq!(
            production_snapshot
                .matches("reachability_observation,")
                .count(),
            1,
            "the snapshot constructor wires exactly one reachability observation"
        );
        assert_eq!(
            production_snapshot
                .matches("fn reachability_observation(")
                .count(),
            1,
            "the sole reachability-derived helper is the observation accessor"
        );
        assert_eq!(
            production_snapshot
                .matches("self.reachability_observation")
                .count(),
            1,
            "no authority-bearing alias helper may derive from the observation"
        );
        let watcher_snapshot_decl = production_snapshot
            .split("pub struct WatcherStateSnapshot")
            .next()
            .and_then(|prefix| prefix.rsplit("#[derive(").next())
            .expect("WatcherStateSnapshot derive");
        assert!(
            !watcher_snapshot_decl.contains("Debug"),
            "the reachability-bearing snapshot must not expose verdicts through Debug formatting"
        );
        let relay_recovery_source = include_str!("relay_recovery.rs");
        let production_relay_recovery = relay_recovery_source
            .split("#[cfg(all(test, unix))]")
            .next()
            .expect("axis-B production section");
        let static_names = production_relay_recovery
            .lines()
            .filter_map(|line| {
                let mut tokens = line.split_whitespace();
                tokens.find(|token| *token == "static")?;
                tokens.next().map(|name| name.trim_end_matches(':'))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            static_names,
            [
                "AXIS_B_TRIAGE",
                "AXIS_B_SINK",
                "AXIS_B_WRITER",
                "AXIS_B_DROPPED_RECORDS",
                "AXIS_B_WRITE_FAILURES",
                "DESTRUCTIVE_CANCEL_POST_GATE_HOOK",
                "IDLE_TMUX_REATTACH_INFLIGHT_CANDIDATE_HOOK",
            ],
            "relay recovery may not acquire a new global side channel"
        );

        for (source, allowed_observer_calls, fixture_observations, fixture_verdicts) in [
            (include_str!("health/recovery.rs"), 2, 1, 3),
            (
                include_str!("health/recovery/watchdog_decisions.rs"),
                2,
                0,
                0,
            ),
            (include_str!("router/intake_gate/stale_turn.rs"), 4, 3, 1),
            (include_str!("health/relay_auto_heal.rs"), 0, 1, 0),
            (include_str!("health/relay_dead_reattach.rs"), 0, 1, 0),
            (include_str!("relay_recovery/apply.rs"), 0, 0, 0),
        ] {
            assert_eq!(
                source.matches("reachability_observation").count(),
                fixture_observations,
                "only pinned test fixtures may name the raw reachability observation"
            );
            assert_eq!(
                source.matches("ReachabilityVerdict").count(),
                fixture_verdicts,
                "only the snapshot-binding fixture may construct a reachability verdict"
            );
            for forbidden in [
                "reachability_unknown_reason_label",
                "plan_relay_recovery_under_reachability",
                "axis_b_observation_report",
                "AxisBObservationReport",
                "AXIS_B_TRIAGE",
                "Debug::fmt",
                "format!(\"{snapshot:?}\")",
                "format!(\"{:?}\", snapshot)",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "a destructive consumer must not depend on axis-B observation via {forbidden}"
                );
            }
            let observer_calls = source.matches("observe_watchdog_axis_b(").count()
                + source.matches("observe_stale_turn_axis_b(").count()
                + source.matches("observe_axis_b_candidate(").count();
            assert_eq!(
                observer_calls, allowed_observer_calls,
                "observer call sites must remain standalone statements with no routing dependency"
            );
            for routed in [
                "if observe_watchdog_axis_b",
                "if observe_stale_turn_axis_b",
                "if observe_axis_b_candidate",
                "= observe_watchdog_axis_b",
                "= observe_stale_turn_axis_b",
                "= observe_axis_b_candidate",
            ] {
                assert!(
                    !source.contains(routed),
                    "observer result routed via {routed}"
                );
            }
        }
        let observer = include_str!("relay_recovery.rs");
        let production_observer = observer
            .split("#[cfg(all(test, unix))]")
            .next()
            .expect("axis-B production section");
        assert_eq!(
            production_observer
                .matches(".reachability_observation()")
                .count(),
            1
        );
        assert_eq!(
            production_observer
                .matches(".reachability_observation")
                .count(),
            1,
            "the observer must use only the pinned accessor, never the raw field"
        );
        for site in AxisBSite::ALL {
            assert!(observer.contains(site.as_str()));
        }
    }

    #[test]
    fn axis_b_apply_site_mapping_matches_the_six_site_taxonomy() {
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::StallWatchdog,
                RelayRecoveryActionKind::ReattachWatcher,
            ),
            Some(AxisBSite::RelayDeadReattach)
        );
        for source in [
            RelayRecoveryApplySource::ProbeAutoHeal,
            RelayRecoveryApplySource::StallWatchdog,
        ] {
            assert_eq!(
                axis_b_site_for_apply(source, RelayRecoveryActionKind::ClearOrphanPendingToken,),
                Some(AxisBSite::ProbeAutoHeal)
            );
        }
        assert_eq!(
            axis_b_site_for_apply(
                RelayRecoveryApplySource::ProbeAutoHeal,
                RelayRecoveryActionKind::ReattachWatcher,
            ),
            Some(AxisBSite::RelayRecovery),
            "redrive reattach is a plan_relay_recovery consumer in design §3.4"
        );
    }

    fn every_verdict() -> Vec<health::reachability::verdict::ReachabilityVerdict> {
        use health::reachability::verdict::{
            NotAliveObligationState, ReachabilityUnknownReason, ReachabilityVerdict,
            TransportUnknownEvidence,
        };
        vec![
            ReachabilityVerdict::Reachable,
            ReachabilityVerdict::Degraded {
                oldest_unsatisfied_age_secs: 300,
                uncovered_ranges: 2,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::RestartBoundaryCrossed,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::PlaceholderPresent,
            },
            ReachabilityVerdict::TransportUnknown {
                since_secs: 700,
                evidence: TransportUnknownEvidence::UnreleasedDeliveryLease,
            },
            ReachabilityVerdict::Unreachable {
                oldest_unsatisfied_age_secs: 900,
                uncovered_ranges: 4,
            },
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::TranscriptUnresolved, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::NeverObserved, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ProviderUnresolved, 30),
            ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                    NotAliveObligationState::NoneOutstanding,
                ),
                30,
            ),
            ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::IncarnationNotAliveWitnessed(
                    NotAliveObligationState::WithinGrace,
                ),
                30,
            ),
            ReachabilityVerdict::unknown(
                ReachabilityUnknownReason::TranscriptCoordinateDivergence,
                30,
            ),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::RowlessActiveTurn, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReadTruncated, 30),
            ReachabilityVerdict::unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable, 30),
        ]
    }

    #[test]
    fn axis_b_observer_runs_every_verdict_without_returning_authority() {
        let temp = tempfile::tempdir().expect("axis-B temp root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = super::super::make_shared_data_for_tests();
        let before = axis_b_observation_report().counters;
        let shipped_outcomes = [
            (
                AxisBSite::RelayRecovery,
                RelayRecoveryActionKind::ObserveOnly,
                false,
            ),
            (
                AxisBSite::WatchdogStaleIdle,
                RelayRecoveryActionKind::ClearStaleThreadProof,
                true,
            ),
            (
                AxisBSite::WatchdogExplicitBackground,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                true,
            ),
            (
                AxisBSite::StaleTurnIntake,
                RelayRecoveryActionKind::ClearStaleThreadProof,
                true,
            ),
            (
                AxisBSite::RelayDeadReattach,
                RelayRecoveryActionKind::ReattachWatcher,
                true,
            ),
            (
                AxisBSite::ProbeAutoHeal,
                RelayRecoveryActionKind::ClearOrphanPendingToken,
                true,
            ),
        ];
        for (site, structural_action, structural_eligible) in shipped_outcomes {
            for verdict in every_verdict() {
                let mut snapshot = quiet_snapshot(54_641);
                snapshot.reachability_observation = Some((verdict, 900));
                let observer_result = observe_axis_b_candidate_with_dial(
                    &shared,
                    &ProviderKind::Codex,
                    &snapshot,
                    site,
                    structural_action,
                    structural_eligible,
                    1_000,
                    (RelayAuthorityMode::Observe, 100),
                );
                assert_eq!(
                    observer_result,
                    (),
                    "axis-B observer must not return authority for {}",
                    site.as_str()
                );
            }
        }
        let after = axis_b_observation_report().counters;
        for site in AxisBSite::ALL {
            let observed: u64 = after
                .iter()
                .filter(|(key, _)| key.starts_with(&format!("{}:", site.as_str())))
                .map(|(_, count)| count)
                .sum();
            let prior: u64 = before
                .iter()
                .filter(|(key, _)| key.starts_with(&format!("{}:", site.as_str())))
                .map(|(_, count)| count)
                .sum();
            assert_eq!(
                observed - prior,
                every_verdict().len() as u64,
                "test-build verdicts must traverse the production observer for {}",
                site.as_str()
            );
        }
    }

    #[test]
    fn axis_b_append_path_uses_the_framing_lock() {
        let source = include_str!("relay_recovery.rs");
        let production = source
            .split("#[cfg(all(test, unix))]")
            .next()
            .expect("axis-B production section");
        let append = production
            .split("fn append_axis_b_jsonl")
            .nth(1)
            .and_then(|body| body.split("fn axis_b_observation_report").next())
            .expect("axis-B append implementation");
        assert_eq!(append.matches("with_axis_b_writer(||").count(), 1);
    }

    #[test]
    fn axis_b_writer_frames_concurrent_records() {
        use std::sync::{Arc, Barrier};

        struct YieldingBytes {
            bytes: Arc<StdMutex<Vec<u8>>>,
        }

        impl std::io::Write for YieldingBytes {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(bytes[0]);
                std::thread::yield_now();
                Ok(1)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        for round in 0..20 {
            let output = Arc::new(StdMutex::new(Vec::new()));
            let start = Arc::new(Barrier::new(9));
            let mut writers = Vec::new();
            for index in 0..8 {
                let output = Arc::clone(&output);
                let start = Arc::clone(&start);
                writers.push(std::thread::spawn(move || {
                    let record = format!("{{\"round\":{round},\"site\":{index}}}\n");
                    start.wait();
                    with_axis_b_writer(|| {
                        YieldingBytes { bytes: output }.write_all(record.as_bytes())
                    })
                    .expect("write framed axis-B record");
                }));
            }
            start.wait();
            for writer in writers {
                writer.join().expect("join axis-B writer");
            }
            let output = output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let lines: Vec<serde_json::Value> = std::str::from_utf8(&output)
                .expect("axis-B test output is UTF-8")
                .lines()
                .map(|line| serde_json::from_str(line).expect("one complete JSON object per line"))
                .collect();
            assert_eq!(lines.len(), 8);
            assert!(lines.iter().all(|line| line["round"] == round));
        }
    }

    #[test]
    fn axis_b_emit_covers_all_six_sites_without_turn_scope() {
        let temp = tempfile::tempdir().expect("axis-B temp root");
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let shared = super::super::make_shared_data_for_tests();
        let snapshot = quiet_snapshot(54_640);
        for site in AxisBSite::ALL {
            record_axis_b(
                AxisBRecord {
                    schema: AXIS_B_SCHEMA,
                    stamp: axis_b_stamp(&shared, RelayAuthorityMode::Observe, 100),
                    provider: "codex",
                    channel_id: snapshot.relay_health.channel_id,
                    site: site.as_str(),
                    structural_action: RelayRecoveryActionKind::ObserveOnly.as_str(),
                    structural_eligible: false,
                    ledger_action: RelayRecoveryActionKind::ObserveOnly.as_str(),
                    ledger_eligible: false,
                    diff: AxisBDiff::Agree,
                    unknown_reason: None,
                    cleanup_delay_ms: 25,
                },
                site,
            );
        }
        let dir = temp.path().join("relay_authority");
        let path = fs::read_dir(dir)
            .expect("axis-B output directory")
            .next()
            .expect("axis-B JSONL file")
            .expect("axis-B JSONL entry")
            .path();
        let records = fs::read_to_string(path).expect("read axis-B JSONL");
        let lines: Vec<serde_json::Value> = records
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid axis-B record"))
            .collect();
        assert_eq!(lines.len(), AxisBSite::ALL.len());
        for (record, site) in lines.iter().zip(AxisBSite::ALL) {
            assert_eq!(record["schema"], AXIS_B_SCHEMA);
            assert_eq!(record["site"], site.as_str());
            assert_eq!(record["cleanup_delay_ms"], 25);
            assert!(record.get("turn_id").is_none());
        }
    }
}

#[cfg(test)]
#[path = "relay_recovery/tests.rs"]
mod tests;
