use super::*;

pub fn subscribe_observed_prompts() -> broadcast::Receiver<ObservedTuiPrompt> {
    OBSERVED_PROMPTS.subscribe()
}

pub fn register_provider_session(
    provider: &str,
    provider_session_id: &str,
    tmux_session_name: &str,
) {
    let provider_session_id = provider_session_id.trim();
    let tmux_session_name = tmux_session_name.trim();
    if provider_session_id.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        state.tmux_by_provider_session.insert(
            PromptKey::new(provider, provider_session_id),
            TimedValue {
                value: tmux_session_name.to_string(),
                recorded_at: Instant::now(),
            },
        );
    }
}

/// Reverse lookup: resolve the provider session id that maps to `tmux_session_name`
/// for `provider`, if one was registered. `register_provider_session` records
/// the forward `provider_session_id -> tmux_session_name` mapping at launch;
/// this scans it for the entry whose value matches the tmux session.
///
/// #tui-hook-ttl-buffer key-match fix: the Claude hook relay buffers under the
/// PROVIDER session UUID (`config.session_id`), but the readiness layer only
/// knows the tmux session name. Callers use this to claim the SAME key the hooks
/// buffered under instead of the tmux fallback (which the buffer never used for a
/// hosted Claude launch). Returns `None` when no mapping is known, in which case
/// the caller should fall back to the tmux session name.
pub fn provider_session_for_tmux(provider: &str, tmux_session_name: &str) -> Option<String> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    // The forward map can in principle hold multiple provider session ids that
    // pointed at the same tmux session over time; prefer the most recently
    // recorded survivor. Do not TTL-expire this bridge: long-lived TUI sessions
    // can keep emitting hooks with the same provider UUID after the ordinary
    // prompt-cache TTL has elapsed.
    state
        .tmux_by_provider_session
        .iter()
        .filter(|(promptkey, timed)| {
            promptkey.provider == provider && timed.value == tmux_session_name
        })
        .max_by_key(|(_, timed)| timed.recorded_at)
        .map(|(promptkey, _)| promptkey.key.clone())
}

pub(crate) fn provider_session_is_registered(provider: &str, provider_session_id: &str) -> bool {
    resolve_tmux_session_name(provider, provider_session_id).is_some()
}

pub fn register_tmux_channel(tmux_session_name: &str, channel_id: u64) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || channel_id == 0 {
        return;
    }
    if tmux_session_name.contains("-dm-") {
        if let Err(error) =
            crate::services::tmux_common::write_tmux_channel_binding(tmux_session_name, channel_id)
        {
            tracing::warn!(
                tmux_session_name,
                channel_id,
                %error,
                "failed to persist DM tmux channel binding"
            );
        }
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.channel_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: channel_id,
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn register_tmux_runtime_binding(tmux_session_name: &str, binding: TuiRuntimeBinding) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || binding.output_path.trim().is_empty() {
        return;
    }
    if binding.relay_output_path().trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: binding,
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn register_rehydrated_tmux_runtime_binding(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    binding: TuiRuntimeBinding,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || channel_id == 0
        || binding.output_path.trim().is_empty()
        || binding.relay_output_path().trim().is_empty()
    {
        return;
    }
    let session_id = binding.session_id.clone();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: binding,
            recorded_at: Instant::now(),
        },
    );
    state.channel_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: channel_id,
            recorded_at: Instant::now(),
        },
    );
    if let Some(session_id) = session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        state.tmux_by_provider_session.insert(
            PromptKey::new(&provider, session_id),
            TimedValue {
                value: tmux_session_name.to_string(),
                recorded_at: Instant::now(),
            },
        );
    }
}

/// #3018: DIAGNOSTIC / MIRROR USE ONLY.
///
/// This expiry-based cache is NOT the authority for tmux-session→channel
/// resolution. The authoritative source is the `tmux_watchers` registry
/// (`SharedData::tmux_watchers`), which holds the 1:1 routing invariant. This
/// lookup may only be used for best-effort diagnostics / rehydration hints — it
/// must never be used as a reverse authority to route relays, or drift between
/// the two sources will silently mis-route. See
/// `tui_prompt_relay::owner_channel_for_tmux_session`.
pub fn owner_channel_for_tmux_session(tmux_session_name: &str) -> Option<u64> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .channel_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value)
}

/// Test-only: reset the entire dedupe state. Crate-visible so sibling modules
/// (e.g. `tui_prompt_relay` regression tests) can isolate the shared
/// prompt-anchor slot under `TEST_LOCK`.
#[cfg(test)]
pub(crate) fn reset_state_for_tests() {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    *state = TuiPromptDedupeState::default();
}

/// Test-only: record a prompt anchor whose `recorded_at` is backdated by `age`,
/// so a test can simulate an anchor stamped at submit time for a turn that has
/// been streaming for `age`. Crate-visible so sibling modules (e.g. the
/// `turn_bridge` same-input correlation tests) can pin that a long streaming
/// turn's anchor still resolves past the legacy 30min purge under
/// `PROMPT_ANCHOR_SUBMIT_TTL`.
#[cfg(test)]
pub(crate) fn record_prompt_anchor_aged_for_tests(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    message_id: u64,
    age: Duration,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 || message_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.prompt_anchor_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: TuiPromptAnchor {
                channel_id,
                message_id,
            },
            recorded_at: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
        },
    );
}

pub(crate) fn record_prompt_anchor(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    message_id: u64,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 || message_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.prompt_anchor_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: TuiPromptAnchor {
                channel_id,
                message_id,
            },
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn take_prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<TuiPromptAnchor> {
    let anchor = prompt_anchor_for_response(provider, tmux_session_name, channel_id)?;
    clear_prompt_anchor_for_response(provider, tmux_session_name, anchor);
    Some(anchor)
}

pub(crate) fn prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<TuiPromptAnchor> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let anchor = state.prompt_anchor_by_tmux.get(&key)?.value;
    if anchor.channel_id != channel_id {
        return None;
    }
    Some(anchor)
}

pub(crate) fn clear_prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    anchor: TuiPromptAnchor,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let key = PromptKey::new(&provider, tmux_session_name);
        let Some(current) = state
            .prompt_anchor_by_tmux
            .get(&key)
            .map(|entry| entry.value)
        else {
            return false;
        };
        if current != anchor {
            return false;
        }
        state.prompt_anchor_by_tmux.remove(&key);
        true
    };
    if removed {
        clear_ssh_direct_observation_pending(&provider, tmux_session_name);
    }
    removed
}

/// #3956: re-stamp an EXISTING submit prompt anchor's `recorded_at` to "now" on
/// observed streaming activity for `(provider, tmux, channel)`. A turn that
/// streams continuously longer than [`PROMPT_ANCHOR_SUBMIT_TTL`] (4h) would
/// otherwise have its anchor expire mid-stream, so the #3885 same-input
/// follow-up-requeue correlation peek ([`prompt_anchor_for_response`]) resolves
/// `None`, `same_input` reads false, and the no-response requeue re-fires
/// duplicate prose. The tmux watcher's per-pane streaming-observation path calls
/// this on every observed output chunk so the anchor stays live for the whole
/// turn, making the correlation TTL-independent (the issue #3956 full fix).
///
/// This is a REFRESH-on-activity, NOT a new lifecycle, and a SINGLE-MAP op:
///   * it only advances an anchor that ALREADY exists for the MATCHING channel —
///     it never resurrects a different channel's anchor and never CREATES one, so
///     a genuinely-unsubmitted pane stays anchor-less and the bridge still
///     requeues it;
///   * it reads/writes ONLY `prompt_anchor_by_tmux`. Crucially it does NOT call
///     [`TuiPromptDedupeState::purge_expired`]: this fires on EVERY watcher
///     chunk-drain (a #3016 hot path), so a global multi-map purge under the lock
///     would scan/mutate the #3459/#3303 `relayed_entry_ids_by_tmux` ledger and
///     every other dedupe map on each chunk. Leaving the ledger entirely untouched
///     is what makes the #3459/#3303 non-regression REAL, not merely benign — and
///     keeps the hot-path op cheap.
///
/// No-resurrection WITHOUT the global purge: the matching anchor's age is checked
/// INLINE against the 4h ceiling. A still-live anchor (< 4h) is re-stamped; a
/// matching anchor already past 4h belongs to a long-dead turn, so it is NOT
/// refreshed (and is evicted from this one map so it cannot linger). The peek path
/// [`prompt_anchor_for_response`] runs its OWN `purge_expired`, so anchor expiry is
/// still enforced there independently of this path.
/// Returns `true` iff a live matching-channel anchor was present and re-stamped.
pub(crate) fn touch_prompt_anchor_on_activity(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.prompt_anchor_by_tmux.get_mut(&key) else {
        return false;
    };
    if entry.value.channel_id != channel_id {
        return false;
    }
    if entry.recorded_at.elapsed() < PROMPT_ANCHOR_SUBMIT_TTL {
        // Live turn: re-stamp this one entry so the stream's anchor stays fresh.
        entry.recorded_at = Instant::now();
        return true;
    }
    // The matching anchor is already past the 4h ceiling — a long-dead turn's
    // anchor. Do NOT refresh it (no-resurrection guarantee); evict just this one
    // entry so it cannot linger, without scanning or mutating any other map.
    state.prompt_anchor_by_tmux.remove(&key);
    false
}

/// #3174: record a deferred ⏳-completion marker for `(provider, tmux, channel)`,
/// stamped with the TURN IDENTITY `turn_lease_generation`.
///
/// Called by the watcher's lease-gated completion path when the gate fired (the
/// external-input lease for THIS turn was present before relay) but the prompt
/// anchor for this turn has not been recorded yet — the provider committed
/// terminal output inside the sub-second `notify-post + ⏳-add` window. Without
/// this marker the anchor-less completion is a silent no-op and the ⏳ is
/// stranded (the lease is cleared after this delivery, so no later pass
/// reconciles it). The SAME turn's [`record_prompt_anchor`] drains this marker
/// (via [`take_deferred_anchor_completion`]) and the relay finishes the ⏳ → ✅
/// swap against the just-recorded anchor.
///
/// `turn_lease_generation` is the `generation` of the external-input lease the
/// completion was gated on (see [`ExternalInputRelayLease::generation`]) — a
/// unique monotonic per-record nonce that identifies the turn. The drain only
/// consumes a marker whose generation MATCHES the draining turn's, so within the
/// marker TTL a NEWER same-(provider,tmux) turn can never complete the previous
/// turn's ⏳. A marker stamped with the `UNRECORDED` sentinel (0) is never
/// recorded — it carries no turn identity, so it cannot be safely drained.
pub(crate) fn record_deferred_anchor_completion(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    turn_lease_generation: u64,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || channel_id == 0
        || turn_lease_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.deferred_anchor_completion_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: turn_lease_generation,
            recorded_at: Instant::now(),
        },
    );
}

/// #3174: peek (read, do NOT clear) whether a deferred ⏳-completion marker for
/// `(provider, tmux)` matching `turn_lease_generation` is present. Returns `true`
/// iff a non-expired marker stamped with EXACTLY this turn's generation exists.
///
/// #3174 codex P2 (HTTP fail-open): the relay peeks BEFORE attempting the
/// ⏳ → ✅ delivery, so it can decide whether a swap is owed WITHOUT consuming the
/// marker. The marker is only removed via [`take_deferred_anchor_completion`]
/// once the swap can actually be delivered; if command_http is unavailable the
/// marker is left in place (mirrors the #3164 ⏳-add fail-open: never strand
/// worse than before).
pub(crate) fn deferred_anchor_completion_present_for_turn(
    provider: &str,
    tmux_session_name: &str,
    turn_lease_generation: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || turn_lease_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .deferred_anchor_completion_by_tmux
        .get(&PromptKey::new(&provider, tmux_session_name))
        .is_some_and(|entry| entry.value == turn_lease_generation)
}

/// #3174: drain (read-and-clear) a deferred ⏳-completion marker for
/// `(provider, tmux)` IFF it is stamped with THIS turn's
/// `turn_lease_generation`. Returns `true` iff such a marker was present and was
/// removed.
///
/// Called by [`record_prompt_anchor`]'s site in the relay immediately after the
/// anchor is recorded (and ⏳ added). Turn-identity safe by construction: the
/// marker stores the `generation` of the lease the watcher completion was gated
/// on, and the relay passes the `generation` of the lease THIS same invocation
/// recorded. A marker set by a DIFFERENT turn (older or newer) on the same
/// `(provider, tmux)` carries a different generation and is left untouched — it
/// can never cross-complete the wrong turn's ⏳.
pub(crate) fn take_deferred_anchor_completion(
    provider: &str,
    tmux_session_name: &str,
    turn_lease_generation: u64,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || turn_lease_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED
    {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let matches = state
        .deferred_anchor_completion_by_tmux
        .get(&key)
        .is_some_and(|entry| entry.value == turn_lease_generation);
    if matches {
        state.deferred_anchor_completion_by_tmux.remove(&key);
    }
    matches
}

pub(crate) fn runtime_binding_for_tmux_session(
    tmux_session_name: &str,
) -> Option<TuiRuntimeBinding> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .runtime_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value.clone())
}

/// Adopt the actual Claude session UUID reported inside a hook payload while
/// retaining the launch-time UUID as a stable hook-routing alias (#4423).
///
/// Claude continuation keeps the tmux process and hook command alive but moves
/// transcript writes to a new `<uuid>.jsonl`.  The hook command therefore still
/// addresses the old UUID while stdin carries the new one.  This update is
/// deliberately limited to an existing ClaudeTui binding reached through the
/// command UUID and to a real sibling transcript file. For a second or later
/// continuation hop, the candidate must also be newer than the transcript
/// currently bound to that pane. It never guesses across project directories.
pub(crate) fn adopt_claude_continuation_session(
    command_session_id: &str,
    payload_session_id: &str,
) -> Option<(String, String)> {
    let command_session_id = command_session_id.trim();
    let payload_session_id = payload_session_id.trim();
    if command_session_id.is_empty()
        || payload_session_id.is_empty()
        || command_session_id == payload_session_id
        || uuid::Uuid::parse_str(payload_session_id).is_err()
    {
        return None;
    }

    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let command_key = PromptKey::new("claude", command_session_id);
    let tmux_session_name = state
        .tmux_by_provider_session
        .get(&command_key)?
        .value
        .clone();
    let binding = state.runtime_by_tmux.get(&tmux_session_name)?;
    if binding.value.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return None;
    }
    let old_output_path = PathBuf::from(&binding.value.output_path);
    let new_output_path = old_output_path
        .parent()?
        .join(format!("{payload_session_id}.jsonl"));
    if !new_output_path.is_file() {
        return None;
    }
    if let Some(current_session_id) = binding.value.session_id.as_deref()
        && current_session_id != command_session_id
        && current_session_id != payload_session_id
    {
        let current_mtime = std::fs::metadata(&old_output_path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        let candidate_mtime = std::fs::metadata(&new_output_path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        if candidate_mtime <= current_mtime {
            return None;
        }
    }
    let new_output_path = new_output_path.display().to_string();

    if binding.value.session_id.as_deref() == Some(payload_session_id)
        && binding.value.output_path == new_output_path
    {
        // Subsequent hooks still carry the launch-time query UUID. Do not reset
        // the already-adopted continuation cursor to zero on every event.
        state.tmux_by_provider_session.insert(
            PromptKey::new("claude", payload_session_id),
            TimedValue {
                value: tmux_session_name.clone(),
                recorded_at: Instant::now(),
            },
        );
        state.tmux_by_provider_session.insert(
            command_key,
            TimedValue {
                value: tmux_session_name.clone(),
                recorded_at: Instant::now(),
            },
        );
        return Some((tmux_session_name, new_output_path));
    }

    let binding = state.runtime_by_tmux.get_mut(&tmux_session_name)?;
    binding.value.output_path = new_output_path.clone();
    binding.value.relay_output_path = None;
    binding.value.session_id = Some(payload_session_id.to_string());
    // Start conservatively at the new transcript head. Stable prompt-entry
    // identities suppress replay; starting at EOF would silently skip the
    // continuation boundary that taught us the new UUID.
    binding.value.last_offset = 0;
    binding.value.relay_last_offset = None;
    binding.recorded_at = Instant::now();
    state.tmux_by_provider_session.insert(
        PromptKey::new("claude", payload_session_id),
        TimedValue {
            value: tmux_session_name.clone(),
            recorded_at: Instant::now(),
        },
    );
    // The running Claude process can cache the launch-time hook command even
    // after its settings artifact is rewritten. Keep that observed command
    // identity newest for future waits until dcserver rehydration establishes
    // the persisted payload UUID as the sole mapping.
    state.tmux_by_provider_session.insert(
        command_key,
        TimedValue {
            value: tmux_session_name.clone(),
            recorded_at: Instant::now(),
        },
    );
    Some((tmux_session_name, new_output_path))
}

pub(crate) fn refresh_tmux_runtime_binding_activity(
    tmux_session_name: &str,
    output_path: &str,
) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    let output_path = output_path.trim();
    if tmux_session_name.is_empty() || output_path.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
        return false;
    };
    if entry.value.output_path == output_path
        || entry.value.relay_output_path.as_deref() == Some(output_path)
    {
        entry.recorded_at = Instant::now();
        return true;
    }
    false
}

pub(crate) fn clear_tmux_runtime_binding(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_provider_sessions
    };
    crate::services::claude_compact_context::clear_launch_provenance_for_tmux(tmux_session_name);
    crate::services::claude_compact_trigger::clear_for_tmux(tmux_session_name);
    removed
}

/// #3105 (codex P1 sub-case B): tombstone-evict every mirror mapping for a tmux
/// session that has been determined dead/orphaned (pane gone AND no live watcher
/// AND no authoritative owner). This removes BOTH the runtime binding (which the
/// idle relay loop iterates) AND the best-effort channel mirror (which the
/// drift-alert resolver reads), so subsequent relay-loop iterations no longer
/// find a stale mapping and stop re-emitting the per-poll drift/skip WARN.
///
/// This is NOT a routing authority change: it only forgets a mirror entry whose
/// session is genuinely gone. A later legitimate re-registration (launch script
/// rehydrate or a fresh watcher) re-populates these maps normally, so a session
/// that comes back relays again.
///
/// Returns `true` when at least one mirror entry was removed (so callers can
/// emit a single bounded incident instead of per-poll spam).
pub(crate) fn evict_dead_tmux_mirror(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        let removed_channel = state.channel_by_tmux.remove(tmux_session_name).is_some();
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_channel || removed_provider_sessions
    };
    crate::services::claude_compact_context::clear_launch_provenance_for_tmux(tmux_session_name);
    crate::services::claude_compact_trigger::clear_for_tmux(tmux_session_name);
    removed
}

pub(crate) fn runtime_bindings_for_kind(
    runtime_kind: RuntimeHandoffKind,
) -> Vec<(String, TuiRuntimeBinding)> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .runtime_by_tmux
        .iter()
        .filter(|(_, entry)| entry.value.runtime_kind == runtime_kind)
        .map(|(tmux_session_name, entry)| (tmux_session_name.clone(), entry.value.clone()))
        .collect()
}

pub(crate) fn advance_tmux_runtime_binding_offset(
    tmux_session_name: &str,
    output_path: &str,
    last_offset: u64,
) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || output_path.trim().is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
        return false;
    };
    if entry.value.output_path == output_path {
        entry.value.last_offset = last_offset;
        if entry.value.relay_output_path.is_none() {
            entry.value.relay_last_offset = Some(last_offset);
        }
        entry.recorded_at = Instant::now();
        return true;
    }
    if entry.value.relay_output_path.as_deref() != Some(output_path) {
        return false;
    }
    entry.value.relay_last_offset = Some(last_offset);
    entry.recorded_at = Instant::now();
    true
}
