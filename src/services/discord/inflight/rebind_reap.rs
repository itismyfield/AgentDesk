use super::*;
use crate::services::platform::tmux::PaneLiveness;

/// #3635: runtime-liveness oracle for the dead-watcher rebind-origin reap path.
///
/// A Watcher-owned rebind-origin orphan (born by `recovery_engine` with
/// `relay_owner_kind = Watcher`, #897) can never satisfy
/// [`should_reap_abandoned_rebind_origin`]'s `== None` owner conjunct, so the
/// strict predicate leaves it on disk forever even after the watcher process
/// has died (#3635). It must NOT be reaped on shape alone, though: #3154/#3540
/// require a *live* Watcher rebind to survive so the watcher can re-adopt the
/// session across a restart, and an idle-stuck dead-watcher row is shape-
/// identical to a live-but-between-turns one. The only safe discriminator is a
/// *runtime* liveness probe, which is injected here so unit tests can stub it
/// (and so the keystone pinning tests, which run with no real tmux/jsonl, are
/// never evaluated against a naive oracle that would mis-judge their synthetic
/// session as dead).
pub(super) trait WatcherLiveness {
    /// True only when the watcher owning `state` is *provably* dead **or idle-
    /// stuck**: no runtime activity (jsonl / `.generation` / rollout mtime) has
    /// advanced within [`DEAD_WATCHER_PROVEN_DEAD_SECS`].
    ///
    /// #3879: a live tmux pane is NO LONGER an unconditional `false`. An idle
    /// TUI watcher keeps its pane up indefinitely while never adopting its empty
    /// rebind-origin placeholder; treating "pane up" as "alive forever" left the
    /// placeholder LIVE past its deadline (observed 64 min, 32× the deadline),
    /// wedging every new turn. The binding death/idle signal is therefore
    /// runtime-activity quiescence, applied whether the pane reads `Live` or
    /// `DeadOrAbsent`. A watcher producing *recent* activity (genuinely working /
    /// re-adoptable, #897/#3635) still yields `false`, as does an unknown probe
    /// (`ProbeError`) or a missing session name — the conservative, re-adopt-
    /// protecting defaults.
    fn is_proven_dead(&self, state: &InflightTurnState) -> bool;
}

/// #3635: production [`WatcherLiveness`] backed by the same runtime signals the
/// stall-watchdog (#3169) and #3629 stall-liveness already trust:
/// `tmux_diagnostics::tmux_session_has_live_pane` and
/// `dispatched_sessions::latest_runtime_activity_unix_nanos`.
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
        // #3635 (codex review): use the THREE-state pane probe. A transient tmux
        // probe failure (`ProbeError`) is "unknown", NOT "dead" — it must never
        // license reaping a row whose owner might still be alive, so we preserve
        // before even touching the activity stat.
        let pane = crate::services::tmux_diagnostics::tmux_session_pane_liveness(session);
        if pane == PaneLiveness::ProbeError {
            return false; // unknown ⇒ preserve
        }
        // #3879: a `Live` pane no longer short-circuits to "alive". An idle TUI
        // keeps its pane up while never adopting the empty rebind-origin, so the
        // pane state alone cannot tell "working" from "idle-stuck". The binding
        // signal is runtime-activity quiescence: fresh jsonl / `.generation` /
        // rollout writes (a just-restarting or actively-producing watcher, the
        // #897/#3635 re-adoptable case) preserve the row; no write within the
        // window is proven dead/idle and reapable, whatever the pane reads.
        proven_dead_from_signals(pane, watcher_runtime_activity_recent(session))
    }
}

/// #3879: pure proven-dead/idle-stuck decision from the two probed signals,
/// extracted so unit tests can pin every `(pane, activity)` combination without
/// spawning tmux or touching jsonl/`.generation` files.
///
/// Returns `true` (reapable) only on runtime-activity quiescence. A live tmux
/// pane ALONE no longer preserves the row (#3879: an idle TUI pane that never
/// adopts its empty rebind-origin past the deadline wedged the relay for 64
/// min). Only a genuine *working* signal — recent jsonl / `.generation` /
/// rollout write (`activity_recent == true`, the #897/#3635 re-adoptable case) —
/// or an UNKNOWN probe (`ProbeError`) preserves it. `DeadOrAbsent` with recent
/// activity is still preserved: a just-restarting watcher touches `.generation`
/// before its pane re-appears.
pub(super) fn proven_dead_from_signals(pane: PaneLiveness, activity_recent: bool) -> bool {
    match pane {
        // Unknown ⇒ preserve (the production caller short-circuits this before the
        // activity stat; handled here too so the core is total and testable).
        PaneLiveness::ProbeError => false,
        // Live OR DeadOrAbsent: runtime-activity quiescence is the death/idle
        // signal. Recent activity ⇒ working / re-adoptable ⇒ preserve.
        PaneLiveness::Live | PaneLiveness::DeadOrAbsent => !activity_recent,
    }
}

/// #3635: true when the watcher's runtime files (jsonl / `.generation` mtime)
/// advanced within [`DEAD_WATCHER_PROVEN_DEAD_SECS`]. Pure fs stat — no tmux
/// subprocess — so it is safe to call under the inflight sidecar lock. A 0
/// (no resolvable temp file) is "no recent activity".
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

/// #3635: decide whether a *Watcher-owned* abandoned rebind-origin row is safe
/// to reap because its owning watcher is provably dead.
///
/// This is a separate gate that runs alongside (OR'd with)
/// [`should_reap_abandoned_rebind_origin`] at the periodic sweeper call site —
/// NOT at boot (see `invalidate_stale_generation_in_root`). It exists so the
/// `relay_owner_kind == Watcher` orphan that the strict `== None` predicate can
/// never touch is finally reaped once the watcher has demonstrably exited.
///
/// The structural conjunction is byte-identical to
/// [`should_reap_abandoned_rebind_origin`] EXCEPT the owner conjunct flips from
/// `== None` to `== Watcher`, and the reap is additionally gated on
/// `liveness.is_proven_dead(state)`. A watcher producing recent runtime activity
/// (jsonl / `.generation` / rollout writes — the #897/#3635 re-adoptable case)
/// is `is_proven_dead == false`, so this predicate is `false` and the row is
/// preserved; an unknown tmux probe (`ProbeError`) preserves it too. #3879: a
/// merely-`Live` tmux pane no longer preserves on its own — an idle watcher with
/// no recent activity past the deadline IS reaped (see [`is_proven_dead`]).
/// Every non-owner live signal (adoption, streamed bytes, anchor, terminal
/// commit, offset progress, planned restart) still independently blocks the reap
/// via the shared structural conjunction, and the deadline/generation disjunct
/// keeps the re-adopt window open until the deadline elapses.
#[cfg(test)]
pub(super) fn should_reap_dead_watcher_rebind_origin(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
    liveness: &dyn WatcherLiveness,
) -> bool {
    dead_watcher_rebind_structurally_reapable(state, age_secs, current_generation)
        // FINAL gate: only reap when the owning watcher is provably dead. A live
        // watcher (tmux pane up or recent runtime activity) is never reaped; an
        // unknown tmux probe also preserves (see `RuntimeWatcherLiveness`).
        && liveness.is_proven_dead(state)
}

/// #3635: the structural + deadline/generation half of the dead-watcher reap
/// predicate, WITHOUT the liveness probe. Split out so the locked re-validation
/// can re-check these cheap, fs-only conditions under the sidecar lock without
/// re-running a tmux subprocess (codex review ISSUE 2). The structural
/// conjunction is byte-identical to [`should_reap_abandoned_rebind_origin`]
/// EXCEPT the owner conjunct flips from `== None` to `== Watcher` (the #897
/// rebind birth shape the None predicate can never match); every other conjunct
/// keeps all the live-protection signals (adoption / streamed bytes / anchor /
/// terminal / offset progress / planned restart) blocking the reap exactly as
/// before.
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

    // Past-deadline OR stale-generation matches the None-owner predicate's
    // disjunct, so a dead-watcher orphan is not reaped the instant it is born.
    let deadline = state
        .rebind_origin_deadline_secs
        .unwrap_or_else(rebind_origin_deadline_secs_env);
    let past_deadline = age_secs >= deadline;
    let stale_generation = state
        .rebind_origin_birth_generation
        .is_some_and(|birth| birth != current_generation);
    past_deadline || stale_generation
}

/// #3581: best-effort age (seconds) for a rebind-origin row. Prefers the
/// persisted `rebind_origin_created_at_unix` stamp (so the deadline is anchored
/// to the row's actual birth even if the file is later touched); falls back to
/// the file's mtime age for legacy rows that pre-date the stamp. Returns 0 when
/// neither signal is available — in that case only the generation-mismatch
/// disjunct of `should_reap_abandoned_rebind_origin` can fire, which is the
/// conservative outcome.
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

pub(super) fn is_inflight_json_lock_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".json.lock"))
}

pub(super) fn inflight_json_path_for_lock(lock_path: &Path) -> Option<PathBuf> {
    let file_name = lock_path.file_name()?.to_str()?;
    let json_file_name = file_name.strip_suffix(".lock")?;
    Some(lock_path.with_file_name(json_file_name))
}

pub(super) fn metadata_mtime_unix_secs(metadata: &fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// #3835: staleness predicates + orphan-lock / rebind-origin reap helpers moved
// verbatim from the inflight.rs parent so the hot state file stays under its
// frozen production-LoC baseline. Parent-core items (now_unix, inflight_state_path,
// load_inflight_state_unlocked, ...) resolve through the `use super::*` glob above.
// ---------------------------------------------------------------------------

/// #1446 stall-deadlock recovery: an inflight state is treated as "stale"
/// (i.e. the dispatch that wrote it almost certainly already terminated
/// without cleanup) when its persisted `updated_at` has not advanced for
/// this many seconds. THREAD-GUARD uses this exact threshold; the
/// stall-watchdog uses `2x` to stay strictly more conservative than any
/// caller that has already observed the state directly.
///
/// `updated_at` is rewritten on every `save_inflight_state` call but is
/// **not** a true heartbeat — a healthy foreground model/tool call can
/// legitimately go silent for multiple minutes (long Bash, slow LLM
/// stream, large Read).
///
/// History: aligned with `placeholder_sweeper::ABANDON_THRESHOLD_SECS` (then
/// 300s) until #2427 (#2436 / #2437 / #2438) added explicit-signal wires (pane
/// death, heartbeat-gap sweeper, generation-mismatch invalidate, TurnCompleted
/// guard), relaxing the sweeper to a pure safety net (abandon timer 1800s). 300s
/// is retained here because it gates **new**-dispatch THREAD-GUARD + the
/// stall-watchdog (#1446): both want fast recovery once an explicit signal failed
/// to fire (a false-positive cleanup of a live turn is far worse than delay).
pub(in crate::services::discord) const INFLIGHT_STALENESS_THRESHOLD_SECS: u64 = 300;

/// #3581: default deadline (seconds) after which an unadopted, never-progressed
/// `rebind_origin` row is reaped. Far shorter than the placeholder sweeper's
/// 1800s safety net so the wedge-causing orphan drains within a handful of
/// sweep ticks, but comfortably longer than the ~8s TUI-direct adoption
/// backstop window so a legitimate `/api/inflight/rebind` → TUI-adopt handoff
/// is never raced. Overridable via `AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS`.
pub(in crate::services::discord) const REBIND_ORIGIN_DEADLINE_SECS_DEFAULT: u64 = 120;

/// #3641: orphan sidecar lock files are cosmetic after process death because
/// `flock` releases with the fd, but stale paths can accumulate forever once the
/// matching `.json` state row has been cleaned/quarantined. Keep the reap
/// conservative so a fresh lock created just before its `.json` row is written is
/// never touched.
pub(in crate::services::discord) const ORPHAN_LOCK_REAP_MIN_AGE_SECS: i64 = 3600;

/// #3581: floor for the env-overridden rebind-origin deadline. Guards against a
/// pathologically small (or zero) override reaping rows before the adoption
/// backstop window can complete.
const REBIND_ORIGIN_DEADLINE_SECS_MIN: u64 = 30;

/// #3635: minimum quiescence (seconds) of *all* runtime liveness signals before
/// a Watcher-owned rebind-origin row is treated as "proven dead" and made
/// eligible for the dead-watcher reap path. Deliberately far larger than the
/// stall-watchdog's positive-liveness window
/// (`STALL_WATCHDOG_POSITIVE_LIVENESS_SECS` = 120s): a live Watcher that is
/// merely between turns (mailbox idle, no fresh jsonl byte) must NEVER be
/// false-classified as dead and reaped, because #3154/#3540 require a live
/// Watcher rebind to survive so it can re-adopt the session across a restart.
/// A 10-minute conservative floor means a false-negative (a genuinely dead
/// watcher whose reap is merely delayed) is the only failure mode — never a
/// false-positive (a live watcher reaped). The orphan is harmless while it
/// lingers (#3631/PR #3634 classifies it idle), so delay is acceptable.
pub(in crate::services::discord) const DEAD_WATCHER_PROVEN_DEAD_SECS: u64 = 600;

/// #3581: resolve the rebind-origin reap deadline. Reads
/// `AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS`; on absence / parse failure falls
/// back to [`REBIND_ORIGIN_DEADLINE_SECS_DEFAULT`]. Any explicit value is
/// clamped up to [`REBIND_ORIGIN_DEADLINE_SECS_MIN`] so an operator cannot
/// accidentally configure a reap that races the adoption backstop.
pub(in crate::services::discord) fn rebind_origin_deadline_secs_env() -> u64 {
    std::env::var("AGENTDESK_REBIND_ORIGIN_DEADLINE_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|secs| secs.max(REBIND_ORIGIN_DEADLINE_SECS_MIN))
        .unwrap_or(REBIND_ORIGIN_DEADLINE_SECS_DEFAULT)
}

/// #3581: decide whether an abandoned `rebind_origin` inflight row is safe to
/// reap. The predicate is a strict conjunction of "never-progressed,
/// never-adopted, never-owned" signals so that a genuinely-live rebind
/// (MonitorTriggered watcher rebind: `relay_owner_kind = Watcher`) or a row
/// that has started relaying / been adopted is NEVER reaped:
///
///   * `rebind_origin` — only rebind-origin rows are in scope.
///   * `turn_source == ExternalAdopted` — pins the predicate to the
///     `recovery_engine` birth site; the `tmux` MonitorTriggered rebind
///     (`turn_source = MonitorTriggered`, owner Watcher) is excluded twice.
///   * `effective_relay_owner_kind() == None` — no live relay owner (also
///     absorbs the legacy `watcher_owns_live_relay` bool).
///   * `user_msg_id == 0 && current_msg_id == 0` — never adopted / no anchor.
///   * `!terminal_delivery_committed` — not finalised.
///   * `response_sent_offset == 0 && full_response.is_empty()` — nothing was
///     ever streamed to Discord.
///   * `last_offset == turn_start_offset` — the watcher never advanced past
///     the birth offset (NOTE: a fresh rebind row is born with
///     `last_offset == turn_start_offset == file_len`, which can be > 0 — the
///     "no progress" test is offset equality, NOT `last_offset == 0`).
///   * `restart_mode.is_none()` — planned restart / hot-swap rows own their
///     own retention and are never reaped here.
///
/// When every structural conjunct holds, the row is reaped iff it is past its
/// deadline OR it was born in a prior generation. `age_secs` is supplied by the
/// caller (file-mtime age in the sweeper path) so legacy rows with no
/// `rebind_origin_created_at_unix` stamp still age out via mtime.
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
/// (#3561 lifecycle stream). Mirrors the `evict_stale_generation` shape so the
/// two boot-time reap reasons aggregate side by side.
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

/// #3641: boot-time cleanup for orphan `discord_inflight/{provider}/*.json.lock`
/// sidecars whose matching `.json` state file no longer exists. The lock file is
/// never authoritative by itself: live turns are protected by the existence of
/// the `.json` row, and fresh first-acquire locks are protected by the age floor.
pub(in crate::services::discord) fn reap_orphan_inflight_locks_in_root(
    root: &Path,
    now_unix: i64,
) -> usize {
    let mut removed = 0usize;
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let provider_dir = inflight_provider_dir(root, &provider);
        let provider_name = provider.as_str();
        let Ok(lock_entries) = fs::read_dir(&provider_dir) else {
            continue;
        };

        for lock_entry in lock_entries {
            let Ok(lock_entry) = lock_entry else {
                continue;
            };
            let lock_path = lock_entry.path();
            if !is_inflight_json_lock_path(&lock_path) {
                continue;
            }
            let Some(json_path) = inflight_json_path_for_lock(&lock_path) else {
                continue;
            };
            if json_path.exists() {
                continue;
            }

            let Ok(metadata) = lock_entry.metadata() else {
                tracing::warn!(
                    provider = provider_name,
                    path = %lock_path.display(),
                    "#3641 failed to stat inflight orphan-lock candidate"
                );
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Some(mtime_unix) = metadata_mtime_unix_secs(&metadata) else {
                continue;
            };
            let age_secs = now_unix.saturating_sub(mtime_unix).max(0);
            if age_secs < ORPHAN_LOCK_REAP_MIN_AGE_SECS {
                continue;
            }
            if json_path.exists() {
                continue;
            }

            match fs::remove_file(&lock_path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        provider = provider_name,
                        path = %lock_path.display(),
                        error = %error,
                        "#3641 failed to remove orphan inflight lock file"
                    );
                }
            }
        }
    }

    if removed > 0 {
        tracing::info!(
            removed,
            root = %root.display(),
            "#3641 reaped orphan inflight lock file(s)"
        );
    }
    removed
}

pub(in crate::services::discord) fn reap_orphan_inflight_locks() -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    reap_orphan_inflight_locks_in_root(&root, now_unix())
}

/// #3581 (codex TOCTOU fix): outcome of a locked rebind-origin reap attempt so
/// callers (and tests) can distinguish "reaped" from "skipped because the row
/// was replaced/no-longer-eligible" from "the file was already gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RebindReapOutcome {
    /// The row was re-validated under the lock and unlinked.
    Reaped,
    /// The on-disk row no longer satisfies the reap predicate (e.g. a live
    /// intake/claim replaced it between the unlocked snapshot and the lock) —
    /// deletion was intentionally skipped.
    Skipped,
    /// The state file was already absent (idempotent no-op) or unreadable.
    Missing,
    /// The advisory lock could not be acquired — the caller should retry later.
    LockUnavailable,
}

/// #3581 (codex TOCTOU fix): true when the on-disk `locked` row is still the
/// *same* abandoned rebind-origin orphan identified by the unlocked `snapshot`.
///
/// The placeholder sweeper (and the boot invalidator) pass an unlocked snapshot
/// to `should_reap_abandoned_rebind_origin`; between that read and acquiring the
/// sidecar lock a normal intake / TUI claim can persist a brand-new live
/// inflight at the same `(provider, channel_id)` path. Re-running the reap
/// predicate alone is not sufficient: a *new* rebind-origin orphan could be born
/// (different birth stamp/generation) that also looks structurally abandoned.
/// We therefore additionally require the row's birth identity to be unchanged
/// (`rebind_origin_created_at_unix` + `rebind_origin_birth_generation`), so a
/// replacement turn is never mistaken for the snapshotted orphan.
fn rebind_row_identity_unchanged(snapshot: &InflightTurnState, locked: &InflightTurnState) -> bool {
    locked.rebind_origin == snapshot.rebind_origin
        && locked.rebind_origin_created_at_unix == snapshot.rebind_origin_created_at_unix
        && locked.rebind_origin_birth_generation == snapshot.rebind_origin_birth_generation
        && locked.turn_start_offset == snapshot.turn_start_offset
}

/// #3581 (codex TOCTOU fix): reap an abandoned rebind-origin orphan under the
/// sidecar lock with a re-validate-then-unlink contract. This is the shared
/// implementation behind both the periodic placeholder-sweeper path and the
/// boot-time `invalidate_stale_generation` path so the two stay consistent.
///
/// Contract:
///   1. Acquire the sidecar advisory lock for the row's path (the same
///      non-reentrant `flock` every intake/claim/persist helper takes).
///   2. **Reload** the current on-disk row under the lock.
///   3. Confirm the reloaded row is still the *same* orphan as `snapshot`
///      (`rebind_row_identity_unchanged`) AND still satisfies
///      `should_reap_abandoned_rebind_origin` with a freshly recomputed age
///      (created-at preferred, mtime fallback for legacy rows).
///   4. Unlink **only** when both checks hold; otherwise skip (a live intake /
///      claim replaced the orphan since the snapshot).
///
/// Returns the [`RebindReapOutcome`]; the caller emits observability on
/// [`RebindReapOutcome::Reaped`].
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
    // The unlocked snapshot may have raced a replacement turn. Re-validate the
    // current row's birth identity AND its eligibility under a freshly computed
    // age before touching the file.
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

/// #3581 (codex TOCTOU fix): env-rooted wrapper around
/// [`reap_abandoned_rebind_origin_locked_in_root`] for the periodic
/// placeholder-sweeper path. Returns `true` iff the row was re-validated under
/// the lock and unlinked (so the sweeper only counts/emits genuine reaps).
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

/// #3635: locked re-validate-then-unlink for the *dead-watcher* rebind-origin
/// reap path. The expensive tmux liveness probe runs OUTSIDE this lock (see
/// [`sweep_reap_dead_watcher_rebind_origin`], off the async runtime via
/// `spawn_blocking`); under the lock we re-check only CHEAP, fs-only conditions —
/// no tmux subprocess — so the per-channel sidecar lock is never held across
/// blocking subprocess I/O (codex review ISSUE 2).
///
/// What the lock DOES close: the row-replacement race — a replacement turn that
/// rewrote this sidecar path is rejected by the birth-identity guard, and a row
/// that progressed (structural conjunction) is rejected. What it does NOT fully
/// close (codex review ISSUE 3): a watcher whose tmux *pane* re-appears between
/// the unlocked probe and this unlink. That window is tiny and the consequence
/// is benign — the reaped artifact is a zero-msg-id orphan that a genuinely
/// re-adopting watcher simply re-creates. As a last cheap guard we re-read the
/// runtime-activity mtime under the lock, so a watcher that resumed *writing*
/// (the most common resurrection signal) is still observed and the reap Skips.
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
    // Birth-identity guard (same as the None-owner path): a replacement turn at
    // the same sidecar path must never be mistaken for the snapshotted orphan.
    if !rebind_row_identity_unchanged(snapshot, &locked) {
        return RebindReapOutcome::Skipped;
    }
    let age_secs = rebind_origin_age_secs(&path, &locked);
    // Re-check the cheap, fs-only half of the predicate under the lock (structure
    // + deadline/generation). The tmux liveness probe already ran unlocked; we do
    // NOT re-spawn it here (ISSUE 2 — no subprocess under the sidecar lock).
    if !dead_watcher_rebind_structurally_reapable(&locked, age_secs, current_generation) {
        return RebindReapOutcome::Skipped;
    }
    // Last cheap guard: a watcher that resumed writing its jsonl/`.generation`
    // since the unlocked probe reads as recent activity ⇒ alive ⇒ Skip. Pure fs
    // stat (no subprocess), safe under the lock.
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
/// [`reap_dead_watcher_rebind_origin_locked_in_root`] for the periodic
/// placeholder-sweeper path. Returns `true` iff the row was re-validated under
/// the lock (cheap fs-only re-checks; no tmux subprocess) and unlinked.
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

/// #3635: the placeholder sweeper's single-call entry point for the dead-watcher
/// rebind-origin reap. Returns `true` only when the row was genuinely unlinked.
/// Three stages, ordered cheapest-first:
///   1. fs-only structural gate ([`dead_watcher_rebind_structurally_reapable`]) —
///      most rows fail here with no tmux probe;
///   2. the production [`RuntimeWatcherLiveness`] proven-dead probe, run on a
///      `spawn_blocking` thread (it spawns tmux subprocesses) so the async
///      sweeper runtime is never blocked, and OUTSIDE any lock (codex review
///      ISSUE 2);
///   3. the locked re-validate ([`reap_dead_watcher_rebind_origin_locked`]),
///      which re-checks only cheap fs-only conditions under the sidecar lock.
///
/// Deliberately NOT called from the boot path
/// (`invalidate_stale_generation_in_root`): at cold start a just-restarted live
/// watcher's session reads as dead, so the liveness gate only ever fires in the
/// warm 30s sweeper where a real runtime probe is meaningful (#3154/#3540 / the
/// keystone boot-preservation invariant).
pub(in crate::services::discord) async fn sweep_reap_dead_watcher_rebind_origin(
    provider: &ProviderKind,
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> bool {
    // (1) Cheap, fs-only structural gate first — skip the subprocess entirely for
    // the common case.
    if !dead_watcher_rebind_structurally_reapable(state, age_secs, current_generation) {
        return false;
    }
    // (2) The proven-dead probe spawns tmux subprocesses; run it off the async
    // runtime via `spawn_blocking`, outside any lock. A join failure (panic /
    // runtime shutdown) is treated as "unknown ⇒ preserve".
    let probe_state = state.clone();
    let proven_dead =
        tokio::task::spawn_blocking(move || RuntimeWatcherLiveness.is_proven_dead(&probe_state))
            .await
            .unwrap_or(false);
    if !proven_dead {
        return false;
    }
    // (3) Locked re-validate (cheap fs-only re-checks; no subprocess under lock).
    reap_dead_watcher_rebind_origin_locked(provider, state, current_generation)
}

/// Parse the persisted `started_at` (`now_string` localtime form) back into
/// a Unix timestamp. Returns `None` for unparseable values so callers can
/// fall back to a wall-clock derived approximation.
pub(in crate::services::discord) fn parse_started_at_unix(started_at: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.timestamp())
}

/// Parse a persisted `updated_at` field (same `now_string` localtime form
/// as `started_at`) back into a Unix timestamp. Wrapper kept distinct from
/// `parse_started_at_unix` purely for call-site readability — both fields
/// share the same encoding but represent different lifecycle moments.
pub(in crate::services::discord) fn parse_updated_at_unix(updated_at: &str) -> Option<i64> {
    parse_started_at_unix(updated_at)
}

/// #1446 stall-deadlock recovery: returns `true` when the persisted
/// `updated_at` of an inflight state is older than
/// `threshold_secs` seconds relative to `now_unix_secs`.
///
/// Returns `false` if `updated_at` is unparseable — staleness should never
/// be inferred from missing data. This keeps the helper safe to call from
/// the THREAD-GUARD and stall-watchdog paths even when a partially
/// migrated state file is on disk.
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
/// claim before the bridge tail has created the real response placeholder. If
/// dcserver restarts in that narrow window, the tail is gone and the row has no
/// live relay owner left. Treat that shape as stale after the normal inflight
/// threshold so prompt scanners / idle relays / health recovery do not block
/// forever on a row that cannot make progress.
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
        // #3976 defense-in-depth (symmetry with the orphan-shape predicate): a
        // genuinely confirmed `SessionBoundRelay` delivery sets this durable
        // marker, so even a row that somehow reached owner `None` while carrying it
        // must not be treated as a never-delivered black-hole and re-recovered.
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
