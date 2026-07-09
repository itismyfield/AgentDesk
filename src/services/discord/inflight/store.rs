//! Inflight sidecar filesystem + advisory-lock seam (#3479 extraction).
//!
//! The low-level path layout (`inflight_provider_dir` / `inflight_state_path`)
//! and the `flock(2)`-backed [`InflightStateFileLock`] guard
//! (`lock_inflight_state_path`) used by every read/modify/write helper in the
//! parent module. Behaviour-preserving move out of `inflight.rs`; the parent
//! re-exports the cross-module items so existing call sites resolve unchanged.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum InflightDeliveryRewindReason {
    TerminalErrorReset,
    MissingWatcherReclaim,
}

impl InflightDeliveryRewindReason {
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::TerminalErrorReset => "terminal_error_reset",
            Self::MissingWatcherReclaim => "missing_watcher_reclaim",
        }
    }
}

pub(super) fn inflight_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

pub(in crate::services::discord::inflight) fn inflight_state_path(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

pub(crate) struct InflightStateFileLock {
    _file: fs::File,
}

impl Drop for InflightStateFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Best effort unlock; closing the fd would release it anyway.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn inflight_state_lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

pub(crate) fn lock_inflight_state_path(path: &Path) -> Result<InflightStateFileLock, String> {
    let lock_path = inflight_state_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(InflightStateFileLock { _file: file })
}

// ---------------------------------------------------------------------------
// #3835: shared lock-held persist tail + save-side validation gate.
//
// Moved verbatim from the `inflight` parent so the CAS save/clear children and
// the sibling child modules (`watcher_state`, `ownership_ops`,
// `orphan_relay_reclaim`, `finalizer_identity`) consume one primitive layer.
// The parent re-imports these at their original inflight-private visibility, so
// `super::persist_under_lock` / `super::validate_inflight_state_for_save` (and the
// CAS children's unqualified calls via `use super::*`) resolve unchanged.
// `persist_under_lock_inner` stays module-private here (internal to the two
// persist wrappers). Behaviour-preserving: no function body is altered.
// ---------------------------------------------------------------------------

pub(super) fn validate_inflight_state_for_save(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    code_location: &'static str,
) -> bool {
    validate_inflight_state_for_save_with_delivery_rewind_reason(
        root,
        path,
        state,
        code_location,
        None,
    )
}

pub(super) fn validate_inflight_state_for_save_with_delivery_rewind_reason(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    code_location: &'static str,
    delivery_rewind_reason: Option<InflightDeliveryRewindReason>,
) -> bool {
    let offset_in_bounds = state.response_sent_offset <= state.full_response.len()
        && state
            .full_response
            .is_char_boundary(state.response_sent_offset);
    record_inflight_invariant(
        offset_in_bounds,
        state,
        "response_sent_offset_in_bounds",
        code_location,
        "inflight response_sent_offset must stay within full_response",
        serde_json::json!({
            "response_sent_offset": state.response_sent_offset,
            "full_response_len": state.full_response.len(),
            "path": path.display().to_string(),
        }),
    );
    debug_assert!(
        offset_in_bounds,
        "inflight response_sent_offset must stay within full_response"
    );

    let Ok(existing_content) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(existing) = serde_json::from_str::<InflightTurnState>(&existing_content) else {
        return true;
    };

    // #3154 — OBSERVE-ONLY on the bridge/watcher save path. A legit fresh-turn
    // reset (different user_msg_id or turn_start_offset) resets
    // response_sent_offset to 0 on purpose (see InflightTurnState::new), so the
    // check is gated by SAME turn identity; only a backward move within the same
    // turn is a violation. We do not skip the write here (that would drop a
    // legit fresh turn); this mirrors the last_offset_monotonic precedent below.
    let same_turn_identity = existing.user_msg_id == state.user_msg_id
        && existing.turn_start_offset == state.turn_start_offset;
    let monotonic_offset =
        !same_turn_identity || state.response_sent_offset >= existing.response_sent_offset;
    // I6 (last_offset_monotonic) — OBSERVE-ONLY on the bridge/watcher save
    // path. A legit fresh-turn reset (different user_msg_id or
    // turn_start_offset) lowers last_offset on purpose, so the check is gated
    // by SAME turn identity; only a backward move within the same turn is a
    // violation. We do not skip the write here (that would drop a legit fresh
    // turn); the enforcing variant lives in the standby/refresh path.
    let last_offset_monotonic = !same_turn_identity || state.last_offset >= existing.last_offset;

    // #3552: when the #3416 enforce guard (below) will SKIP this backward write
    // and preserve the offset (zero data loss), the offset-monotonic violation
    // has already been safely handled — record it at WARN instead of ERROR so
    // the paired `#3416 enforce` WARN is the only operator-facing log, killing
    // the duplicate ERROR-log noise. When enforce is OFF a GENUINE (non-reset)
    // backward write actually persists below, so that violation stays ERROR (a
    // real breach); the legitimate re-stream reset (#3933) is handled separately
    // just before the records below. Computed BEFORE the records so the severity
    // is correct; the enforce branch itself (skip + return false) is unchanged.
    // #3933: a legitimate Gemini/Qwen `RetryBoundary` reset rewinds the SAME
    // turn's frontier to the start — `full_response` cleared and
    // `response_sent_offset` back to 0 — to re-stream the answer
    // (turn_bridge/retry_state.rs::clear_response_delivery_state). That backward
    // move is NOT a stale-snapshot regression, so the enforce guard must permit
    // it (the release runs AGENTDESK_DELIVERY_RECORD_AUTHORITY=1; blocking it
    // drops the re-streamed body). A genuine backward regression carries a
    // non-empty body, so it never matches this rewind signature and stays
    // blocked. The signal is derived here from the incoming state — no call-site
    // change — so the guard stays self-contained.
    let is_legitimate_full_reset =
        same_turn_identity && state.full_response.is_empty() && state.response_sent_offset == 0;
    // #4110: terminal-error reset and dead-watcher reclaim intentionally lower a
    // same-turn delivery frontier while keeping a non-empty response body. Those
    // are not generic saves: the bridge first performs an identity-checked,
    // lock-held RMW save carrying this reason marker. Only that path may carve
    // out the non-empty backward move; ordinary stale snapshots still have no
    // marker and remain blocked by authority.
    let is_legitimate_reasoned_delivery_rewind = delivery_rewind_reason.is_some()
        && same_turn_identity
        && !state.full_response.is_empty()
        && state.response_sent_offset < existing.response_sent_offset
        && last_offset_monotonic;
    let is_legitimate_delivery_rewind =
        is_legitimate_full_reset || is_legitimate_reasoned_delivery_rewind;
    use crate::services::discord::outbound::delivery_record as dr;
    let authority = dr::delivery_record_authority_enabled();
    let enforce_skips_backward_write = dr::authority_blocks_backward_inflight_write(
        authority,
        monotonic_offset,
        last_offset_monotonic,
        is_legitimate_delivery_rewind,
    );
    // #3933: a legitimate full reset PERSISTS its backward write (it is a permitted
    // re-stream rewind, so the enforce guard does NOT skip it —
    // `enforce_skips_backward_write` is false). That rewind is intended, not a
    // data-loss regression, so it must not surface an operator-facing ERROR: treat
    // it as "safely handled" (WARN) exactly like the enforce-skip case. This is a
    // severity-label change ONLY — the enforce guard, the debug tripwire (which
    // still keys off `enforce_skips_backward_write`), and the on-disk schema are
    // all unchanged.
    let monotonic_violation_safely_handled =
        enforce_skips_backward_write || is_legitimate_delivery_rewind;
    let offset_monotonic_severity =
        offset_monotonic_invariant_severity(monotonic_violation_safely_handled);

    record_inflight_invariant_with_severity(
        monotonic_offset,
        state,
        "response_sent_offset_monotonic",
        code_location,
        "inflight response_sent_offset must not move backwards for the same turn identity",
        serde_json::json!({
            "previous": existing.response_sent_offset,
            "next": state.response_sent_offset,
            "same_turn_identity": same_turn_identity,
            "path": path.display().to_string(),
            "delivery_rewind_reason": delivery_rewind_reason.map(InflightDeliveryRewindReason::as_str),
        }),
        offset_monotonic_severity,
    );
    // #3933: when the enforce guard is about to SKIP this backward write it never
    // persists, so the debug tripwire has nothing to catch — asserting there would
    // panic on a write we already discard. Relax the tripwire for that skipped
    // case only; a backward move that actually PERSISTS (enforce OFF, or a
    // permitted legitimate reset in release) still trips it, preserving the
    // tripwire's purpose and every existing observe-only test verbatim.
    debug_assert!(
        monotonic_offset || enforce_skips_backward_write || is_legitimate_reasoned_delivery_rewind,
        "inflight response_sent_offset must not move backwards for the same turn identity"
    );

    record_inflight_invariant_with_severity(
        last_offset_monotonic,
        state,
        "last_offset_monotonic",
        code_location,
        "inflight last_offset must not move backwards for the same turn identity",
        serde_json::json!({
            "previous": existing.last_offset,
            "next": state.last_offset,
            "same_turn_identity": same_turn_identity,
            "path": path.display().to_string(),
        }),
        offset_monotonic_severity,
    );
    debug_assert!(
        last_offset_monotonic || enforce_skips_backward_write,
        "inflight last_offset must not move backwards for the same turn identity"
    );

    let same_tmux_owner = existing.tmux_session_name.is_none()
        || state.tmux_session_name.is_none()
        || existing.tmux_session_name == state.tmux_session_name;
    record_inflight_invariant(
        same_tmux_owner,
        state,
        "inflight_tmux_one_to_one",
        code_location,
        "inflight state for a channel must not drift between tmux sessions",
        serde_json::json!({
            "previous_tmux_session_name": existing.tmux_session_name.as_deref(),
            "next_tmux_session_name": state.tmux_session_name.as_deref(),
            "root": root.display().to_string(),
            "path": path.display().to_string(),
        }),
    );

    // #3416 (#3089 B3): observe→ENFORCE under the durable-authority flag (no-op
    // when OFF); see dr::authority_blocks_backward_inflight_write. The violation
    // itself was already recorded by the monotonic record_inflight_invariant
    // above (downgraded to WARN for this skipped-write case — see #3552).
    if enforce_skips_backward_write {
        tracing::warn!(
            "#3416 enforce: skipped backward inflight write at {}",
            path.display()
        );
        return false;
    }
    true
}

/// Reads + deserializes the inflight row at `path` while the caller holds the
/// sidecar flock. Returns `None` on a missing/malformed file (same lenient
/// posture as `load_inflight_state`).
pub(super) fn load_inflight_state_unlocked(path: &Path) -> Option<InflightTurnState> {
    let data = fs::read_to_string(path).ok()?;
    parse_inflight_state_content(&data).ok()
}

/// Shared lock-held persist tail: validate, optionally stamp `updated_at`,
/// atomic-write. Caller must already hold `lock_inflight_state_path`.
///
/// `bump_updated_at` controls whether `updated_at` is reset to now. Real
/// lifecycle mutations bump it (quiescence clock resets); an owner *correction*
/// of a proven-dead orphan (#3982) preserves the old, already-stale timestamp so
/// downstream ownerless-stale filters drop the row immediately on the next read
/// instead of after another 300 s window.
fn persist_under_lock_inner(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
    bump_updated_at: bool,
) -> Result<(), String> {
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(root, path, &updated, caller) {
        return Ok(());
    }
    if bump_updated_at {
        updated.updated_at = now_string();
    }
    bump_save_generation_for_write(path, &mut updated);
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(path, &json)
}

/// Shared lock-held persist tail: validate, stamp `updated_at`, atomic-write.
/// Caller must already hold `lock_inflight_state_path`.
pub(super) fn persist_under_lock(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    persist_under_lock_inner(root, path, state, caller, true)
}

/// Like [`persist_under_lock`] but preserves the row's existing `updated_at`
/// instead of bumping it to now. Used by the #3982 orphan downgrade: the owner
/// correction of a confirmed-dead orphan is not new lifecycle activity, so its
/// quiescence clock must not be reset, or the triggering TUI-direct turn's fresh
/// re-read would see a "fresh" row and keep aborting.
pub(super) fn persist_under_lock_preserving_updated_at(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    persist_under_lock_inner(root, path, state, caller, false)
}

#[cfg(test)]
mod relay_state_contract_refs {
    //! #4268 — relay-state contract symbol anchors for the `inflight`
    //! state/store (compiler-checked existence).
    //!
    //! Every statement below is a real reference that fails to COMPILE if its
    //! symbol is renamed, moved, or removed, so `cargo check --workspace
    //! --all-targets` (a required CI gate) is the source of truth for whether
    //! each contract symbol still exists.
    //! `scripts/check_contract_symbol_refs.py` parses the anchor SET from these
    //! reference expressions — never from comments — and checks it equals the
    //! `sym:` anchors in `docs/relay-state-contract.md`.
    //!
    //! There are deliberately no `// sym:` labels: the anchor name is derived
    //! from the reference the compiler checks (`use` path / field expression), so
    //! commenting out or `use super::*;`-replacing a reference removes its anchor
    //! and the set comparison fails — no comment can name a symbol the code does
    //! not actually reference. The block cfg gate and the attributes inside are
    //! byte-exact whitelists in the checker (no cfg parser): the gate must be
    //! `#[cfg(test)]` or `#[cfg(all(test, unix))]` (the latter for a `#[cfg(unix)]`
    //! symbol — see the watchdog block), and the only attribute allowed inside is
    //! `#[test]`. This blocks a feature/non-ubuntu block gate AND an item-level
    //! cfg on a reference, either of which would drop a symbol from the required
    //! compile and silently disable the proof.
    //!
    //! Hosted here (not in `inflight.rs`) because several referenced items are
    //! `pub(super)` within the `inflight` subtree and are only nameable from
    //! inside it, while `inflight.rs` is a frozen test-residue file whose ceiling
    //! must not grow (#4269). This whole module is `#[cfg(test)]`, so it is test
    //! LoC and adds no production surface.
    #[test]
    fn contract_symbols_exist() {
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.response_sent_offset;
        };
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.current_msg_id;
        };
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.last_offset;
        };
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.last_watcher_relayed_offset;
        };
        let _ = super::super::model::InflightTurnState::effective_relay_owner_kind;
        use super::super::clear_store::refresh_inflight_last_offset_if_matches_identity as _;
        use super::super::clear_store::refresh_inflight_last_offset_if_matches_identity_in_root as _;
        use super::super::save_store::save_inflight_state as _;
        use super::validate_inflight_state_for_save as _;
    }
}
