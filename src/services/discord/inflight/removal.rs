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
                    tracing::warn!(
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
                    tracing::warn!(
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
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = modified.elapsed().ok()?;
    stale_removal_reason(state, age.as_secs(), current_generation)
}

pub(super) fn load_inflight_states_from_root(
    root: &Path,
    provider: &ProviderKind,
) -> Vec<InflightTurnState> {
    let dir = inflight_provider_dir(root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut states = Vec::new();
    let mut tmux_owners: HashMap<String, u64> = HashMap::new();
    let current_generation = crate::services::discord::runtime_store::process_generation();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
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
                        continue;
                    };
                    match read_inflight_state_content(&path) {
                        Some(locked_state) => (locked_state, false),
                        None => {
                            log_inflight_remove_for_path(
                                provider,
                                channel_id_from_path(&path),
                                "load_inflight_states_from_root_malformed",
                                &path,
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
                continue;
            };
            let Some(locked_state) = read_inflight_state_content(&path) else {
                log_inflight_remove_for_path(
                    provider,
                    channel_id_from_path(&path),
                    "load_inflight_states_from_root_provider_mismatch",
                    &path,
                );
                let _ = fs::remove_file(&path);
                continue;
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                log_inflight_remove(
                    provider,
                    locked_state.channel_id,
                    locked_state.user_msg_id,
                    "load_inflight_states_from_root_provider_mismatch",
                    &path,
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            finalizer_backfilled = false;
            state = locked_state;
        }
        if stale_removal_reason_for_path(&path, &state, current_generation).is_some() {
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                continue;
            };
            let Some(locked_state) = read_inflight_state_content(&path) else {
                log_inflight_remove_for_path(
                    provider,
                    channel_id_from_path(&path),
                    "load_inflight_states_from_root_stale",
                    &path,
                );
                let _ = fs::remove_file(&path);
                continue;
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                log_inflight_remove(
                    provider,
                    locked_state.channel_id,
                    locked_state.user_msg_id,
                    "load_inflight_states_from_root_stale_provider_mismatch",
                    &path,
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            if let Some(reason) =
                stale_removal_reason_for_path(&path, &locked_state, current_generation)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ⚠ {}: {}", reason, path.display());
                log_inflight_remove(
                    provider,
                    locked_state.channel_id,
                    locked_state.user_msg_id,
                    "load_inflight_states_from_root_stale",
                    &path,
                );
                let _ = fs::remove_file(&path);
                continue;
            }
            finalizer_backfilled = false;
            state = locked_state;
        }
        if finalizer_backfilled
            && let Some(locked_state) = backfill_finalizer_turn_id_under_lock(root, &path, provider)
        {
            state = locked_state;
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
    states
}
