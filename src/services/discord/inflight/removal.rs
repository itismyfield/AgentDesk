//! Inflight removal, stale-generation invalidation, and load-time pruning.

use super::*;
use std::collections::HashMap;
use std::path::Path;

fn channel_id_from_path(path: &Path) -> u64 {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<u64>().ok())
        .unwrap_or(0)
}

fn user_msg_id_for_inflight_remove_log(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| parse_inflight_state_content(&content).ok())
        .map(|state| state.user_msg_id)
        .unwrap_or(0)
}

pub(crate) fn log_inflight_remove(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    reason: &'static str,
    path: &Path,
) {
    tracing::warn!(
        target: "agentdesk::inflight_remove",
        provider = %provider.as_str(),
        channel_id,
        user_msg_id,
        reason,
        path = %path.display(),
        "discord inflight state row removal"
    );
}

fn inflight_age_secs_for_path(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
        .map(|age| age.as_secs())
}

fn log_loader_inflight_remove(
    provider: &ProviderKind,
    channel_id: u64,
    user_msg_id: u64,
    reason: &'static str,
    path: &Path,
    state: Option<&InflightTurnState>,
    current_generation: u64,
) {
    let born_generation = state.map(|row| row.born_generation);
    let tmux_session_name_present = state.is_some_and(|row| row.tmux_session_name.is_some());
    let age_secs = inflight_age_secs_for_path(path);
    tracing::warn!(
        target: "agentdesk::inflight_remove",
        provider = %provider.as_str(),
        channel_id,
        user_msg_id,
        reason,
        path = %path.display(),
        born_generation = ?born_generation,
        current_generation,
        tmux_session_name_present,
        age_secs = ?age_secs,
        "discord inflight state row removal"
    );
}

fn loader_gate_refuses(state: &InflightTurnState, current_generation: u64) -> bool {
    row_is_current_generation(state, current_generation) && state.tmux_session_name.is_some()
}

fn loader_gate_refuses_with_allocation(
    state: &InflightTurnState,
    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,
) -> bool {
    loader_gate_refuses(state, allocation.generation)
}

/// §7.2-1's refusal payload for the loader fence, filed under an invariant name
/// it SHARES with `clear_store::reconcile_gate::refusal_details`.
///
/// The two producers publish DIFFERENT field sets under that one name (#5462 S5
/// r3). Here `user_msg_id` / `finalizer_turn_id` / `turn_nonce` / `updated_at` /
/// `save_generation` / `tmux_session_name` are unprefixed because they come off
/// the row this fence just read under the lock. The reconcile site builds those
/// same six from a caller snapshot that is already stale by the time its fence
/// judges, so #5462 S5 r2 gave them a `snapshot_` prefix rather than let them
/// stand in for the row it protected. Unifying the schemas is not the fix —
/// each spelling is true where it stands — so `site` is the discriminator: the
/// `load_inflight_states_from_root_stale` written below, against the reconcile
/// site's `clear_*_for_reconcile`. A §9-2-style query filtering an unprefixed
/// field (`details->>'tmux_session_name' IS NOT NULL`) therefore counts this
/// site only, and drops every reconcile-gate row without saying so.
fn record_loader_generation_gate(
    state: &InflightTurnState,
    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,
    path: &Path,
) {
    record_inflight_invariant_with_severity(
        false,
        state,
        "reconcile_never_clears_current_generation_row",
        "src/services/discord/inflight/removal.rs:load_inflight_states_from_root",
        "destructive inflight loader must preserve a named row matching the process generation",
        serde_json::json!({
            "site": "load_inflight_states_from_root_stale",
            "born_generation": state.born_generation,
            "current_generation": allocation.generation,
            "epoch_route": allocation.epoch_route(),
            "epoch_advanced": allocation.epoch_advanced(),
            "user_msg_id": state.user_msg_id,
            "finalizer_turn_id": state.finalizer_turn_id,
            "turn_nonce": state.turn_nonce,
            "updated_at": state.updated_at,
            "save_generation": state.save_generation,
            "tmux_session_name": state.tmux_session_name,
            "path": path.display().to_string(),
        }),
        ObsSeverity::Warn,
    );
}

/// Lifecycle kind for the loader fence's allow counter (#5462 S5 r2).
///
/// Deliberately NOT `reconcile_generation_gate_allowed`: that kind belongs to
/// `clear_store/reconcile_gate.rs`, where "allowed" means the fence delegated
/// and the identity guard behind it may still refuse, and where the payload
/// carries `generation_relation` / `current_generation_nonzero` /
/// `delegated_outcome`. Here "allowed" means the row is unlinked immediately and
/// none of those three fields exist. Sharing one kind put two incompatible
/// schemas and two meanings of "allowed" behind a single name, so
/// `delegated_outcome = "Cleared"` — the natural filter for "the gate let a real
/// destruction through" — silently dropped every loader row.
const LOADER_GENERATION_GATE_ALLOWED: &str = "loader_generation_gate_allowed";

/// #5462 S5 §7.2-2 for the loader fence. Its refusal above is an invariant WARN,
/// so without this the analysis stream counts blocked removals but not permitted
/// ones — and a reject count with no allow count cannot tell an over-blocking
/// fence from a quiet boot. §7.1: `emit_inflight_lifecycle_event` never reaches
/// stdout, so §7.2-2's `tracing::info!` half is what makes the allow side
/// visible in a release log at all.
///
/// The generation/name triple is raw rather than bucketed because §9-2's transfer
/// condition is one combination this fence deliberately does NOT refuse (current
/// generation, no tmux name), which becomes a filter over these fields. §7.2-6's
/// removal log (`log_loader_inflight_remove`) carries the same triple by NAME,
/// but not in the same representation: it renders `born_generation` as
/// `Some(N)`/`None` rather than a number, and adds `age_secs` that the allow
/// side has no equivalent for. The tmux item is NOT one of the differences —
/// the removal log derives the same bool under the same
/// `tmux_session_name_present` spelling (#5462 S5 r3 correction). The site that
/// publishes the raw string `tmux_session_name` is the loader's REFUSAL WARN,
/// `record_loader_generation_gate` above, which is the stream this event is
/// actually the counterpart of.
///
/// A fourth difference runs the other way, so "the same triple by NAME" does not
/// cover it: `current_generation_row`, the derived fence bool below, exists on
/// the allow event alone — neither the removal log nor the refusal WARN carries
/// it, and both would have to recompute it from their generation pair. Joining
/// any two of these streams needs those normalizations first.
///
/// One more asymmetry, this time in the reconcile allow event rather than here
/// (#5462 S5 r4): its `born_generation` and `generation_relation` are spelled
/// WITHOUT the `snapshot_` prefix that its own refusal payload gives stale
/// caller fields, yet both are derived from that caller snapshot and not from a
/// locked re-read — only its `current_generation_nonzero` comes from the epoch.
/// Every generation field on THIS event instead comes off the row this fence
/// just read under its own lock. That site also counts rows this one never
/// could: its `Delegated(GuardedClearOutcome::Missing)` arm stamps a bucket even
/// when the state file could not be read or could not be parsed, so the row it
/// classifies may no longer be on disk at all.
fn emit_loader_generation_gate_allowed(
    provider: &ProviderKind,
    state: &InflightTurnState,
    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,
    path: &Path,
) {
    let current_generation_row = row_is_current_generation(state, allocation.generation);
    let epoch_route = allocation.epoch_route();
    let epoch_advanced = allocation.epoch_advanced();
    let tmux_session_name_present = state.tmux_session_name.is_some();
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        state.channel_id,
        state.dispatch_id.as_deref(),
        state.session_key.as_deref(),
        None,
        LOADER_GENERATION_GATE_ALLOWED,
        serde_json::json!({
            "site": "load_inflight_states_from_root_stale",
            "born_generation": state.born_generation,
            "current_generation": allocation.generation,
            "epoch_route": epoch_route,
            "epoch_advanced": epoch_advanced,
            "current_generation_row": current_generation_row,
            "tmux_session_name_present": tmux_session_name_present,
            "user_msg_id": state.user_msg_id,
            "path": path.display().to_string(),
        }),
    );
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = state.channel_id,
        user_msg_id = state.user_msg_id,
        born_generation = state.born_generation,
        current_generation = allocation.generation,
        epoch_route,
        epoch_advanced,
        current_generation_row,
        tmux_session_name_present,
        site = "load_inflight_states_from_root_stale",
        path = %path.display(),
        "loader generation gate allowed destructive inflight row removal"
    );
}

pub(crate) fn log_inflight_remove_for_path(
    provider: &ProviderKind,
    channel_id: u64,
    reason: &'static str,
    path: &Path,
) {
    log_inflight_remove(
        provider,
        channel_id,
        user_msg_id_for_inflight_remove_log(path),
        reason,
        path,
    );
}

/// #2437 (#2427 C wire) boot-time bulk invalidate. Removes inflight
/// state files whose `restart_generation` does not match
/// `current_generation` AND that are NOT planned-restart rows. The
/// planned-restart gate in `stale_removal_reason` (this file, the
/// `state.restart_mode.is_some()` branch) already handles its own
/// generation-mismatch eviction with `DRAIN_RESTART_MAX_AGE_SECS` /
/// `HOT_SWAP_HANDOFF_MAX_AGE_SECS` retention — do not double-evict
/// those here or recovery will lose handoff rows from the prior
/// generation.
///
/// Skips:
///   * `state.restart_mode.is_some()` — planned restart / hot-swap.
///   * `state.rebind_origin` — rebind API owns these, not generation.
///   * `state.restart_generation == Some(current_generation)` — this
///     generation's own rows.
///
/// Returns the number of state files removed. Intended to be called
/// **once per provider** at dcserver boot, BEFORE
/// `restore_inflight_turns`, so recovery does not revive a row from a
/// generation whose tmux session no longer exists.
pub(crate) fn invalidate_stale_generation(
    provider: &ProviderKind,
    current_generation: u64,
) -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    let removed = invalidate_stale_generation_in_root(&root, provider, current_generation);
    removed.len()
}

/// Test-friendly variant. Returns the list of evicted `(channel_id,
/// row_generation)` tuples so unit tests can pin both the count and
/// the row identities without re-loading the directory.
pub(super) fn invalidate_stale_generation_in_root(
    root: &Path,
    provider: &ProviderKind,
    current_generation: u64,
) -> Vec<(u64, Option<u64>)> {
    let states = load_inflight_states_from_root(root, provider);
    let mut removed = Vec::new();
    for state in states {
        if state.restart_mode.is_some() {
            continue;
        }
        if state.rebind_origin {
            // #3581: a rebind-origin row is normally owned by the rebind API
            // and skipped here. The one exception is an abandoned, never-
            // progressed orphan from a STALL-WATCHDOG respawn: reap it at boot
            // if it is past its deadline OR was born in a prior generation.
            // The reap predicate's strict conjunction guarantees a live /
            // adopted rebind is never touched.
            //
            // #3581 (codex TOCTOU fix): gate the unlocked-snapshot pre-check
            // with the same locked re-validate-then-unlink helper the periodic
            // sweeper now uses, so boot and sweeper stay consistent and a row
            // replaced between the snapshot and the lock is never wiped.
            let path = inflight_state_path(root, provider, state.channel_id);
            let age_secs = rebind_origin_age_secs(&path, &state);
            if should_reap_abandoned_rebind_origin(&state, age_secs, current_generation)
                && reap_abandoned_rebind_origin_locked_in_root(
                    root,
                    provider,
                    &state,
                    current_generation,
                ) == RebindReapOutcome::Reaped
            {
                emit_reap_abandoned_rebind_origin(
                    provider,
                    &state,
                    age_secs,
                    current_generation,
                    "invalidate_stale_generation_boot",
                );
                removed.push((state.channel_id, state.rebind_origin_birth_generation));
            }
            continue;
        }
        // Codex review HIGH on PR #2460: normal rows are constructed with
        // `restart_generation: None` (see `InflightTurnState::new`). The
        // previous `Some(current_generation)` guard alone would evict every
        // healthy current-generation row at boot. Preserve unstamped rows
        // too so only rows explicitly stamped from a PRIOR generation are
        // evicted. (Stale unstamped rows are still bounded by the
        // intake-time staleness threshold path; this function is the
        // boot-time hammer, not the long-lived cleaner.)
        match state.restart_generation {
            None => continue,
            Some(row_generation) if row_generation == current_generation => continue,
            Some(_) => {}
        }
        let path = inflight_state_path(root, provider, state.channel_id);
        let Ok(_lock) = lock_inflight_state_path(&path) else {
            continue;
        };
        let Some(state) = read_inflight_state_content(&path) else {
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            continue;
        }
        if state.restart_mode.is_some() {
            continue;
        }
        if state.rebind_origin {
            continue;
        }
        match state.restart_generation {
            None => continue,
            Some(row_generation) if row_generation == current_generation => continue,
            Some(_) => {}
        }
        log_inflight_remove(
            provider,
            state.channel_id,
            state.user_msg_id,
            "invalidate_stale_generation_boot",
            &path,
        );
        if fs::remove_file(&path).is_ok() {
            // Only emit observability when called via the env wrapper —
            // raw `_in_root` calls are unit tests and we want to keep
            // them deterministic.
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                state.channel_id,
                state.dispatch_id.as_deref(),
                None,
                None,
                "evict_stale_generation",
                serde_json::json!({
                    "reason": "generation_mismatch_boot",
                    "row_generation": state.restart_generation,
                    "current_generation": current_generation,
                    "user_msg_id": state.user_msg_id,
                }),
            );
            removed.push((state.channel_id, state.restart_generation));
        }
    }
    removed
}

fn planned_restart_retention_secs(restart_mode: InflightRestartMode) -> u64 {
    match restart_mode {
        InflightRestartMode::DrainRestart => DRAIN_RESTART_MAX_AGE_SECS,
        InflightRestartMode::HotSwapHandoff => HOT_SWAP_HANDOFF_MAX_AGE_SECS,
    }
}

/// Thread-local test seam for `tmux_pane_alive_for_stale_check`. Production
/// always calls `tmux_diagnostics::tmux_session_has_live_pane`; tests inject a
/// known-alive name set via `set_test_tmux_alive_override` so the override
/// behaviour can be exercised without spawning real tmux.
#[cfg(test)]
static TEST_TMUX_ALIVE_OVERRIDE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn set_test_tmux_alive_override(names: Option<&[&str]>) {
    let lock = TEST_TMUX_ALIVE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock.lock().expect("tmux alive override lock poisoned");
    *guard = names.map(|slice| slice.iter().map(|s| (*s).to_string()).collect());
}

fn tmux_pane_alive_for_stale_check(name: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(lock) = TEST_TMUX_ALIVE_OVERRIDE.get()
            && let Ok(guard) = lock.lock()
            && let Some(set) = guard.as_ref()
        {
            return set.contains(name);
        }
    }
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(name)
}

pub(super) fn stale_removal_reason(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> Option<String> {
    match state.restart_mode {
        Some(restart_mode) => {
            // A planned-restart row is intentionally authored by the outgoing
            // process and consumed by its immediate replacement. Therefore an
            // E -> E+1 generation mismatch is the normal handoff shape, not stale
            // evidence. Retention and the live-pane override below bound orphaned
            // markers; successful recovery adoption clears the restart marker.
            let replacement_handoff = state.restart_generation.is_some_and(|generation| {
                generation == current_generation
                    || generation.saturating_add(1) == current_generation
            });
            if !replacement_handoff {
                return Some(format!(
                    "removing {} inflight state outside replacement generation window {:?} (current generation {})",
                    restart_mode.label(),
                    state.restart_generation,
                    current_generation
                ));
            }
            let max_age = planned_restart_retention_secs(restart_mode);
            if age_secs > max_age {
                // Defense-in-depth: when DrainRestart inflight ages past the
                // 30-min retention window, refuse to wipe if the inflight's
                // tmux pane is still alive. Wiping the row strands the live
                // CLI's eventual response — see the 2026-05-26 incident where
                // repeated quick-exits left a codex turn pane alive but its
                // inflight anchor was removed at the 10th boot. Only one
                // probe per stale row, gated by all the cheaper checks above.
                if matches!(restart_mode, InflightRestartMode::DrainRestart)
                    && let Some(name) = state.tmux_session_name.as_deref()
                    && tmux_pane_alive_for_stale_check(name)
                {
                    tracing::info!(
                        "  ⚠ inflight stale-age ({age_secs}s > {max_age}s) overridden — tmux pane '{name}' still alive (channel {})",
                        state.channel_id
                    );
                    return None;
                }
                return Some(format!(
                    "removing stale {} inflight state file ({age_secs}s old > {max_age}s)",
                    restart_mode.label()
                ));
            }
            None
        }
        None => {
            if age_secs > INFLIGHT_MAX_AGE_SECS {
                if let Some(name) = state.tmux_session_name.as_deref()
                    && tmux_pane_alive_for_stale_check(name)
                {
                    tracing::info!(
                        "  ⚠ inflight stale-age ({age_secs}s > {INFLIGHT_MAX_AGE_SECS}s) overridden — tmux pane '{name}' still alive (channel {})",
                        state.channel_id
                    );
                    return None;
                }
                Some(format!(
                    "removing stale inflight state file ({age_secs}s old > {INFLIGHT_MAX_AGE_SECS}s)"
                ))
            } else {
                None
            }
        }
    }
}

fn stale_removal_reason_for_path(
    path: &Path,
    state: &InflightTurnState,
    current_generation: u64,
) -> Option<String> {
    stale_removal_reason(state, inflight_age_secs_for_path(path)?, current_generation)
}

enum LockedInflightRead {
    State(InflightTurnState),
    GoneOrMalformed,
    Unreadable,
}

fn read_inflight_state_for_probe_under_lock(path: &Path) -> LockedInflightRead {
    match fs::read_to_string(path) {
        Ok(content) => parse_inflight_state_content(&content)
            .map(LockedInflightRead::State)
            .unwrap_or(LockedInflightRead::GoneOrMalformed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            LockedInflightRead::GoneOrMalformed
        }
        Err(_) => LockedInflightRead::Unreadable,
    }
}

pub(in crate::services::discord) struct InflightProbeLoad {
    pub(in crate::services::discord) states: Vec<InflightTurnState>,
    pub(in crate::services::discord) complete: bool,
}

pub(super) fn load_inflight_states_from_root(
    root: &Path,
    provider: &ProviderKind,
) -> Vec<InflightTurnState> {
    load_inflight_states_for_probe_from_root(root, provider).states
}

pub(in crate::services::discord) fn load_inflight_states_for_probe_from_root(
    root: &Path,
    provider: &ProviderKind,
) -> InflightProbeLoad {
    let dir = inflight_provider_dir(root, provider);
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            return InflightProbeLoad {
                states: Vec::new(),
                complete: error.kind() == std::io::ErrorKind::NotFound,
            };
        }
    };
    let mut states = Vec::new();
    let mut complete = true;
    let mut tmux_owners: HashMap<String, u64> = HashMap::new();
    let allocation = crate::services::discord::runtime_store::process_generation_binding();
    let current_generation = allocation.generation;
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => {
                complete = false;
                continue;
            }
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            complete = false;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read inflight state file: {}",
                path.display()
            );
            continue;
        };
        let (mut state, mut finalizer_backfilled) =
            match parse_inflight_state_content_with_finalizer_backfill(&content) {
                Ok(parsed) => parsed,
                Err(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ removing malformed inflight state file: {}",
                        path.display()
                    );
                    let Ok(_lock) = lock_inflight_state_path(&path) else {
                        complete = false;
                        continue;
                    };
                    match fs::read_to_string(&path) {
                        Ok(locked_content) => match parse_inflight_state_content(&locked_content) {
                            Ok(locked_state) => (locked_state, false),
                            Err(_) => {
                                log_loader_inflight_remove(
                                    provider,
                                    channel_id_from_path(&path),
                                    user_msg_id_for_inflight_remove_log(&path),
                                    "load_inflight_states_from_root_malformed",
                                    &path,
                                    None,
                                    current_generation,
                                );
                                let _ = fs::remove_file(&path);
                                continue;
                            }
                        },
                        Err(error) => {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                complete = false;
                            }
                            log_loader_inflight_remove(
                                provider,
                                channel_id_from_path(&path),
                                user_msg_id_for_inflight_remove_log(&path),
                                "load_inflight_states_from_root_malformed",
                                &path,
                                None,
                                current_generation,
                            );
                            let _ = fs::remove_file(&path);
                            continue;
                        }
                    }
                }
            };
        if state.provider_kind().as_ref() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing inflight state with provider mismatch: {}",
                path.display()
            );
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                complete = false;
                continue;
            };
            let locked_state = match read_inflight_state_for_probe_under_lock(&path) {
                LockedInflightRead::State(state) => state,
                read => {
                    complete &= !matches!(read, LockedInflightRead::Unreadable);
                    log_loader_inflight_remove(
                        provider,
                        channel_id_from_path(&path),
                        user_msg_id_for_inflight_remove_log(&path),
                        "load_inflight_states_from_root_provider_mismatch",
                        &path,
                        None,
                        current_generation,
                    );
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                log_loader_inflight_remove(
                    provider,
                    locked_state.channel_id,
                    locked_state.user_msg_id,
                    "load_inflight_states_from_root_provider_mismatch",
                    &path,
                    Some(&locked_state),
                    current_generation,
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            finalizer_backfilled = false;
            state = locked_state;
        }
        if stale_removal_reason_for_path(&path, &state, current_generation).is_some() {
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                complete = false;
                continue;
            };
            let locked_state = match read_inflight_state_for_probe_under_lock(&path) {
                LockedInflightRead::State(state) => state,
                read => {
                    complete &= !matches!(read, LockedInflightRead::Unreadable);
                    log_loader_inflight_remove(
                        provider,
                        channel_id_from_path(&path),
                        user_msg_id_for_inflight_remove_log(&path),
                        "load_inflight_states_from_root_stale",
                        &path,
                        None,
                        current_generation,
                    );
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                log_loader_inflight_remove(
                    provider,
                    locked_state.channel_id,
                    locked_state.user_msg_id,
                    "load_inflight_states_from_root_stale_provider_mismatch",
                    &path,
                    Some(&locked_state),
                    current_generation,
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            if let Some(reason) =
                stale_removal_reason_for_path(&path, &locked_state, current_generation)
            {
                if loader_gate_refuses_with_allocation(&locked_state, allocation) {
                    record_loader_generation_gate(&locked_state, allocation, &path);
                } else {
                    emit_loader_generation_gate_allowed(provider, &locked_state, allocation, &path);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] ⚠ {}: {}", reason, path.display());
                    log_loader_inflight_remove(
                        provider,
                        locked_state.channel_id,
                        locked_state.user_msg_id,
                        "load_inflight_states_from_root_stale",
                        &path,
                        Some(&locked_state),
                        current_generation,
                    );
                    let _ = fs::remove_file(&path);
                    continue;
                }
            }
            finalizer_backfilled = false;
            state = locked_state;
        }
        if finalizer_backfilled {
            if let Some(locked_state) = backfill_finalizer_turn_id_under_lock(root, &path, provider)
            {
                state = locked_state;
            } else {
                complete = false;
            }
        }
        if let Some(tmux_session_name) = state
            .tmux_session_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            if let Some(previous_channel_id) =
                tmux_owners.insert(tmux_session_name.to_string(), state.channel_id)
            {
                record_inflight_invariant(
                    false,
                    &state,
                    "inflight_tmux_one_to_one",
                    "src/services/discord/inflight.rs:load_inflight_states_from_root",
                    "one tmux session must not be owned by multiple inflight channel files",
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "previous_channel_id": previous_channel_id,
                        "current_channel_id": state.channel_id,
                        "path": path.display().to_string(),
                    }),
                );
            }
        }
        states.push(state);
    }
    InflightProbeLoad { states, complete }
}

#[cfg(test)]
mod loader_gate_observation_tests {
    use super::*;

    fn row(channel_id: u64, born_generation: u64, tmux: Option<&str>) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-claude".to_string()),
            7,
            8,
            9,
            "loader gate row".to_string(),
            Some("session-5462".to_string()),
            tmux.map(str::to_string),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.born_generation = born_generation;
        state
    }

    #[rustfmt::skip]
    fn allocation(generation: u64, route: crate::services::discord::runtime_store::GenerationAllocationRoute) -> crate::services::discord::runtime_store::ProcessGenerationAllocation {
        crate::services::discord::runtime_store::process_generation_allocation_for_tests(generation, route)
    }

    #[rustfmt::skip]
    fn route_cases() -> [(crate::services::discord::runtime_store::GenerationAllocationRoute, u64, u64, &'static str, bool); 8] {
        use crate::services::discord::runtime_store::GenerationAllocationRoute as R;
        [
            (R::AdvancedWithSyncedRename, 5462, 5461, "advanced_with_synced_rename", true),
            (R::ParentSyncFailed, 5462, 5461, "parent_sync_failed", false),
            (R::CounterReadFailed, 1, 0, "counter_read_failed", false),
            (R::Saturated, u64::MAX, u64::MAX - 1, "saturated", false),
            (R::WriteFailed, 5461, 5460, "write_failed", false),
            (R::LockFailed, 5461, 5460, "lock_failed", false),
            (R::PathUnavailable, 0, 5461, "path_unavailable", false),
            (R::Unwitnessed, 5461, 5460, "unwitnessed", false),
        ]
    }

    fn emitted_allow_payload(channel_id: u64) -> serde_json::Value {
        crate::services::observability::events::recent(usize::MAX)
            .into_iter()
            .rev()
            .find(|event| {
                event.event_type == "inflight_lifecycle"
                    && event.channel_id == Some(channel_id)
                    && event.payload["kind"] == LOADER_GENERATION_GATE_ALLOWED
            })
            .map(|event| event.payload["extra"].clone())
            .expect("loader gate allow event")
    }

    fn emitted_refusal_payload(channel_id: u64) -> serde_json::Value {
        crate::services::observability::events::recent(usize::MAX)
            .into_iter()
            .rev()
            .find(|event| {
                event.event_type == "invariant_violation"
                    && event.channel_id == Some(channel_id)
                    && event.payload["invariant"] == "reconcile_never_clears_current_generation_row"
                    && event.payload["details"]["site"] == "load_inflight_states_from_root_stale"
            })
            .map(|event| event.payload["details"].clone())
            .expect("loader generation gate refusal event")
    }

    fn seed_generation_counter(root: &std::path::Path, generation: u64) {
        let path = root.join("runtime").join("generation");
        std::fs::create_dir_all(path.parent().expect("generation parent")).unwrap();
        std::fs::write(path, generation.to_string()).unwrap();
    }

    struct TmuxAliveOverrideReset;

    impl Drop for TmuxAliveOverrideReset {
        fn drop(&mut self) {
            set_test_tmux_alive_override(None);
        }
    }

    fn seed_stale(root: &std::path::Path, state: &InflightTurnState) -> std::path::PathBuf {
        super::super::save_inflight_state_in_root(root, state).expect("seed loader row");
        let path = inflight_state_path(root, &ProviderKind::Claude, state.channel_id);
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(
                chrono::Utc::now().timestamp() - INFLIGHT_MAX_AGE_SECS as i64 - 2,
                0,
            ),
        )
        .expect("age row past loader threshold");
        path
    }

    // The allow counter is §9-2's denominator, so its fields are the filter that
    // isolates the transfer condition (current generation, no tmux name) — the
    // one combination this fence deliberately does not refuse.
    #[test]
    fn loader_allow_event_carries_the_generation_and_name_filter() {
        let state = row(54_630, 54_61, None);
        emit_loader_generation_gate_allowed(
            &ProviderKind::Claude,
            &state,
            allocation(54_62, crate::services::discord::runtime_store::GenerationAllocationRoute::AdvancedWithSyncedRename),
            Path::new("/tmp/54630.json"),
        );

        let extra = emitted_allow_payload(54_630);
        assert_eq!(extra["site"], "load_inflight_states_from_root_stale");
        assert_eq!(extra["born_generation"], 54_61);
        assert_eq!(extra["current_generation"], 54_62);
        assert_eq!(extra["current_generation_row"], false);
        assert_eq!(extra["tmux_session_name_present"], false);
        assert_eq!(extra["path"], "/tmp/54630.json");
    }

    // §9-2's transfer condition itself: a current-generation row with no tmux
    // name is ALLOWED through, and both halves of that pair must be readable
    // from the event or the condition cannot be counted.
    #[test]
    fn loader_allow_event_marks_the_unnamed_current_generation_row() {
        let state = row(54_631, 54_62, None);
        emit_loader_generation_gate_allowed(
            &ProviderKind::Claude,
            &state,
            allocation(54_62, crate::services::discord::runtime_store::GenerationAllocationRoute::AdvancedWithSyncedRename),
            Path::new("/tmp/54631.json"),
        );

        let extra = emitted_allow_payload(54_631);
        assert_eq!(extra["current_generation_row"], true);
        assert_eq!(extra["tmux_session_name_present"], false);
    }

    // The two allow-side producers publish different schemas, so they must not
    // share a kind: filtering the reconcile fence's `delegated_outcome` would
    // otherwise drop every loader row without saying so.
    #[test]
    fn loader_allow_kind_is_distinct_from_the_reconcile_fence_kind() {
        assert_ne!(
            LOADER_GENERATION_GATE_ALLOWED,
            "reconcile_generation_gate_allowed"
        );
    }

    #[test]
    fn allocation_route_is_observed_but_never_changes_loader_preservation() {
        for (index, (route, generation, stale, expected_route, expected_advanced)) in
            route_cases().into_iter().enumerate()
        {
            let bound = allocation(generation, route);
            let matching = row(61_000 + index as u64 * 2, bound.generation, Some("named"));
            let refuses = bound.generation != 0;
            assert_eq!(
                loader_gate_refuses_with_allocation(&matching, bound),
                refuses
            );
            let details = if refuses {
                record_loader_generation_gate(
                    &matching,
                    bound,
                    Path::new("/tmp/loader-route-proof.json"),
                );
                emitted_refusal_payload(matching.channel_id)
            } else {
                emit_loader_generation_gate_allowed(
                    &ProviderKind::Claude,
                    &matching,
                    bound,
                    Path::new("/tmp/loader-route-proof.json"),
                );
                emitted_allow_payload(matching.channel_id)
            };
            assert_eq!(details["epoch_route"], expected_route);
            assert_eq!(details["epoch_advanced"], expected_advanced);

            let nonmatching = row(61_001 + index as u64 * 2, stale, Some("named"));
            assert!(!loader_gate_refuses_with_allocation(&nonmatching, bound));
            emit_loader_generation_gate_allowed(
                &ProviderKind::Claude,
                &nonmatching,
                bound,
                Path::new("/tmp/loader-route-proof.json"),
            );
            let extra = emitted_allow_payload(nonmatching.channel_id);
            assert_eq!(extra["epoch_route"], expected_route);
            assert_eq!(extra["epoch_advanced"], expected_advanced);
        }
    }

    fn assert_public_loader_pair(
        root: &Path,
        index: usize,
        bound: crate::services::discord::runtime_store::ProcessGenerationAllocation,
        stale: u64,
    ) {
        let _publication =
            crate::services::discord::runtime_store::publish_process_generation_allocation_for_tests(bound);
        for (offset, born, matching) in [(0, bound.generation, true), (1, stale, false)] {
            let state = row(63_000 + index as u64 * 2 + offset, born, Some("named"));
            let path = seed_stale(root, &state);
            let loaded = load_inflight_states_for_probe_from_root(root, &ProviderKind::Claude);
            let refused = matching && bound.generation != 0;
            assert_eq!(path.exists(), refused);
            assert_eq!(
                loaded
                    .states
                    .iter()
                    .any(|row| row.channel_id == state.channel_id),
                refused
            );
            let payload = if refused {
                emitted_refusal_payload(state.channel_id)
            } else {
                emitted_allow_payload(state.channel_id)
            };
            assert_eq!(payload["current_generation"], bound.generation);
            assert_eq!(payload["epoch_route"], bound.epoch_route());
            assert_eq!(payload["epoch_advanced"], bound.epoch_advanced());
            if !refused {
                assert_eq!(payload["current_generation_row"], false);
            }
        }
    }

    #[test]
    fn public_loader_observes_production_shaped_published_routes() {
        let _tmux_lock = super::super::stall_recovery_tests::stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        crate::services::discord::runtime_store::set_process_generation_for_tests(None);
        set_test_tmux_alive_override(Some(&[]));
        let _tmux_override = TmuxAliveOverrideReset;
        for (sync, expected) in [
            (false, "parent_sync_failed"),
            (true, "advanced_with_synced_rename"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                root.path(),
            );
            seed_generation_counter(root.path(), 54_61);
            let _publication =
                crate::services::discord::runtime_store::allocate_and_publish_process_generation_for_tests(sync);
            assert_eq!(
                crate::services::discord::runtime_store::process_generation_binding().epoch_route(),
                expected
            );
        }
        let root = tempfile::tempdir().unwrap();
        let inflight_root = root.path().join("runtime").join("discord_inflight");
        for (index, (route, generation, stale, _, _)) in route_cases().into_iter().enumerate() {
            assert_public_loader_pair(&inflight_root, index, allocation(generation, route), stale);
        }
    }

    /// Lexical tripwire only: aliases or helper extraction can evade it. The
    /// route-complete execution test above is the semantic proof.
    #[test]
    fn observation_labels_and_allocation_only_evaluation_api_are_exact() {
        let source = include_str!("removal.rs");
        let production = source.split_once("#[cfg(test)]").unwrap().0;
        let observation = source
            .split_once("fn loader_gate_refuses_with_allocation(")
            .expect("loader allocation observation must remain present")
            .1
            .split_once("/// Thread-local test seam for `tmux_pane_alive_for_stale_check`.")
            .expect("loader allocation observation must remain bounded")
            .0;
        assert_eq!(observation.matches("\"epoch_route\"").count(), 2);
        assert_eq!(observation.matches("\"epoch_advanced\"").count(), 2);
        assert_eq!(observation.matches("epoch_route,").count(), 2);
        assert_eq!(observation.matches("epoch_advanced,").count(), 2);
        assert_eq!(
            observation.matches("ProcessGenerationAllocation").count(),
            3
        );
        let wrapper = production
            .split_once("fn loader_gate_refuses_with_allocation(")
            .unwrap()
            .1
            .split_once("/// §7.2-1's refusal payload")
            .unwrap()
            .0;
        assert_eq!(
            wrapper.trim(),
            "state: &InflightTurnState,\n    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,\n) -> bool {\n    loader_gate_refuses(state, allocation.generation)\n}"
        );

        let public_loader = source
            .split_once("fn load_inflight_states_for_probe_from_root(")
            .expect("public probe loader must remain present")
            .1
            .split_once(
                "#[cfg(test)]
mod loader_gate_observation_tests",
            )
            .expect("public probe loader must remain bounded by its tests")
            .0;
        assert_eq!(
            public_loader
                .matches("let allocation = crate::services::discord::runtime_store::process_generation_binding();")
                .count(),
            1,
        );
        let stale_decision = public_loader.rsplit_once("            if let Some(reason) =\n                stale_removal_reason_for_path(&path, &locked_state, current_generation)").unwrap().1.split_once("\n        if finalizer_backfilled {").unwrap().0;
        assert_eq!(
            stale_decision,
            "\n            {\n                if loader_gate_refuses_with_allocation(&locked_state, allocation) {\n                    record_loader_generation_gate(&locked_state, allocation, &path);\n                } else {\n                    emit_loader_generation_gate_allowed(provider, &locked_state, allocation, &path);\n                    let ts = chrono::Local::now().format(\"%H:%M:%S\");\n                    tracing::info!(\"  [{ts}] ⚠ {}: {}\", reason, path.display());\n                    log_loader_inflight_remove(\n                        provider,\n                        locked_state.channel_id,\n                        locked_state.user_msg_id,\n                        \"load_inflight_states_from_root_stale\",\n                        &path,\n                        Some(&locked_state),\n                        current_generation,\n                    );\n                    let _ = fs::remove_file(&path);\n                    continue;\n                }\n            }\n            finalizer_backfilled = false;\n            state = locked_state;\n        }"
        );
        assert!(!public_loader.contains("allocation.epoch_route()"));
        assert!(!public_loader.contains("allocation.epoch_advanced()"));

        let public_witness = source
            .split_once("fn public_loader_observes_production_shaped_published_routes()")
            .expect("public loader witness must remain present")
            .1
            .split_once("/// Lexical tripwire only")
            .expect("public loader witness must remain bounded")
            .0;
        assert_eq!(
            public_witness
                .matches("super::super::stall_recovery_tests::stale_override_test_mutex()")
                .count(),
            1,
            "the process-global tmux override witness must share its canonical test mutex",
        );
    }
}
