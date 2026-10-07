use super::*;
use binding_events::{BindingPersistError, CauseSource, HookSignal, Proposal};
mod adopt_skip;
mod claude_source;
#[cfg(test)]
pub(crate) use claude_source::{AFTER_CHECK, BEFORE_AUTHORITY, after_check, before_authority};
pub(crate) use claude_source::{Persisted, Record, reclaim_with_current_prompt};
mod binding_access;
mod codex_cursor;
mod codex_policy;
pub(crate) use codex_cursor::{
    advance_tmux_runtime_binding_offset_under_source_authority,
    preservable_marker as codex_verified_preservable_marker,
};
pub(crate) use codex_policy::{
    codex_verified_channel_delivery_allowed, codex_verified_event_allowed,
    codex_verified_input_blocked, codex_verified_o_source_allowed, codex_verified_requires_proof,
    codex_verified_requires_proof_under_source_authority, codex_verified_source_allowed,
    codex_verified_source_allowed_under_source_authority,
};
mod codex_hook;
pub use binding_access::register_provider_session;
pub(crate) use binding_access::{
    codex_verified_marker_metadata, codex_verified_publication_allowed,
    runtime_binding_for_tmux_session, runtime_binding_for_tmux_session_under_source_authority,
    runtime_bindings_for_kind,
};
pub(crate) mod codex_verified;
#[cfg(test)]
pub(crate) use codex_hook::SHADOW_IO_CALLS;
pub(crate) use codex_hook::{
    codex_tail_source_retired, codex_tui_binding_is_subagent, observe_codex_hook,
    observe_codex_shadow, publish_unless_codex_tail_retired, register_codex_herdr_placeholder,
    register_launched_tmux_runtime_binding,
    register_launched_tmux_runtime_binding_under_source_authority,
};
#[cfg(test)]
pub(crate) use codex_verified::{
    DeliveryPermission as CodexDeliveryPermissionForTests,
    set_permission_for_tests as set_codex_delivery_permission_for_tests,
};
pub(crate) use codex_verified::{
    channel_allowed_under_source_authority as codex_verified_channel_allowed_under_source_authority,
    codex_verified_discovered_channel, observe_verified_codex_hook,
    recover_discovered_codex_binding, resolve_registered_claims as resolve_codex_claims,
};
pub(crate) mod pane_registration;
pub(crate) use adopt_skip::*;
pub(crate) use pane_registration::register_rehydrated_tmux_runtime_binding_under_source_authority;

fn with_runtime_binding_state_under_source_authority<R>(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    operation: impl FnOnce(&mut TuiPromptDedupeState) -> R,
) -> R {
    let tmux_session_name = authority.session();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.purge_expired_runtime_binding_under_authority(tmux_session_name);
    operation(&mut state)
}

fn with_runtime_binding_authority<R>(
    tmux_session_name: &str,
    operation: impl FnOnce(&mut TuiPromptDedupeState) -> R,
) -> R {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        with_runtime_binding_state_under_source_authority(authority, operation)
    })
}

pub fn subscribe_observed_prompts() -> broadcast::Receiver<ObservedTuiPrompt> {
    OBSERVED_PROMPTS.subscribe()
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
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        register_tmux_channel_under_source_authority(authority, channel_id);
    });
}

pub(crate) fn register_tmux_channel_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    channel_id: u64,
) {
    let tmux_session_name = authority.session();
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
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        register_tmux_runtime_binding_under_source_authority(authority, binding)
    });
}

pub(crate) fn register_tmux_runtime_binding_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    binding: TuiRuntimeBinding,
) -> bool {
    let cause = CauseSource::Observed;
    publish_runtime_binding(authority, binding, None, cause, Record::Stat)
        .is_some_and(Persisted::published)
}

/// Persists the binding event first; nothing is published on failure (`None`) or when a pin refuses.
fn publish_runtime_binding(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    binding: TuiRuntimeBinding,
    channel_id: Option<u64>,
    cause: CauseSource,
    record: Record,
) -> Option<Persisted> {
    let tmux_session_name = authority.session();
    if tmux_session_name.is_empty() || binding.output_path.trim().is_empty() {
        return None;
    }
    if binding.relay_output_path().trim().is_empty() {
        return None;
    }
    if codex_tui_binding_is_subagent(tmux_session_name, &binding) {
        return None;
    }
    if binding.runtime_kind == RuntimeHandoffKind::CodexTui {
        if !codex_verified::publication_allowed(authority, &binding) {
            return None;
        }
        match codex_verified::current_context(authority) {
            Ok(Some(_)) => {
                if !codex_verified::consumer_allowed(authority, &binding) {
                    return None;
                }
                let binding = codex_cursor::restore(authority, binding).ok()?;
                codex_cursor::persist_after_restore(authority, &binding).ok()?;
                with_runtime_binding_state_under_source_authority(authority, |state| {
                    state.runtime_by_tmux.insert(
                        tmux_session_name.to_owned(),
                        TimedValue {
                            value: binding,
                            recorded_at: Instant::now(),
                        },
                    );
                });
                return Some(Persisted::Logged);
            }
            Err(_) => return None,
            Ok(None) => {}
        }
    }
    with_runtime_binding_state_under_source_authority(authority, |state| {
        let channel_id = channel_id.or_else(|| {
            state
                .channel_by_tmux
                .get(tmux_session_name)
                .map(|e| e.value)
        });
        let replaced = state
            .runtime_by_tmux
            .get(tmux_session_name)
            .map(|e| &e.value);
        let proposal =
            Proposal::for_binding(channel_id, tmux_session_name, &binding, replaced, cause);
        let persisted = match proposal.map(|proposal| record.persist(&proposal)) {
            Some(Err(error)) => {
                tracing::error!(
                    tmux_session_name,
                    channel_id,
                    %error,
                    "binding event log append failed; runtime binding left unchanged"
                );
                return None;
            }
            Some(Ok(persisted)) if !persisted.published() => return Some(persisted),
            Some(Ok(persisted)) => persisted,
            None => record.unlogged(),
        };
        state.runtime_by_tmux.insert(
            tmux_session_name.to_string(),
            TimedValue {
                value: binding,
                recorded_at: Instant::now(),
            },
        );
        Some(persisted)
    })
}

pub(crate) fn register_rehydrated_tmux_runtime_binding(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    binding: TuiRuntimeBinding,
) -> bool {
    #[rustfmt::skip]
    let registered = crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| register_rehydrated_tmux_runtime_binding_under_source_authority(authority, provider, channel_id, binding));
    registered
}

/// `None` when nothing was published; otherwise what the record left, published only if it says so.
fn register_rehydrated_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    provider: &str,
    channel_id: u64,
    binding: TuiRuntimeBinding,
    record: Record,
    cause: CauseSource,
) -> Option<Persisted> {
    let provider = normalize_provider(provider);
    let tmux_session_name = authority.session();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || channel_id == 0
        || binding.output_path.trim().is_empty()
        || binding.relay_output_path().trim().is_empty()
    {
        return None;
    }
    if !codex_verified::publication_allowed(authority, &binding) {
        return None;
    }
    let binding = codex_hook::restored_source(authority, &provider, channel_id, binding)?;
    let session_id = binding.session_id.clone();
    let persisted = publish_runtime_binding(authority, binding, Some(channel_id), cause, record)?;
    if !persisted.published() {
        return Some(persisted);
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
    Some(persisted)
}

/// Resolve and optionally replace one binding while its source authority stays held.
#[rustfmt::skip]
pub(crate) fn reconcile_rehydrated_tmux_runtime_binding(
    provider: &str, tmux_session_name: &str, channel_id: u64,
    observe_before_replace: impl FnOnce(),
    decide: impl FnOnce(Option<TuiRuntimeBinding>) -> Option<(TuiRuntimeBinding, bool)>,
) -> Option<TuiRuntimeBinding> {
    reconcile_rehydrated_tmux_runtime_binding_with_authority(
        provider, tmux_session_name, channel_id, observe_before_replace,
        |_, existing| decide(existing),
    )
}

#[rustfmt::skip]
pub(crate) fn reconcile_rehydrated_tmux_runtime_binding_with_authority(
    provider: &str, tmux_session_name: &str, channel_id: u64,
    observe_before_replace: impl FnOnce(),
    decide: impl FnOnce(&crate::services::tmux_common::TmuxSourceAuthority<'_>, Option<TuiRuntimeBinding>) -> Option<(TuiRuntimeBinding, bool)>,
) -> Option<TuiRuntimeBinding> {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        let (binding, replace) = decide(authority, runtime_binding_for_tmux_session_under_source_authority(authority))?;
        if replace {
            observe_before_replace();
            // An unpublished binding reports none; a published one reports what restore stored.
            if !register_rehydrated_tmux_runtime_binding_under_source_authority(authority, provider, channel_id, binding.clone()) {
                return None;
            }
            return runtime_binding_for_tmux_session_under_source_authority(authority);
        }
        codex_verified::consumer_allowed(authority, &binding).then_some(binding)
    })
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
    Instant::reset();
}

/// Test-only: when the runtime binding for `tmux_session_name` was last written.
#[cfg(test)]
pub(crate) fn runtime_binding_recorded_at_for_tests(tmux_session_name: &str) -> Option<Instant> {
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .runtime_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.recorded_at)
}

/// Records an aged anchor using a forward-moving test clock, preserving other records' ages.
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
    let recorded_at = state.aged_stamp_for_tests(age);
    state.prompt_anchor_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: TuiPromptAnchor {
                channel_id,
                message_id,
            },
            recorded_at,
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

/// Adopt the actual Claude session UUID reported inside a hook payload while
/// retaining the launch-time UUID as a stable hook-routing alias (#4423).
///
/// Claude continuation keeps the tmux process and hook command alive but moves
/// transcript writes to a new `<uuid>.jsonl`.  The hook command therefore still
/// addresses the old UUID while stdin carries the new one.  This update is
/// deliberately limited to an existing ClaudeTui binding reached through the
/// command UUID and to the payload's own transcript, which must pass the Claude
/// source check under the bound transcript's projects root; a session the pane
/// left comes back only on a hook published after the pane's last transition.
pub(crate) fn adopt_claude_continuation_session(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
) -> Result<Option<(String, String)>, BindingPersistError> {
    adopt_claude_continuation_explained(command_session_id, payload_session_id, hook)
        .map(|(adopted, _)| adopted)
}

/// `failure` is set only when the adoption was decided but its binding event could not be persisted.
/// `skip` names why nothing was adopted or logged; each check below sets it before it can return.
fn adopt_continuation(
    command_session_id: &str,
    payload_session_id: &str,
    hook: &HookSignal,
    failure: &mut Option<BindingPersistError>,
    skip: &mut Option<AdoptSkip>,
) -> Option<(String, String)> {
    let command_session_id = command_session_id.trim();
    let payload_session_id = payload_session_id.trim();
    *skip = Some(AdoptSkip::PayloadNotUuid);
    let same = command_session_id == payload_session_id;
    #[cfg(test)]
    let same = same && !crate::services::claude_tui::source_verify::n2b_mutant("r5-ingress-start");
    if command_session_id.is_empty()
        || payload_session_id.is_empty()
        || same
        || uuid::Uuid::parse_str(payload_session_id).is_err()
    {
        return None;
    }
    *skip = Some(AdoptSkip::PayloadPathMissing);
    let payload_path = hook.transcript_path.as_deref().map(str::trim);
    let payload_path = PathBuf::from(payload_path.filter(|path| !path.is_empty())?);
    // Up to 1 MiB of first line is read before the binding state is locked.
    let opened = crate::services::claude_tui::source_verify::observe_transcript(&payload_path);

    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let command_key = PromptKey::new("claude", command_session_id);
    let (tmux_session_name, binding) = AdoptSkip::bound_pane(&state, &command_key, skip)?;
    *skip = Some(AdoptSkip::NotClaudeTui);
    if binding.value.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return None;
    }
    *skip = Some(AdoptSkip::MalformedBindingPath);
    let old_output_path = PathBuf::from(&binding.value.output_path);
    // Projects root of the account this pane launched under, not the receiver's own.
    let root = old_output_path.parent()?.parent()?;
    let new_output_path =
        crate::services::claude_tui::source_verify::normalize_payload_path(root, &payload_path);
    let (candidate, replaced) = (new_output_path.display().to_string(), binding.value.clone());
    let hook = &HookSignal {
        transcript_path: Some(candidate.clone()),
        ..hook.clone()
    };
    let channel_id = state
        .channel_by_tmux
        .get(&tmux_session_name)
        .map(|e| e.value);
    let proposal = channel_id.filter(|id| *id != 0).map(|channel_id| Proposal {
        channel_id,
        provider: "claude",
        tmux_session: &tmux_session_name,
        session_id: Some(payload_session_id),
        path: &candidate,
        replaced: Some((&replaced.output_path, replaced.session_id.as_deref())),
        cause: CauseSource::Hook(hook.cause()),
        hook: Some(hook),
    });
    let judged = claude_source::Candidate {
        proposal,
        tmux_session: &tmux_session_name,
        bound: &replaced,
        root,
        command_session_id,
        payload_session_id,
        hook,
        opened: &opened,
    };
    if !judged.judge(failure, skip) {
        return None;
    }
    let new_output_path = candidate;
    let same_session = binding.value.session_id.as_deref() == Some(payload_session_id)
        && binding.value.output_path == new_output_path;
    if same_session {
        // Subsequent hooks still carry the launch-time query UUID. Do not reset
        // the already-adopted continuation cursor to zero on every event.
        //
        // #5188 (P1-A): but DO refresh the adopted-session authority. This is
        // the branch every hook after the first one takes, so leaving it out
        // made the authority a write-once fact tied to a single instant instead
        // of a standing statement about what this pane is running now.
        super::session_rotation::record_hook_adopted_claude_session_id(
            &tmux_session_name,
            payload_session_id,
        );
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
    // #5188 (R1/R2): record the rotation BEFORE the fields are overwritten. The
    // pre-rotation transcript and cursor are the only evidence of what may still
    // be owed to Discord, and the adopted session id is what stops the
    // launch-script rehydration pass from reverting this binding to the frozen
    // file. Without this, adoption stayed a purely local mutation: the log said
    // "adopted" while delivery kept reading a transcript that never grew again.
    super::session_rotation::record_claude_session_rotation(
        super::session_rotation::ClaudeSessionRotation {
            tmux_session_name: tmux_session_name.clone(),
            old_session_id: binding.value.session_id.clone(),
            old_output_path: binding.value.output_path.clone(),
            old_last_offset: binding.value.relay_last_offset(),
            new_session_id: payload_session_id.to_string(),
            new_output_path: new_output_path.clone(),
            observed_drain_frontier: 0,
            polls_without_drain_progress: 0,
        },
    );
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
    with_runtime_binding_authority(tmux_session_name, |state| {
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
    })
}

pub(crate) fn clear_tmux_runtime_binding(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let removed = with_runtime_binding_authority(tmux_session_name, |state| {
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_provider_sessions
    });
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
    let removed = with_runtime_binding_authority(tmux_session_name, |state| {
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        let removed_channel = state.channel_by_tmux.remove(tmux_session_name).is_some();
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_channel || removed_provider_sessions
    });
    crate::services::claude_compact_context::clear_launch_provenance_for_tmux(tmux_session_name);
    crate::services::claude_compact_trigger::clear_for_tmux(tmux_session_name);
    removed
}

#[cfg(test)]
mod codex_legacy_lock_tests;
#[cfg(test)]
mod codex_verified_tests;

/// Live sessions of `kinds` with their cached owner channel, copied without purging relay state;
/// `None` when the lock is busy.
pub(crate) fn peek_tui_session_channels(
    kinds: &[RuntimeHandoffKind],
) -> Option<Vec<(String, u64)>> {
    let state = match STATE.try_lock() {
        Ok(state) => state,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    let live = |at: &Instant| at.elapsed() <= SESSION_MAPPING_TTL;
    let sessions = state
        .runtime_by_tmux
        .iter()
        .filter(|(_, entry)| kinds.contains(&entry.value.runtime_kind) && live(&entry.recorded_at));
    let owned = sessions.filter_map(|(tmux, _)| {
        let owner = state.channel_by_tmux.get(tmux)?;
        live(&owner.recorded_at).then(|| (tmux.clone(), owner.value))
    });
    Some(owned.collect())
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
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        advance_tmux_runtime_binding_offset_under_source_authority(
            authority,
            output_path,
            last_offset,
        )
    })
}

#[cfg(test)]
mod shadow_session_tests {
    use super::*;

    /// Every map `purge_expired` prunes, rendered so any removed or refreshed entry shows.
    fn relay_state() -> Vec<String> {
        let state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut rows: Vec<String> = Vec::new();
        rows.extend(
            state
                .channel_by_tmux
                .iter()
                .map(|e| format!("channel {e:?}")),
        );
        rows.extend(
            state
                .runtime_by_tmux
                .iter()
                .map(|e| format!("runtime {e:?}")),
        );
        rows.extend(
            state
                .pending_by_tmux
                .iter()
                .map(|e| format!("pending {e:?}")),
        );
        let leases = state.external_input_relay_lease_by_tmux.iter();
        rows.extend(leases.map(|e| format!("lease {e:?}")));
        let relayed = state.relayed_entry_ids_by_tmux.iter();
        rows.extend(relayed.map(|e| format!("relayed {e:?}")));
        rows.sort();
        rows
    }

    #[test]
    fn observer_discovery_lists_live_sessions_without_changing_relay_state() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_state_for_tests();
        let expired = {
            let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
            state.aged_stamp_for_tests(SESSION_MAPPING_TTL + Duration::from_secs(1))
        };
        let binding = |runtime_kind| TuiRuntimeBinding {
            runtime_kind,
            output_path: "/t/s.jsonl".into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        {
            let mut state = STATE.lock().unwrap_or_else(|poison| poison.into_inner());
            let rows = [
                (
                    "live",
                    7,
                    RuntimeHandoffKind::ClaudeTui,
                    Instant::now(),
                    Instant::now(),
                ),
                (
                    "other",
                    9,
                    RuntimeHandoffKind::CodexTui,
                    Instant::now(),
                    Instant::now(),
                ),
                (
                    "stale-binding",
                    7,
                    RuntimeHandoffKind::CodexTui,
                    expired,
                    Instant::now(),
                ),
                (
                    "stale-owner",
                    7,
                    RuntimeHandoffKind::ClaudeTui,
                    Instant::now(),
                    expired,
                ),
            ];
            for (name, channel, kind, bound_at, owned_at) in rows {
                let value = binding(kind);
                let runtime = TimedValue {
                    value,
                    recorded_at: bound_at,
                };
                state.runtime_by_tmux.insert(name.to_string(), runtime);
                let owner = TimedValue {
                    value: channel,
                    recorded_at: owned_at,
                };
                state.channel_by_tmux.insert(name.to_string(), owner);
            }
            let victim = PromptKey::new("claude", "stale-binding");
            let pending = TimedValue {
                value: "p".to_string(),
                recorded_at: expired,
            };
            state
                .pending_by_tmux
                .insert(victim.clone(), VecDeque::from([pending]));
            let lease = ExternalInputRelayLease::unassigned(Some(7));
            let lease = TimedValue {
                value: lease,
                recorded_at: expired,
            };
            state
                .external_input_relay_lease_by_tmux
                .insert(victim.clone(), lease);
            let entry = TimedValue {
                value: "e".to_string(),
                recorded_at: expired,
            };
            state
                .relayed_entry_ids_by_tmux
                .insert(victim, VecDeque::from([entry]));
        }
        let before = relay_state();
        let targets = crate::services::tui_o::shadow_host::discover_targets(&[7]);
        let after = relay_state();
        reset_state_for_tests();
        let listed: Vec<(u64, &str)> = targets
            .iter()
            .map(|t| (t.channel_id, t.tmux_session.as_str()))
            .collect();
        assert_eq!(listed, [(7, "live")]);
        assert_eq!(
            (before.len(), &after),
            (11, &before),
            "discovery must not touch relay state"
        );
    }
}
