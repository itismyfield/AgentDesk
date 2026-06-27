use super::*;

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
    /// True only when the watcher owning `state` is *provably* dead: its tmux
    /// session has no live pane AND no runtime activity (jsonl/.generation
    /// mtime) has advanced within [`DEAD_WATCHER_PROVEN_DEAD_SECS`]. Any
    /// positive signal (or an unknown session name) yields `false` — the
    /// conservative, live-protecting default.
    fn is_proven_dead(&self, state: &InflightTurnState) -> bool;
}

/// #3635: production [`WatcherLiveness`] backed by the same runtime signals the
/// stall-watchdog (#3169) and #3629 stall-liveness already trust:
/// `tmux_diagnostics::tmux_session_has_live_pane` and
/// `dispatched_sessions::latest_runtime_activity_unix_nanos`.
pub(super) struct RuntimeWatcherLiveness;

impl WatcherLiveness for RuntimeWatcherLiveness {
    fn is_proven_dead(&self, state: &InflightTurnState) -> bool {
        use crate::services::platform::tmux::PaneLiveness;
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
        // license reaping a live-but-idle watcher. Only a *definitive* dead/absent
        // answer is even a candidate for reap; any live signal or unknown answer
        // preserves the row (the #3154/#3540 live-protection invariant at its
        // weakest point).
        match crate::services::tmux_diagnostics::tmux_session_pane_liveness(session) {
            PaneLiveness::Live => return false,       // hard "alive" signal
            PaneLiveness::ProbeError => return false, // unknown ⇒ preserve
            PaneLiveness::DeadOrAbsent => {}          // candidate — confirm via activity
        }
        // No live pane, definitively. Fresh runtime activity (jsonl / .generation
        // mtime) within the window is still "alive" — a just-restarting watcher.
        // Only a dead/absent session AND no recent activity is provably dead.
        !watcher_runtime_activity_recent(session)
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
/// `liveness.is_proven_dead(state)`. A live watcher (positive tmux pane or
/// recent runtime activity) is `is_proven_dead == false`, so this predicate is
/// `false` and the row is preserved — the #3154/#3540 / keystone live-
/// protection invariant holds verbatim. Every non-owner live signal (adoption,
/// streamed bytes, anchor, terminal commit, offset progress, planned restart)
/// still independently blocks the reap via the shared structural conjunction.
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
