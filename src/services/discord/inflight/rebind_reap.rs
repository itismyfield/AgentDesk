use super::*;
use crate::services::platform::tmux::PaneLiveness;

/// #3635: runtime-liveness oracle for the dead-watcher rebind-origin reap path.
///
/// A Watcher-owned orphan can never satisfy
/// [`should_reap_abandoned_rebind_origin`]'s `== None` owner conjunct, and is
/// shape-identical to a live-but-idle row (#3154/#3540 require live Watcher
/// rebinds to survive restarts). Only a *runtime* probe can tell them apart,
/// hence this trait, stubbable in tests.
pub(super) trait WatcherLiveness {
    /// True only when the watcher owning `state` is *provably* dead **or
    /// idle-stuck**: no runtime activity has advanced within
    /// [`DEAD_WATCHER_PROVEN_DEAD_SECS`], regardless of tmux pane state
    /// (#3879). Unknown probes or a missing session name yield `false`.
    fn is_proven_dead(&self, state: &InflightTurnState) -> bool;
}

/// #3635: production [`WatcherLiveness`] using the same signals the
/// stall-watchdog (#3169/#3629) trusts: tmux pane liveness + runtime activity.
pub(super) struct RuntimeWatcherLiveness;

impl WatcherLiveness for RuntimeWatcherLiveness {
    fn is_proven_dead(&self, state: &InflightTurnState) -> bool {
        // No session name to probe => cannot prove death => never reap.
        let Some(session) = state.tmux_session_name.as_deref() else {
            return false;
        };
        let session = session.trim();
        if session.is_empty() {
            return false;
        }
        // A transient probe failure is "unknown", not "dead" — preserve.
        let pane = crate::services::tmux_diagnostics::tmux_session_pane_liveness(session);
        if pane == PaneLiveness::ProbeError {
            return false;
        }
        proven_dead_from_signals(pane, watcher_runtime_activity_recent(session))
    }
}

/// Pure proven-dead/idle-stuck decision from the two probed signals,
/// extracted so tests can pin every `(pane, activity)` combination without
/// spawning tmux or touching jsonl/`.generation` files. Reapable only on
/// activity quiescence, regardless of pane state (#3879); an unknown probe
/// (`ProbeError`) always preserves.
pub(super) fn proven_dead_from_signals(pane: PaneLiveness, activity_recent: bool) -> bool {
    match pane {
        PaneLiveness::ProbeError => false,
        PaneLiveness::Live | PaneLiveness::DeadOrAbsent => !activity_recent,
    }
}

/// #3635: true when the watcher's runtime files (jsonl / `.generation` mtime)
/// advanced within [`DEAD_WATCHER_PROVEN_DEAD_SECS`]. Pure fs stat, safe under
/// the sidecar lock; 0 (no resolvable file) counts as no recent activity.
pub(super) fn watcher_runtime_activity_recent(session: &str) -> bool {
    let latest_nanos =
        crate::services::dispatched_sessions::latest_runtime_activity_unix_nanos(session);
    latest_nanos > 0 && {
        let age_secs = now_unix()
            .saturating_sub(latest_nanos / 1_000_000_000)
            .max(0) as u64;
        age_secs < DEAD_WATCHER_PROVEN_DEAD_SECS
    }
}

/// Test-only composition of [`dead_watcher_rebind_structurally_reapable`]
/// with an injected [`WatcherLiveness`], so tests can pin every
/// `(structural, liveness)` combination. Same gate the production sweeper
/// uses (see [`sweep_reap_dead_watcher_rebind_origin`]).
#[cfg(test)]
pub(super) fn should_reap_dead_watcher_rebind_origin(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
    liveness: &dyn WatcherLiveness,
) -> bool {
    dead_watcher_rebind_structurally_reapable(state, age_secs, current_generation)
        && liveness.is_proven_dead(state)
}

/// The structural + deadline/generation half of the dead-watcher reap
/// predicate, without the liveness probe — split out so the locked
/// re-validation can re-check these cheap fs-only conditions without a tmux
/// subprocess. Byte-identical to [`should_reap_abandoned_rebind_origin`]
/// except the owner conjunct is `== Watcher` instead of `== None`.
pub(super) fn dead_watcher_rebind_structurally_reapable(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> bool {
    if !state.rebind_origin {
        return false;
    }
    let structurally_abandoned = state.turn_source == TurnSource::ExternalAdopted
        && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher
        && state.user_msg_id == 0
        && state.current_msg_id == 0
        && !state.terminal_delivery_committed
        && state.response_sent_offset == 0
        && state.full_response.is_empty()
        && state.last_offset == state.turn_start_offset.unwrap_or(state.last_offset)
        && state.restart_mode.is_none();
    if !structurally_abandoned {
        return false;
    }

    // Not reaped the instant it's born: past-deadline OR stale-generation.
    let deadline = state
        .rebind_origin_deadline_secs
        .unwrap_or_else(rebind_origin_deadline_secs_env);
    let past_deadline = age_secs >= deadline;
    let stale_generation = state
        .rebind_origin_birth_generation
        .is_some_and(|birth| birth != current_generation);
    past_deadline || stale_generation
}

/// Best-effort age (seconds) for a rebind-origin row: prefers the persisted
/// `rebind_origin_created_at_unix` stamp, falling back to file mtime for
/// legacy rows. 0 means neither is available (the conservative outcome).
pub(super) fn rebind_origin_age_secs(path: &Path, state: &InflightTurnState) -> u64 {
    if let Some(created) = state.rebind_origin_created_at_unix {
        return now_unix().saturating_sub(created).max(0) as u64;
    }
    fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// #3835: staleness predicates + rebind-origin reap helpers. Unqualified items
// (now_unix, inflight_state_path, ...) resolve through the `use super::*` glob above.

/// #1446: an inflight state is "stale" (writer likely terminated without
/// cleanup) when `updated_at` hasn't advanced for this many seconds.
/// THREAD-GUARD uses this value directly; the stall-watchdog uses `2x` to
/// stay more conservative. Not a true heartbeat — a healthy foreground call
/// can go silent for minutes, so this only gates recovery once an explicit
/// signal failed to fire (a false-positive live-turn cleanup is worse than
/// the delay).
pub(in crate::services::discord) const INFLIGHT_STALENESS_THRESHOLD_SECS: u64 = 300;

/// #3581: default reap deadline (seconds) for an unadopted, never-progressed
/// `rebind_origin` row — shorter than the sweeper's 1800s safety net, longer
/// than the ~8s TUI-adopt backstop. Overridable via `AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS`.
pub(in crate::services::discord) const REBIND_ORIGIN_DEADLINE_SECS_DEFAULT: u64 = 120;

/// #3581: floor for the env-overridden deadline, guarding against a
/// pathologically small override racing the adoption backstop.
const REBIND_ORIGIN_DEADLINE_SECS_MIN: u64 = 30;

/// #3635: minimum quiescence (seconds) of runtime liveness signals before a
/// Watcher-owned rebind-origin row is "proven dead" and reapable — far larger
/// than the stall-watchdog's 120s window so a live Watcher merely idle
/// between turns is never false-classified as dead (#3154/#3540).
pub(in crate::services::discord) const DEAD_WATCHER_PROVEN_DEAD_SECS: u64 = 600;

/// #3581: resolve the rebind-origin reap deadline from
/// `AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS` (clamped to [`REBIND_ORIGIN_DEADLINE_SECS_MIN`]),
/// falling back to [`REBIND_ORIGIN_DEADLINE_SECS_DEFAULT`] on absence/parse failure.
pub(in crate::services::discord) fn rebind_origin_deadline_secs_env() -> u64 {
    std::env::var("AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|secs| secs.max(REBIND_ORIGIN_DEADLINE_SECS_MIN))
        .unwrap_or(REBIND_ORIGIN_DEADLINE_SECS_DEFAULT)
}

/// #3581: decide whether an abandoned `rebind_origin` inflight row is safe to
/// reap. Strict conjunction of never-progressed/never-adopted/never-owned
/// signals so a live rebind (MonitorTriggered watcher rebind) or an
/// adopted/relaying row is never reaped. NOTE: "no progress" is
/// `last_offset == turn_start_offset` (offset equality), not
/// `last_offset == 0` — a fresh row can be born with both already > 0.
/// Reaped iff past deadline OR born in a prior generation; `age_secs` is
/// caller-supplied (file-mtime in the sweeper path) for legacy rows.
pub(in crate::services::discord) fn should_reap_abandoned_rebind_origin(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> bool {
    if !state.rebind_origin {
        return false;
    }
    let structurally_abandoned = state.turn_source == TurnSource::ExternalAdopted
        && state.effective_relay_owner_kind() == RelayOwnerKind::None
        && state.user_msg_id == 0
        && state.current_msg_id == 0
        && !state.terminal_delivery_committed
        && state.response_sent_offset == 0
        && state.full_response.is_empty()
        && state.last_offset == state.turn_start_offset.unwrap_or(state.last_offset)
        && state.restart_mode.is_none();
    if !structurally_abandoned {
        return false;
    }

    let deadline = state
        .rebind_origin_deadline_secs
        .unwrap_or_else(rebind_origin_deadline_secs_env);
    let past_deadline = age_secs >= deadline;
    let stale_generation = state
        .rebind_origin_birth_generation
        .is_some_and(|birth| birth != current_generation);
    past_deadline || stale_generation
}

/// #3581: operator-visibility event for a reaped abandoned rebind-origin row
/// (#3561), mirroring `evict_stale_generation`'s shape.
pub(in crate::services::discord) fn emit_reap_abandoned_rebind_origin(
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
    reason: &str,
) {
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        state.channel_id,
        state.dispatch_id.as_deref(),
        None,
        None,
        "reap_abandoned_rebind_origin",
        serde_json::json!({
            "reason": reason,
            "age_secs": age_secs,
            "deadline_secs": state
                .rebind_origin_deadline_secs
                .unwrap_or_else(rebind_origin_deadline_secs_env),
            "birth_generation": state.rebind_origin_birth_generation,
            "current_generation": current_generation,
            "turn_source": state.turn_source.as_str(),
            "tmux_session_name": state.tmux_session_name,
        }),
    );
}

/// Outcome of a locked rebind-origin reap attempt, so callers/tests can
/// distinguish reaped / skipped (row replaced or no longer eligible) / missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RebindReapOutcome {
    /// The row was re-validated under the lock and unlinked.
    Reaped,
    /// The on-disk row no longer satisfies the reap predicate (e.g. replaced
    /// by a live intake/claim since the snapshot).
    Skipped,
    /// The state file was already absent (idempotent no-op) or unreadable.
    Missing,
    /// The advisory lock could not be acquired — the caller should retry later.
    LockUnavailable,
}

/// True when the on-disk `locked` row is still the *same* orphan identified
/// by the unlocked `snapshot` — a new intake/claim could persist a different
/// orphan at the same path first. Requires birth identity
/// (`rebind_origin_created_at_unix` + `rebind_origin_birth_generation`) to match.
fn rebind_row_identity_unchanged(snapshot: &InflightTurnState, locked: &InflightTurnState) -> bool {
    locked.rebind_origin == snapshot.rebind_origin
        && locked.rebind_origin_created_at_unix == snapshot.rebind_origin_created_at_unix
        && locked.rebind_origin_birth_generation == snapshot.rebind_origin_birth_generation
        && locked.turn_start_offset == snapshot.turn_start_offset
}

/// Reap an abandoned rebind-origin orphan under the sidecar lock:
/// re-validate-then-unlink. Reload, confirm it's still the same orphan
/// (`rebind_row_identity_unchanged`) and still eligible under a fresh age,
/// then unlink — otherwise skip. Shared by the sweeper and boot paths.
pub(super) fn reap_abandoned_rebind_origin_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> RebindReapOutcome {
    let path = inflight_state_path(root, provider, snapshot.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return RebindReapOutcome::LockUnavailable;
    };
    let Some(locked) = read_inflight_state_content(&path) else {
        return RebindReapOutcome::Missing;
    };
    if !rebind_row_identity_unchanged(snapshot, &locked) {
        return RebindReapOutcome::Skipped;
    }
    let age_secs = rebind_origin_age_secs(&path, &locked);
    if !should_reap_abandoned_rebind_origin(&locked, age_secs, current_generation) {
        return RebindReapOutcome::Skipped;
    }
    log_inflight_remove(
        provider,
        locked.channel_id,
        locked.user_msg_id,
        "reap_abandoned_rebind_origin_locked",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => RebindReapOutcome::Reaped,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RebindReapOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = snapshot.channel_id,
                error = %error,
                "#3581 rebind reap remove_file failed under lock; treating as Missing"
            );
            RebindReapOutcome::Missing
        }
    }
}

/// #3581: env-rooted wrapper around
/// [`reap_abandoned_rebind_origin_locked_in_root`] for the sweeper path.
pub(in crate::services::discord) fn reap_abandoned_rebind_origin_locked(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    reap_abandoned_rebind_origin_locked_in_root(&root, provider, snapshot, current_generation)
        == RebindReapOutcome::Reaped
}

/// Locked re-validate-then-unlink for the dead-watcher rebind-origin reap
/// path. The expensive tmux probe runs OUTSIDE this lock (via
/// `spawn_blocking`); only cheap fs-only conditions are re-checked here, so
/// the sidecar lock is never held across subprocess I/O.
///
/// Closes the row-replacement race (birth-identity guard); does not fully
/// close a pane that re-appears mid-window (benign — a re-adopting watcher
/// just re-creates the orphan). A re-read of the activity mtime catches a
/// watcher that resumed writing.
pub(super) fn reap_dead_watcher_rebind_origin_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> RebindReapOutcome {
    let path = inflight_state_path(root, provider, snapshot.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return RebindReapOutcome::LockUnavailable;
    };
    let Some(locked) = read_inflight_state_content(&path) else {
        return RebindReapOutcome::Missing;
    };
    if !rebind_row_identity_unchanged(snapshot, &locked) {
        return RebindReapOutcome::Skipped;
    }
    let age_secs = rebind_origin_age_secs(&path, &locked);
    if !dead_watcher_rebind_structurally_reapable(&locked, age_secs, current_generation) {
        return RebindReapOutcome::Skipped;
    }
    // Last guard: recent activity since the unlocked probe means alive ⇒ skip.
    if let Some(session) = locked.tmux_session_name.as_deref() {
        let session = session.trim();
        if !session.is_empty() && watcher_runtime_activity_recent(session) {
            return RebindReapOutcome::Skipped;
        }
    }
    log_inflight_remove(
        provider,
        locked.channel_id,
        locked.user_msg_id,
        "reap_dead_watcher_rebind_origin_locked",
        &path,
    );
    match fs::remove_file(&path) {
        Ok(()) => RebindReapOutcome::Reaped,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RebindReapOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = snapshot.channel_id,
                error = %error,
                "#3635 dead-watcher rebind reap remove_file failed under lock; treating as Missing"
            );
            RebindReapOutcome::Missing
        }
    }
}

/// #3635: env-rooted wrapper around
/// [`reap_dead_watcher_rebind_origin_locked_in_root`] for the sweeper path.
pub(in crate::services::discord) fn reap_dead_watcher_rebind_origin_locked(
    provider: &ProviderKind,
    snapshot: &InflightTurnState,
    current_generation: u64,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    reap_dead_watcher_rebind_origin_locked_in_root(&root, provider, snapshot, current_generation)
        == RebindReapOutcome::Reaped
}

/// The placeholder sweeper's entry point for the dead-watcher rebind-origin
/// reap. Cheapest-first: fs-only structural gate, then the
/// [`RuntimeWatcherLiveness`] probe (`spawn_blocking`, outside any lock),
/// then the locked re-validate. Returns `true` only when genuinely unlinked.
///
/// Not called from the boot path: a just-restarted watcher's session reads
/// as dead at cold start, so the liveness gate only fires in the warm sweeper.
pub(in crate::services::discord) async fn sweep_reap_dead_watcher_rebind_origin(
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> bool {
    if !dead_watcher_rebind_structurally_reapable(state, age_secs, current_generation) {
        return false;
    }
    let probe_state = state.clone();
    // A spawn_blocking join failure (panic/shutdown) is treated as unknown ⇒ preserve.
    let proven_dead =
        tokio::task::spawn_blocking(move || RuntimeWatcherLiveness.is_proven_dead(&probe_state))
            .await
            .unwrap_or(false);
    if !proven_dead {
        return false;
    }
    reap_dead_watcher_rebind_origin_locked(provider, state, current_generation)
}

/// Parse the persisted `started_at` (`now_string` localtime form) into a
/// Unix timestamp; `None` for unparseable values.
pub(in crate::services::discord) fn parse_started_at_unix(started_at: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.timestamp())
}

/// Parse a persisted `updated_at` field (same encoding as `started_at`) back
/// into a Unix timestamp. Kept distinct from `parse_started_at_unix` purely
/// for call-site readability.
pub(in crate::services::discord) fn parse_updated_at_unix(updated_at: &str) -> Option<i64> {
    parse_started_at_unix(updated_at)
}

/// #1446: `true` when the persisted `updated_at` of an inflight state is
/// older than `threshold_secs` relative to `now_unix_secs`. Returns `false`
/// if unparseable — staleness is never inferred from missing data.
pub(in crate::services::discord) fn inflight_state_is_stale(
    state: &InflightTurnState,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(updated_at_unix) = parse_updated_at_unix(&state.updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

fn inflight_state_started_at_is_stale(
    state: &InflightTurnState,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(started_at_unix) = parse_started_at_unix(&state.started_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(started_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

/// A TUI-direct `ExternalInput` row can be born as a bridge-owned synthetic
/// claim before the bridge tail creates the real placeholder; if dcserver
/// restarts in that window, the row is left with no live relay owner. Treat
/// that shape as stale so scanners/health recovery don't block forever.
pub(in crate::services::discord) fn ownerless_external_input_inflight_is_stale_at(
    state: &InflightTurnState,
    now_unix_secs: i64,
) -> bool {
    state.turn_source == TurnSource::ExternalInput
        && state.effective_relay_owner_kind() == RelayOwnerKind::None
        && state.injected_prompt_message_id.is_some()
        && state.current_msg_id == 0
        && state.response_sent_offset == 0
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
        && !state.terminal_delivery_committed
        // #3976: a confirmed SessionBoundRelay delivery sets this durable marker,
        // so it must not be re-recovered as a never-delivered black-hole.
        && !state.session_bound_delivered
        && (inflight_state_is_stale(state, now_unix_secs, INFLIGHT_STALENESS_THRESHOLD_SECS)
            || (state.restart_mode.is_some()
                && inflight_state_started_at_is_stale(
                    state,
                    now_unix_secs,
                    INFLIGHT_STALENESS_THRESHOLD_SECS,
                )))
}

pub(in crate::services::discord) fn ownerless_external_input_inflight_is_stale(
    state: &InflightTurnState,
) -> bool {
    ownerless_external_input_inflight_is_stale_at(state, now_unix())
}
