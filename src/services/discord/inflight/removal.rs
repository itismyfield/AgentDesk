//! Inflight removal, stale-generation invalidation, and the non-destructive loader.

use super::*;
use std::collections::HashMap;
use std::path::Path;

use super::store::InflightStateFileLock;

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
    stale_removal_reason_with_probe(
        state,
        age_secs,
        current_generation,
        &mut tmux_pane_alive_for_stale_check,
    )
}

fn stale_removal_reason_with_probe(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
    pane_alive: &mut dyn FnMut(&str) -> bool,
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
                    && pane_alive(name)
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
                    && pane_alive(name)
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

#[cfg(test)]
type PreLockHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
thread_local! {
    static BEFORE_ROW_LOCK: std::cell::RefCell<Option<PreLockHook>> = std::cell::RefCell::new(None);
}

/// One row's loader verdict. Deciding never writes: every hide verdict and
/// the gate refusal carry the sidecar lock they were decided under, so the boot
/// reaper acts on them without releasing that lock first.
enum RowVerdict {
    /// Returned; the flag marks an unlocked parse that needs a finalizer-id backfill.
    Keep(InflightTurnState, bool),
    /// Returned without backfill: stale, but the generation gate refuses.
    KeepGateRefused(InflightTurnState, InflightStateFileLock),
    /// Hidden stale row: reason, unlocked snapshot, locked re-read.
    HideStale(
        String,
        InflightTurnState,
        InflightTurnState,
        InflightStateFileLock,
    ),
    /// Hidden foreign row: log label, unlocked snapshot, locked re-read.
    HideForeign(
        &'static str,
        InflightTurnState,
        InflightTurnState,
        InflightStateFileLock,
    ),
    /// Hidden unparseable row: unlocked bytes, locked bytes.
    HideMalformed(String, String, InflightStateFileLock),
    /// Nothing to return; `true` when the scan is left incomplete.
    Skip(bool),
}

/// Takes the row's lock and re-reads it; anything but a parsed row is already
/// the verdict.
fn relock(
    path: &Path,
    content: &str,
) -> Result<(InflightTurnState, InflightStateFileLock), Box<RowVerdict>> {
    #[cfg(test)]
    BEFORE_ROW_LOCK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(path);
        }
    });
    let lock = lock_inflight_state_path(path).map_err(|_| Box::new(RowVerdict::Skip(true)))?;
    match fs::read_to_string(path) {
        Ok(locked) => match parse_inflight_state_content(&locked) {
            Ok(state) => Ok((state, lock)),
            Err(_) => Err(Box::new(RowVerdict::HideMalformed(
                content.to_string(),
                locked,
                lock,
            ))),
        },
        Err(error) => Err(Box::new(RowVerdict::Skip(
            error.kind() != std::io::ErrorKind::NotFound,
        ))),
    }
}

fn classify_inflight_row(
    path: &Path,
    provider: &ProviderKind,
    allocation: crate::services::discord::runtime_store::ProcessGenerationAllocation,
) -> RowVerdict {
    let generation = allocation.generation;
    let verdict = || -> Result<RowVerdict, Box<RowVerdict>> {
        let Ok(content) = fs::read_to_string(path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read inflight state file: {}",
                path.display()
            );
            return Ok(RowVerdict::Skip(true));
        };
        let (mut state, mut backfill) =
            match parse_inflight_state_content_with_finalizer_backfill(&content) {
                Ok(parsed) => parsed,
                Err(_) => (relock(path, &content)?.0, false),
            };
        let foreign = |row: &InflightTurnState| row.provider_kind().as_ref() != Some(provider);
        if foreign(&state) {
            let (locked, lock) = relock(path, &content)?;
            if foreign(&locked) {
                let label = "load_inflight_states_from_root_provider_mismatch";
                return Ok(RowVerdict::HideForeign(label, state, locked, lock));
            }
            (state, backfill) = (locked, false);
        }
        if stale_removal_reason_for_path(path, &state, generation).is_none() {
            return Ok(RowVerdict::Keep(state, backfill));
        }
        let (locked, lock) = relock(path, &content)?;
        if foreign(&locked) {
            let label = "load_inflight_states_from_root_stale_provider_mismatch";
            return Ok(RowVerdict::HideForeign(label, state, locked, lock));
        }
        Ok(
            match stale_removal_reason_for_path(path, &locked, generation) {
                Some(_) if loader_gate_refuses_with_allocation(&locked, allocation) => {
                    RowVerdict::KeepGateRefused(locked, lock)
                }
                Some(reason) => RowVerdict::HideStale(reason, state, locked, lock),
                None => RowVerdict::Keep(locked, false),
            },
        )
    };
    verdict().unwrap_or_else(|verdict| *verdict)
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

/// Rows the loader's verdict would retire are hidden, never unlinked or
/// renamed: `boot_reaper` is their only retirement path.
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
        let state = match classify_inflight_row(&path, provider, allocation) {
            RowVerdict::Keep(state, true) => {
                let backfilled = backfill_finalizer_turn_id_under_lock(root, &path, provider);
                complete &= backfilled.is_some();
                backfilled.unwrap_or(state)
            }
            RowVerdict::Keep(state, false) | RowVerdict::KeepGateRefused(state, _) => state,
            RowVerdict::Skip(incomplete) => {
                complete &= !incomplete;
                continue;
            }
            RowVerdict::HideStale(..)
            | RowVerdict::HideForeign(..)
            | RowVerdict::HideMalformed(..) => {
                continue;
            }
        };
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

mod boot_reaper;
pub(in crate::services::discord) use boot_reaper::reap_inflight_rows_at_boot_blocking;
#[cfg(test)]
use boot_reaper::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum CollisionVerdict {
    Keep,
    KeepGateRefused,
    HiddenStale,
    /// Aged past the threshold; only a tmux pane probe could decide it.
    AgedNamedUnprobed,
    HiddenProviderMismatch,
    HiddenMalformed,
    Missing,
    Unreadable,
}

/// What a create-new turn start collided with, for measuring turns that run
/// without a durable row. Takes no lock, writes nothing and never probes tmux.
pub(in crate::services::discord) fn observe_inflight_create_collision(
    provider: &ProviderKind,
    channel_id: u64,
) -> (CollisionVerdict, serde_json::Value) {
    let allocation = crate::services::discord::runtime_store::process_generation_binding();
    let path = inflight_runtime_root().map(|root| inflight_state_path(&root, provider, channel_id));
    let content = path.as_deref().map(fs::read_to_string);
    let mtime_age_secs = path.as_deref().and_then(inflight_age_secs_for_path);
    let state = match &content {
        Some(Ok(content)) => parse_inflight_state_content(content).ok(),
        _ => None,
    };
    let mut probe_consulted = false;
    let verdict = match (&content, &state) {
        (Some(Err(error)), _) if error.kind() == std::io::ErrorKind::NotFound => {
            CollisionVerdict::Missing
        }
        (None | Some(Err(_)), _) => CollisionVerdict::Unreadable,
        (_, None) => CollisionVerdict::HiddenMalformed,
        (_, Some(row)) if row.provider_kind().as_ref() != Some(provider) => {
            CollisionVerdict::HiddenProviderMismatch
        }
        (_, Some(row)) => match mtime_age_secs.and_then(|age| {
            stale_removal_reason_with_probe(row, age, allocation.generation, &mut |_| {
                probe_consulted = true;
                false
            })
        }) {
            None => CollisionVerdict::Keep,
            Some(_) if loader_gate_refuses_with_allocation(row, allocation) => {
                CollisionVerdict::KeepGateRefused
            }
            Some(_) if probe_consulted => CollisionVerdict::AgedNamedUnprobed,
            Some(_) => CollisionVerdict::HiddenStale,
        },
    };
    let row = state.as_ref();
    let detail = serde_json::json!({
        "verdict": verdict,
        "mtime_age_secs": mtime_age_secs,
        "updated_at_age_secs": row
            .and_then(|row| parse_updated_at_unix(&row.updated_at))
            .map(|at| now_unix() - at),
        "existing_user_msg_id": row.map(|row| row.user_msg_id),
        "existing_turn_nonce_present": row.is_some_and(|row| row.turn_nonce.is_some()),
        "existing_born_generation": row.map(|row| row.born_generation),
        "current_generation": allocation.generation,
        "existing_restart_mode": row.and_then(|row| row.restart_mode).map(InflightRestartMode::label),
        "existing_tmux_named": row.is_some_and(|row| row.tmux_session_name.is_some()),
    });
    (verdict, detail)
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
    fn route_cases() -> [(crate::services::discord::runtime_store::GenerationAllocationRoute, u64, u64, &'static str, bool); 9] {
        use crate::services::discord::runtime_store::GenerationAllocationRoute as R;
        [
            (R::AdvancedWithSyncedRename, 5462, 5461, "advanced_with_synced_rename", true),
            (R::AdvancedWithUnflushedRename, 5462, 5461, "advanced_with_unflushed_rename", false),
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
            reap_inflight_rows_at_boot_in_root(root, &ProviderKind::Claude);
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
        use crate::services::discord::runtime_store::{
            ADVANCED_ROUTE_FOR_TESTS, GenerationAllocationRoute as R,
        };
        for (sync, expected) in [
            (false, R::ParentSyncFailed),
            (true, ADVANCED_ROUTE_FOR_TESTS),
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
                crate::services::discord::runtime_store::process_generation_binding(),
                allocation(54_62, expected)
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

        // Reads decide without side effects; only the reaper retires, and only
        // after its lock-held revalidation.
        let owned = source
            .split_once("\nmod loader_gate_observation_tests")
            .unwrap()
            .0;
        let reads = owned.split_once("fn classify_inflight_row(").unwrap().1;
        let reads = reads.split_once("\nmod boot_reaper;").unwrap().0;
        let effects = [
            "fs::remove_file",
            "fs::rename",
            "fs::write",
            "atomic_write",
            "persist_under_lock",
        ];
        let events = [
            "record_loader_generation_gate",
            "emit_loader_generation_gate_allowed",
        ];
        let observe = owned
            .split_once("fn observe_inflight_create_collision(")
            .unwrap()
            .1;
        for effect in effects.iter().chain(&events) {
            assert!(
                !reads.contains(effect) && !observe.contains(effect),
                "{effect}"
            );
        }
        assert!(!observe.contains("lock") && !observe.contains("emit_inflight_lifecycle_event"));
        assert_eq!(
            reads
                .matches("runtime_store::process_generation_binding();")
                .count(),
            1
        );
        assert!(
            !reads.contains("allocation.epoch_route()")
                && !reads.contains("allocation.epoch_advanced()")
        );
        let reaper = include_str!("removal/boot_reaper.rs");
        let reaper = reaper
            .split_once("fn reap_inflight_rows_at_boot_in_root(")
            .unwrap()
            .1;
        let arms: Vec<_> = reaper.split("RowVerdict::").collect();
        for (arm, check) in [
            ("HideStale(", "same_snapshot(&unlocked, &locked)"),
            ("HideForeign(", "same_snapshot(&unlocked, &locked)"),
            ("HideMalformed(", "unlocked == locked"),
        ] {
            assert!(
                arms.iter()
                    .any(|arm_text| arm_text.starts_with(arm) && arm_text.contains(check)),
                "{arm}"
            );
        }
        let marks = [
            "if !unchanged {",
            "emit_loader_generation_gate_allowed(",
            "fs::remove_file(&path)",
        ];
        assert!(marks.map(|mark| reaper.find(mark).expect(mark)).is_sorted());
        assert_eq!(
            owned.matches("fs::remove_file(").count() + reaper.matches("fs::remove_file(").count(),
            2
        );

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

#[cfg(test)]
mod nondestructive_loader_tests {
    use super::*;
    use std::path::PathBuf;

    const G: u64 = 59_960;
    const STALE: i64 = INFLIGHT_MAX_AGE_SECS as i64 + 1;
    const CLAUDE: ProviderKind = ProviderKind::Claude;

    /// Pinned generation, no live tmux pane, rows under `AGENTDESK_ROOT_DIR`.
    struct Env {
        root: tempfile::TempDir,
        _root_env: crate::config::TestEnvVarGuard,
        _env_lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
        _tmux: std::sync::MutexGuard<'static, ()>,
    }

    impl Env {
        fn new() -> Self {
            let tmux = super::super::stall_recovery_tests::stale_override_test_mutex()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
            let root = tempfile::tempdir().unwrap();
            let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                root.path(),
            );
            crate::services::discord::runtime_store::set_process_generation_for_tests(Some(G));
            set_test_tmux_alive_override(Some(&[]));
            Self {
                root,
                _root_env,
                _env_lock: env_lock,
                _tmux: tmux,
            }
        }

        fn dir(&self) -> PathBuf {
            self.root.path().join("runtime").join("discord_inflight")
        }

        /// Writes `state` verbatim into the Claude directory, `age` seconds old.
        fn seed(&self, state: &InflightTurnState, age: i64) -> PathBuf {
            let path = inflight_state_path(&self.dir(), &CLAUDE, state.channel_id);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            rewrite(&path, state, age);
            path
        }

        fn reap(&self) -> BootReapReport {
            reap_inflight_rows_at_boot_in_root(&self.dir(), &CLAUDE)
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            set_test_tmux_alive_override(None);
            crate::services::discord::runtime_store::set_process_generation_for_tests(None);
        }
    }

    fn row(channel_id: u64, tmux: Option<&str>) -> InflightTurnState {
        let (owner, msg, text) = (42, channel_id + 1, "prompt".to_string());
        let (tmux, out) = (tmux.map(str::to_string), Some("/tmp/out.jsonl".to_string()));
        let mut state = InflightTurnState::new(
            CLAUDE, channel_id, None, owner, msg, msg, text, None, tmux, out, None, 0,
        );
        state.born_generation = G - 1;
        state
    }

    fn rewrite(path: &Path, state: &InflightTurnState, age: i64) {
        fs::write(path, serde_json::to_string_pretty(state).unwrap()).unwrap();
        let mtime = filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() - age, 0);
        filetime::set_file_mtime(path, mtime).unwrap();
    }

    fn snapshot(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
        (
            fs::read(path).unwrap(),
            fs::metadata(path).unwrap().modified().unwrap(),
        )
    }

    /// The shapes the loader verdict retires: stale prior-generation, malformed,
    /// and another provider's row filed in this directory.
    fn seed_retirable(env: &Env) -> [PathBuf; 3] {
        let malformed = inflight_state_path(&env.dir(), &CLAUDE, 999);
        let mut foreign = row(5_996_003, None);
        foreign.provider = ProviderKind::Codex.as_str().to_string();
        let [stale, foreign] = [
            env.seed(&row(5_996_001, None), STALE),
            env.seed(&foreign, 0),
        ];
        fs::write(&malformed, "{ malformed json ]").unwrap();
        [stale, malformed, foreign]
    }

    /// Runs `action` once, the first time a verdict is about to take a lock.
    fn with_pre_lock_hook<R>(action: impl FnOnce(&Path) + 'static, run: impl FnOnce() -> R) -> R {
        let mut action = Some(action);
        let hook = move |path: &Path| {
            if let Some(action) = action.take() {
                action(path);
            }
        };
        BEFORE_ROW_LOCK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        let out = run();
        BEFORE_ROW_LOCK.with(|slot| *slot.borrow_mut() = None);
        out
    }

    // R1-R4: every production read wrapper hides the rows it used to unlink.
    #[test]
    fn production_readers_hide_but_never_unlink_retirable_rows() {
        let env = Env::new();
        let [stale, malformed, foreign] = seed_retirable(&env);
        let named = env.seed(&row(5_996_004, Some("AgentDesk-claude-r4")), STALE);
        let paths = [stale, malformed, foreign, named];
        let before = paths.each_ref().map(|path| snapshot(path));

        assert!(super::super::load_inflight_states(&CLAUDE).is_empty());
        assert_eq!(
            super::super::latest_request_owner_user_id_for_channel(5_996_001),
            None
        );
        assert!(!crate::services::discord::has_fresh_inflight_for_channel(
            5_996_001
        ));
        use crate::services::discord::zombie_foreground_release as zombie;
        let lookup = zombie::inflight_episode_lookup_for_tmux_name(&CLAUDE, "AgentDesk-claude-r4");
        assert_eq!(lookup, zombie::InflightEpisodeLookup::Unclaimed);
        assert_eq!(paths.each_ref().map(|path| snapshot(path)), before);
    }

    // R6 + M1: shutdown marking skips a hidden stale row without refreshing it,
    // and the next boot's reaper retires it.
    #[test]
    fn restart_marking_leaves_hidden_rows_to_the_next_boot_reaper() {
        let env = Env::new();
        let stale = env.seed(&row(5_996_011, None), STALE);
        env.seed(&row(5_996_012, None), 0);
        let before = snapshot(&stale);
        let mode = InflightRestartMode::DrainRestart;
        assert_eq!(
            super::super::mark_all_inflight_states_restart_mode_checked(&CLAUDE, mode),
            Ok(1)
        );
        assert_eq!(snapshot(&stale), before);

        crate::services::discord::runtime_store::set_process_generation_for_tests(Some(G + 1));
        let report = env.reap();
        assert_eq!((report.reaped_stale, report.kept), (1, 1), "{report:?}");
        assert!(!stale.exists());
    }

    // B1-B3: backfill is written only to a row whose unlocked verdict was keep,
    // so a read never refreshes the mtime a hidden or refused row is aged by.
    #[test]
    fn loader_backfills_only_rows_it_keeps_on_the_unlocked_verdict() {
        let env = Env::new();
        let mut refused = row(5_996_021, Some("AgentDesk-claude-b2"));
        refused.born_generation = G;
        let [mut hidden, mut fresh] = [row(5_996_022, None), row(5_996_023, None)];
        for state in [&mut refused, &mut hidden, &mut fresh] {
            state.finalizer_turn_id = 0;
        }
        let kept_legacy = [env.seed(&refused, STALE), env.seed(&hidden, STALE)];
        let fresh_path = env.seed(&fresh, 0);
        let before = kept_legacy.each_ref().map(|path| snapshot(path));

        assert_eq!(super::super::load_inflight_states(&CLAUDE).len(), 2);
        assert_eq!(kept_legacy.each_ref().map(|path| snapshot(path)), before);
        let on_disk: InflightTurnState =
            serde_json::from_slice(&fs::read(fresh_path).unwrap()).unwrap();
        assert_ne!(
            on_disk.finalizer_turn_id, 0,
            "a kept legacy row is backfilled"
        );
    }

    // M3's route table covers the gate refusal; this covers the other verdicts.
    #[test]
    fn boot_reaper_retires_foreign_malformed_and_out_of_window_rows() {
        let env = Env::new();
        let [stale, malformed, foreign] = seed_retirable(&env);
        let drains = [(5_996_031, G - 2), (5_996_032, G - 1)].map(|(channel_id, generation)| {
            let mut state = row(channel_id, None);
            state.set_restart_mode(InflightRestartMode::DrainRestart);
            state.restart_generation = Some(generation);
            env.seed(&state, STALE)
        });
        let report = env.reap();
        let reaped = (
            report.reaped_stale,
            report.reaped_provider_mismatch,
            report.reaped_malformed,
        );
        assert_eq!((reaped, report.kept), ((2, 1, 1), 1), "{report:?}");
        assert!(
            [&stale, &malformed, &foreign, &drains[0]]
                .iter()
                .all(|path| !path.exists())
        );
        assert!(drains[1].exists(), "a drain row inside its window survives");
        assert!(
            malformed.with_extension("json.lock").exists(),
            "lock sidecars are never reaped"
        );
    }

    // X1-X5: whatever replaces, refreshes, advances, clears or repairs the row
    // between the unlocked verdict and the lock stops the unlink.
    #[test]
    fn boot_reaper_revalidates_the_snapshot_under_the_lock() {
        let env = Env::new();
        let a = row(5_996_041, None);
        let mut swapped = row(5_996_041, None);
        (swapped.user_msg_id, swapped.turn_nonce) = (7, Some("successor".to_string()));
        let mut progressed = a.clone();
        (progressed.save_generation, progressed.updated_at) =
            (9, "2099-01-01 00:00:00".to_string());
        type Case = (
            &'static str,
            Box<dyn FnOnce(&Path)>,
            fn(&BootReapReport) -> usize,
            bool,
        );
        let cases: [Case; 4] = [
            (
                "X1",
                Box::new(move |p| rewrite(p, &swapped, STALE)),
                |r| r.changed,
                true,
            ),
            (
                "X2",
                Box::new({
                    let a = a.clone();
                    move |p| rewrite(p, &a, 0)
                }),
                |r| r.kept,
                true,
            ),
            (
                "X3",
                Box::new(move |p| rewrite(p, &progressed, STALE)),
                |r| r.changed,
                true,
            ),
            (
                "X4",
                Box::new(|p| fs::remove_file(p).unwrap()),
                |r| r.missing,
                false,
            ),
        ];
        for (label, action, counter, survives) in cases {
            let path = env.seed(&a, STALE);
            let report = with_pre_lock_hook(action, || env.reap());
            assert_eq!(
                (counter(&report), report.reaped_stale),
                (1, 0),
                "{label}: {report:?}"
            );
            assert_eq!(path.exists(), survives, "{label}");
        }
        let torn = inflight_state_path(&env.dir(), &CLAUDE, 5_996_042);
        fs::write(&torn, "{ torn").unwrap();
        let repaired = row(5_996_042, None);
        let report = with_pre_lock_hook(move |p| rewrite(p, &repaired, 0), || env.reap());
        assert_eq!(
            (report.kept, report.reaped_malformed),
            (1, 0),
            "X5: {report:?}"
        );
    }

    // X6: a writer holding the lock across the swap is waited for, then honoured.
    #[test]
    fn boot_reaper_waits_for_a_lock_holder_then_keeps_its_row() {
        let env = Env::new();
        let path = env.seed(&row(5_996_051, None), STALE);
        let mut successor = row(5_996_051, None);
        successor.user_msg_id = 7;
        let writer = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (hook_writer, hook_path) = (writer.clone(), path.clone());
        let hook = move |_: &Path| {
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            *hook_writer.borrow_mut() = Some(std::thread::spawn(move || {
                let _lock = lock_inflight_state_path(&hook_path).unwrap();
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(100));
                rewrite(&hook_path, &successor, STALE);
            }));
            held_rx.recv().unwrap();
        };
        let report = with_pre_lock_hook(hook, || env.reap());
        writer.borrow_mut().take().unwrap().join().unwrap();
        assert_eq!((report.changed, report.reaped_stale), (1, 0), "{report:?}");
        assert_eq!(read_inflight_state_content(&path).unwrap().user_msg_id, 7);
    }

    // S1 + S2: a row refreshed while a read waited on its lock is returned and
    // marked, as the in-read revalidation always did.
    #[test]
    fn reads_return_and_mark_a_row_refreshed_before_the_lock() {
        let env = Env::new();
        let mut fresh = row(5_996_061, None);
        let path = env.seed(&fresh, STALE);
        fresh.save_generation += 1;
        let refresh = |fresh: InflightTurnState| move |p: &Path| rewrite(p, &fresh, 0);
        let loaded = with_pre_lock_hook(refresh(fresh.clone()), || {
            super::super::load_inflight_states(&CLAUDE)
        });
        assert_eq!(
            loaded.iter().map(|s| s.save_generation).collect::<Vec<_>>(),
            [fresh.save_generation]
        );

        rewrite(&path, &fresh, STALE);
        let mode = InflightRestartMode::DrainRestart;
        let mark = || super::super::mark_all_inflight_states_restart_mode_checked(&CLAUDE, mode);
        assert_eq!(with_pre_lock_hook(refresh(fresh), mark), Ok(1));
        assert!(
            read_inflight_state_content(&path)
                .unwrap()
                .restart_mode
                .is_some()
        );
    }

    // C1 + C2: a hidden row blocks create-new, and the observation classifies
    // it without a lock, a write or a tmux probe.
    #[test]
    fn create_collision_is_observed_without_touching_the_row() {
        let env = Env::new();
        let hidden = row(5_996_071, None);
        let path = env.seed(&hidden, STALE);
        let before = snapshot(&path);
        let created = super::super::save_inflight_state_create_new(&row(5_996_071, None));
        assert!(matches!(
            created,
            Err(CreateNewInflightError::AlreadyExists)
        ));
        let (verdict, detail) = observe_inflight_create_collision(&CLAUDE, 5_996_071);
        assert_eq!(verdict, CollisionVerdict::HiddenStale);
        assert!(
            detail["mtime_age_secs"]
                .as_i64()
                .is_some_and(|age| age >= STALE)
        );
        assert_eq!(detail["existing_user_msg_id"], hidden.user_msg_id);
        assert_eq!(snapshot(&path), before);

        set_test_tmux_alive_override(Some(&["AgentDesk-claude-c2"]));
        let named = env.seed(&row(5_996_072, Some("AgentDesk-claude-c2")), STALE);
        fs::write(inflight_state_path(&env.dir(), &CLAUDE, 5_996_073), "{").unwrap();
        env.seed(&row(5_996_074, None), 0);
        for (channel_id, expected) in [
            (5_996_072, CollisionVerdict::AgedNamedUnprobed),
            (5_996_073, CollisionVerdict::HiddenMalformed),
            (5_996_074, CollisionVerdict::Keep),
            (5_996_075, CollisionVerdict::Missing),
        ] {
            assert_eq!(
                observe_inflight_create_collision(&CLAUDE, channel_id).0,
                expected
            );
        }
        assert!(
            !named.with_extension("json.lock").exists(),
            "the observation never locks"
        );
    }

    // W3: one pass per provider; a later caller waits for it and runs nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boot_reap_once_runs_once_per_provider_and_later_callers_wait() {
        let guard = std::sync::Arc::new(BootReapOnce::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let kept = |kept| BootReapReport {
            kept,
            ..BootReapReport::default()
        };
        let first_reap = move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            kept(7)
        };
        let first = tokio::spawn({
            let guard = guard.clone();
            async move { guard.run_once(&CLAUDE, first_reap).await }
        });
        started_rx.await.unwrap();
        let second = tokio::spawn({
            let guard = guard.clone();
            async move {
                guard
                    .run_once(&CLAUDE, || -> BootReapReport { panic!("second pass") })
                    .await
            }
        });
        let other = guard.run_once(&ProviderKind::Codex, move || kept(1)).await;
        assert_eq!((other.already_ran, other.kept), (false, 1));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !second.is_finished(),
            "a later caller must wait for the first pass"
        );
        release_tx.send(()).unwrap();
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        assert_eq!(
            [
                (first.already_ran, first.kept),
                (second.already_ran, second.kept)
            ],
            [(false, 7), (true, 7)]
        );
    }

    // W4: a cancelled first caller does not reopen the gate; the next caller
    // waits for the pass already running instead of starting a second one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boot_reap_once_survives_a_cancelled_first_caller() {
        let guard = std::sync::Arc::new(BootReapOnce::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let first = tokio::spawn({
            let guard = guard.clone();
            async move {
                let reap = move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    BootReapReport {
                        kept: 7,
                        ..BootReapReport::default()
                    }
                };
                guard.run_once(&CLAUDE, reap).await
            }
        });
        started_rx.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let second = tokio::spawn({
            let guard = guard.clone();
            async move {
                guard
                    .run_once(&CLAUDE, || -> BootReapReport { panic!("second pass") })
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!second.is_finished(), "the running pass must be awaited");
        release_tx.send(()).unwrap();
        let second = second.await.unwrap();
        assert_eq!((second.already_ran, second.kept), (true, 7));
    }

    // The env-root wrapper reaches `_in_root` through the guard, once.
    #[tokio::test]
    async fn boot_reaper_wrapper_reaps_the_env_root_once() {
        let env = Env::new();
        let path = env.seed(&row(5_996_081, None), STALE);
        let guard = BootReapOnce::default();
        let first = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE).await;
        assert_eq!(
            (first.already_ran, first.reaped_stale, path.exists()),
            (false, 1, false)
        );
        env.seed(&row(5_996_081, None), STALE);
        let second = reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE).await;
        assert_eq!((second.already_ran, path.exists()), (true, true));
    }
}
