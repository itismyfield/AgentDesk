//! Inflight sidecar filesystem + advisory-lock seam (#3479).
//!
//! Path layout (`inflight_provider_dir` / `inflight_state_path`) and the
//! cross-platform [`InflightStateFileLock`] guard used by every
//! read/modify/write helper in the parent module.

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

/// Outcome of the identity-guarded durable inflight writes in `save_store`
/// (#5951 S1). Decomposed along the two axes callers need: may I restore the
/// projection, and may I treat my own turn as over?
/// [`GuardedSaveOutcome::is_identity_mismatch_legacy`] reproduces the old
/// collapsed value for consumers that predate the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum GuardedSaveOutcome {
    /// On-disk row still matched the turn identity; the row was rewritten.
    Saved,
    /// No inflight row existed (`NotFound` only — see `guarded_read.rs`). The
    /// lease holder may already have cleared it on its success path. The only
    /// refusal a projection repair may ever gate on (#5951 §1.2 mechanism R);
    /// today nothing repairs, so it still resurrects nothing.
    RowAbsent,
    /// A durable or structural authority forbids the write and the row must be
    /// left byte-identical: a planned-restart / rebind-origin marker owns it,
    /// the durable `output_path` moved, a concurrent same-turn writer won the
    /// compare-and-set, the caller's own preconditions do not hold, or
    /// validation refused the refreshed state. Never repairable, and never a
    /// licence to finalize — the caller's turn may still be live.
    AuthorityPinned,
    /// The identity in play cannot be named at all: an offsetless
    /// `user_msg_id == 0` snapshot (or an empty session / tmux / nonce frame)
    /// can never uniquely match a durable row, so the write fails closed. This
    /// is the opposite of a successor turn — it is *no* turn.
    Unnameable,
    /// A different episode demonstrably owns the row: the pinned identity no
    /// longer matches it, its birth offset differs, or a terminal-delivery-
    /// committed row carries another `turn_nonce`. Not repairable, but the
    /// caller's own turn really is over.
    SuccessorOwned,
    /// Filesystem, malformed durable JSON, or serialization error.
    IoError,
}

impl GuardedSaveOutcome {
    /// Every value the pre-#5951 `IdentityMismatch` stood for, in one place.
    /// A consumer that only asks "was this an identity mismatch?" calls this;
    /// one that matches exhaustively spells out the three variants instead,
    /// so a future seventh variant is a compile error at every decision point.
    pub(in crate::services::discord) const fn is_identity_mismatch_legacy(self) -> bool {
        matches!(
            self,
            Self::AuthorityPinned | Self::Unnameable | Self::SuccessorOwned
        )
    }

    /// Classify a refusal whose guard mixes pinned authority with succession.
    /// Reads the row that actually refused: a pinned restart/rebind marker is
    /// [`Self::AuthorityPinned`]; anything else is a successor episode holding
    /// the row.
    pub(in crate::services::discord) fn from_durable_authority(
        on_disk: &InflightTurnState,
    ) -> Self {
        if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
            Self::AuthorityPinned
        } else {
            Self::SuccessorOwned
        }
    }
}

pub(super) fn inflight_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

pub(in crate::services::discord) fn inflight_state_path(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

pub(crate) struct InflightStateFileLock {
    file: Option<fs::File>,
    _population: Option<crate::services::discord::input_runtime::fence::PopulationWriter>,
    state_path: PathBuf,
}

impl InflightStateFileLock {
    pub(in crate::services::discord::inflight) fn state_path(&self) -> &Path {
        &self.state_path
    }
}

impl Drop for InflightStateFileLock {
    fn drop(&mut self) {
        // Ignore explicit-unlock failure: closing the handle immediately after
        // this remains the advisory lock's release fallback.
        if let Some(file) = &self.file {
            let _ = file.unlock();
        }
    }
}

fn inflight_state_lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

#[cfg(test)]
pub(in crate::services::discord) fn second_handle_try_lock(
    path: &Path,
) -> Result<(), std::fs::TryLockError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(inflight_state_lock_path(path))
        .map_err(std::fs::TryLockError::Error)?;
    file.try_lock()
}

/// Acquires the canonical `<channel>.json.lock` sidecar as a blocking exclusive
/// advisory file lock; the JSON row itself remains unlocked. The sidecar is
/// never unlinked, even when no JSON row exists, so an open handle can never
/// become a detached old inode while a new canonical inode admits another
/// writer. Handle close (including process exit) releases the lock;
/// network-filesystem behavior and non-cooperating writers are outside the
/// contract.
pub(crate) fn lock_inflight_state_path(path: &Path) -> Result<InflightStateFileLock, String> {
    if let Some(population) = crate::services::discord::input_runtime::fence::inflight_writer(path)
        .map_err(|failure| format!("input fence: {failure:?}"))?
    {
        return Ok(InflightStateFileLock {
            file: None,
            _population: Some(population),
            state_path: path.to_owned(),
        });
    }
    let file = open_inflight_lock_file(path)?;
    file.lock().map_err(|e| e.to_string())?;
    Ok(InflightStateFileLock {
        file: Some(file),
        _population: None,
        state_path: path.to_path_buf(),
    })
}

// Contention is retryable; scheduler callers must not wait for another open handle.
pub(crate) fn try_lock_inflight_state_path(path: &Path) -> Result<InflightStateFileLock, String> {
    if let Some(population) = crate::services::discord::input_runtime::fence::inflight_writer(path)
        .map_err(|failure| format!("input fence: {failure:?}"))?
    {
        return Ok(InflightStateFileLock {
            file: None,
            _population: Some(population),
            state_path: path.to_owned(),
        });
    }
    let file = open_inflight_lock_file(path)?;
    file.try_lock().map_err(|e| e.to_string())?;
    Ok(InflightStateFileLock {
        file: Some(file),
        _population: None,
        state_path: path.to_path_buf(),
    })
}

pub(in crate::services::discord) struct BorrowedInflightRow<'a> {
    path: PathBuf,
    _guard: &'a crate::services::discord::input_runtime::fence::ClosedPopulationGuard,
}
impl<'a> BorrowedInflightRow<'a> {
    pub(in crate::services::discord) fn new(
        guard: &'a crate::services::discord::input_runtime::fence::ClosedPopulationGuard,
        root: &Path,
        provider: &ProviderKind,
        channel: u64,
    ) -> std::io::Result<Self> {
        guard.borrowed(root, provider, channel, || Ok(()))?;
        Ok(Self {
            path: root
                .join("discord_inflight")
                .join(provider.as_str())
                .join(format!("{channel}.json")),
            _guard: guard,
        })
    }
    pub(super) fn state_path(&self) -> &Path {
        &self.path
    }
}

fn open_inflight_lock_file(path: &Path) -> Result<fs::File, String> {
    let lock_path = inflight_state_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod population_tests {
    use super::*;
    #[test]
    fn b2_off_lock_error_detail_and_representative_writer_results_are_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let channel = 6_325_311;
        let blocker = root.path().join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join(format!("claude/{channel}.json"));
        let expected = open_inflight_lock_file(&path).err().unwrap();
        assert_eq!(lock_inflight_state_path(&path).err().unwrap(), expected);
        assert_eq!(try_lock_inflight_state_path(&path).err().unwrap(), expected);
        let mkdir_error = fs::create_dir_all(path.parent().unwrap()).unwrap_err();
        assert_eq!(mkdir_error.kind(), std::io::ErrorKind::NotADirectory);
        let state = InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            None,
            7,
            8,
            0,
            "input".into(),
            None,
            Some("off".into()),
            None,
            None,
            0,
        );
        assert_eq!(
            super::super::save_store::save_inflight_state_in_root(&blocker, &state).unwrap_err(),
            mkdir_error.to_string()
        );
        use super::super::ownership_ops::{
            StatusPanelBindGuard, StatusPanelBindOutcome, bind_status_panel_in_root,
        };
        assert_eq!(
            bind_status_panel_in_root(
                &blocker,
                &ProviderKind::Claude,
                channel,
                100,
                &StatusPanelBindGuard::default()
            ),
            StatusPanelBindOutcome::IoError
        );
        use super::super::watcher_state::{
            WatcherProgressOutcome, WatcherStreamProgressPatch,
            persist_watcher_stream_progress_locked_in_root,
        };
        let patch = WatcherStreamProgressPatch {
            current_msg_id: None,
            full_response: String::new(),
            response_sent_offset: 0,
            current_tool_line: None,
            prev_tool_status: None,
            task_notification_kind: None,
            any_tool_used: false,
            has_post_tool_text: false,
            streaming_rollover_frozen_msg_ids: vec![],
        };
        assert_eq!(
            persist_watcher_stream_progress_locked_in_root(
                &blocker,
                &ProviderKind::Claude,
                channel,
                None,
                "off",
                patch.clone()
            ),
            WatcherProgressOutcome::IoError
        );
        let empty = root.path().join("empty");
        assert_eq!(
            bind_status_panel_in_root(
                &empty,
                &ProviderKind::Claude,
                channel,
                100,
                &StatusPanelBindGuard::default()
            ),
            StatusPanelBindOutcome::Missing
        );
        assert_eq!(
            persist_watcher_stream_progress_locked_in_root(
                &empty,
                &ProviderKind::Claude,
                channel,
                None,
                "off",
                patch
            ),
            WatcherProgressOutcome::RowAbsent
        );
        let absent = empty.join(format!("claude/{channel}.json"));
        assert!(!absent.exists());
        assert!(
            absent.with_extension("json.lock").exists(),
            "original off writers create sidecar before missing-row result"
        );
        let held = lock_inflight_state_path(&absent).unwrap();
        let baseline = open_inflight_lock_file(&absent)
            .unwrap()
            .try_lock()
            .unwrap_err()
            .to_string();
        assert_eq!(
            try_lock_inflight_state_path(&absent).err().unwrap(),
            baseline
        );
        drop(held);
    }
}

// #3835: shared lock-held persist tail + save-side validation gate, consumed
// by the CAS save/clear children and sibling modules via `use super::*`.

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

    // OBSERVE-ONLY (#3154): a fresh-turn reset (different user_msg_id or
    // turn_start_offset) resets response_sent_offset to 0 on purpose (see
    // InflightTurnState::new), so only a backward move within the SAME turn
    // identity is a violation; the write itself is never skipped here.
    let same_turn_identity = existing.user_msg_id == state.user_msg_id
        && existing.turn_start_offset == state.turn_start_offset;
    let monotonic_offset =
        !same_turn_identity || state.response_sent_offset >= existing.response_sent_offset;
    // I6 last_offset_monotonic: OBSERVE-ONLY here too — a fresh-turn reset
    // lowers last_offset on purpose, so only a same-turn backward move is a
    // violation; the enforcing variant lives in the standby/refresh path.
    let last_offset_monotonic = !same_turn_identity || state.last_offset >= existing.last_offset;

    // #3552: enforce-skip or a legitimate rewind selects WARN below; anything
    // else selects ERROR. #3933: a legitimate RetryBoundary reset (see
    // turn_bridge/retry_state.rs::clear_response_delivery_state) clears
    // full_response and resets response_sent_offset to 0 to re-stream the
    // answer; a genuine regression always carries a non-empty body, so it
    // never matches this signature.
    let is_legitimate_full_reset =
        same_turn_identity && state.full_response.is_empty() && state.response_sent_offset == 0;
    // #4110: terminal-error reset / dead-watcher reclaim intentionally lower a
    // same-turn frontier while keeping a non-empty body, via an
    // identity-checked RMW save carrying this reason marker; ordinary stale
    // snapshots carry no marker and stay blocked.
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
    // #3933: WARN when the enforce guard skips the write OR the rewind
    // classifier permits it; this only changes the severity label, not the
    // enforce guard or debug tripwire below.
    let warn_downgrade_selected = enforce_skips_backward_write || is_legitimate_delivery_rewind;
    let offset_monotonic_severity = offset_monotonic_invariant_severity(warn_downgrade_selected);

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
            // #5500: lets a stored event be triaged after the fact.
            // `full_reset_signature_matched` is a SHAPE test (same_turn_identity
            // + empty full_response + offset 0), not proof of legitimacy. Pair
            // is exhaustive for WARN: enforce_skips_backward_write → skipped;
            // only full_reset_signature_matched → proceeded (mutually
            // exclusive); neither → #4110 reasoned rewind. Missing key = older
            // event, never "false".
            "full_reset_signature_matched": is_legitimate_full_reset,
            "enforce_skips_backward_write": enforce_skips_backward_write,
        }),
        offset_monotonic_severity,
    );
    // #3933: a write the enforce guard is about to SKIP never persists, so the
    // debug tripwire has nothing to catch there; relax it only for that case —
    // a backward move that actually PERSISTS still trips it.
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
            // #5500: same discriminators as above (one severity decision for
            // both invariants) — explain THIS event's severity, not its
            // offsets. #3933's signature says nothing about last_offset; the
            // #4110 rewind can't fire here (needs last_offset_monotonic, false
            // here), so WARN is always exactly one of these keys.
            "full_reset_signature_matched": is_legitimate_full_reset,
            "enforce_skips_backward_write": enforce_skips_backward_write,
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

    // #3416/#3089 B3: observe→ENFORCE under the durable-authority flag (no-op
    // when OFF); see dr::authority_blocks_backward_inflight_write. The
    // violation was already recorded above (WARN, per #3552).
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
/// sidecar lock. Returns `None` on a missing/malformed file (same lenient
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
) -> Result<Option<InflightTurnState>, String> {
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(root, path, &updated, caller) {
        return Ok(None);
    }
    if bump_updated_at {
        updated.updated_at = now_string();
    }
    bump_save_generation_for_write(path, &mut updated);
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(path, &json)?;
    Ok(Some(updated))
}

/// Shared lock-held persist tail: validate, stamp `updated_at`, atomic-write.
/// Caller must already hold `lock_inflight_state_path`.
pub(super) fn persist_under_lock(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    persist_under_lock_inner(root, path, state, caller, true).map(|_| ())
}

/// Persists while returning the exact stamped row written under the lock.
/// Callers that keep a retry baseline must use this instead of retaining the
/// pre-write snapshot, whose timestamp and save generation are stale.
pub(super) fn persist_under_lock_with_snapshot(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<Option<InflightTurnState>, String> {
    persist_under_lock_inner(root, path, state, caller, true)
}

/// Complete a successful restart readoption while the caller owns the canonical
/// sidecar lock. The readoption bit is idempotent, but an outgoing-process
/// restart marker is always consumed before the row can be cleared normally.
pub(super) fn persist_readopted_under_lock(
    root: &Path,
    path: &Path,
    state: &mut InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    let needs_persist = !state.readopted_from_inflight || state.restart_mode.is_some();
    if !needs_persist {
        return Ok(());
    }
    state.readopted_from_inflight = true;
    state.clear_restart_mode();
    persist_under_lock(root, path, state, caller)
}

/// Like [`persist_under_lock`] but preserves the row's existing `updated_at`
/// instead of bumping it to now (#3982 orphan downgrade): an owner correction
/// of a confirmed-dead orphan is not new lifecycle activity, so the
/// quiescence clock must not reset, or a fresh TUI-direct re-read would abort
/// on a "fresh" row.
pub(super) fn persist_under_lock_preserving_updated_at(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    persist_under_lock_inner(root, path, state, caller, false).map(|_| ())
}

#[cfg(test)]
mod relay_state_contract_refs {
    //! #4268 — relay-state contract symbol anchors for the `inflight`
    //! state/store: compiler-checked existence, not comments.
    //!
    //! Every reference below fails to COMPILE if its symbol is renamed, moved,
    //! or removed. `scripts/check_contract_symbol_refs.py` derives the anchor
    //! SET from these reference expressions and checks it against the `sym:`
    //! anchors in `docs/relay-state-contract.md` — no comment can name a
    //! symbol the code doesn't reference.
    //!
    //! The checker whitelists this block's cfg gate and attributes byte-exact:
    //! gate must be `#[cfg(test)]` or `#[cfg(all(test, unix))]`, only `#[test]`
    //! is allowed inside. Hosted here rather than in the frozen `inflight.rs`
    //! (#4269) because several referenced items are `pub(super)` and only
    //! nameable from inside this subtree.
    #[test]
    fn contract_symbols_exist() {
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.response_sent_offset;
        };
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.current_msg_id;
        };
        let _ = |s: &super::super::model::InflightTurnState| {
            let _ = &s.turn_nonce;
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
