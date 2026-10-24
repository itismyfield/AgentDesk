use super::*;
use binding_events::{BindingPersistError, CauseSource, HookSignal, Proposal};
mod adopt_skip;
mod claude_source;
#[cfg(test)]
pub(crate) use claude_source::{AFTER_CHECK, BEFORE_AUTHORITY, after_check, before_authority};
pub(crate) use claude_source::{Persisted, Record, reclaim_with_current_prompt};
mod binding_access;
mod codex_cursor;
pub(crate) use codex_cursor::exports::*;
mod codex_policy;
pub(crate) use codex_policy::{
    clear_relaunch, codex_verified_channel_delivery_allowed, codex_verified_channel_requires_proof,
    codex_verified_event_allowed, codex_verified_hold_detail, codex_verified_hold_reason,
    codex_verified_input_blocked, codex_verified_o_source_allowed, codex_verified_requires_proof,
    codex_verified_requires_proof_under_source_authority, codex_verified_source_allowed,
    codex_verified_source_allowed_under_source_authority,
};
mod codex_hook;
pub use binding_access::register_provider_session;
use binding_access::same_runtime_source;
pub(crate) use binding_access::{
    RuntimeObservation, codex_verified_marker_metadata, codex_verified_publication_allowed,
    runtime_binding_for_tmux_session, runtime_binding_for_tmux_session_under_source_authority,
    runtime_bindings_for_kind, runtime_observation_under_source_authority,
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
struct RuntimeBindingPublication<'a, 'b> {
    target: &'a mut TuiPromptDedupeState,
    staged: TuiPromptDedupeState,
    previous: Option<TuiRuntimeBinding>,
    providers: std::collections::HashSet<PromptKey>,
    authority: &'a crate::services::tmux_common::TmuxSourceAuthority<'b>,
}

impl Drop for RuntimeBindingPublication<'_, '_> {
    fn drop(&mut self) {
        let session = self.authority.session();
        let current = self
            .staged
            .runtime_by_tmux
            .get(session)
            .map(|entry| &entry.value);
        let providers = self
            .staged
            .tmux_by_provider_session
            .iter()
            .filter(|(_, entry)| entry.value == session);
        let changed_providers = providers.clone().count() != self.providers.len()
            || providers
                .into_iter()
                .any(|(key, _)| !self.providers.contains(key));
        if changed_providers || !same_runtime_source(self.previous.as_ref(), current) {
            self.authority.invalidate();
        }
        std::mem::swap(self.target, &mut self.staged);
    }
}

fn with_runtime_binding_state_under_source_authority<R>(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    operation: impl FnOnce(&mut TuiPromptDedupeState) -> R,
) -> R {
    let tmux_session_name = authority.session();
    #[cfg(test)]
    let _publication = super::state::BindingPublicationHeld::enter();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    pane_registration::purge_expired_under_authority(&mut state, authority);
    let previous = state
        .runtime_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value.clone());
    let providers = state
        .tmux_by_provider_session
        .iter()
        .filter(|(_, entry)| entry.value == tmux_session_name)
        .map(|(key, _)| key.clone())
        .collect();
    let staged = std::mem::take(&mut *state);
    // The cache lock hides staged edits; Drop fences publication, including unwinding.
    let mut publication = RuntimeBindingPublication {
        target: &mut state,
        staged,
        previous,
        providers,
        authority,
    };
    operation(&mut publication.staged)
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

/// Find the newest provider-session mapping for this tmux pane without expiring that bridge.
/// Hook buffers use this provider key; callers fall back to the tmux name when absent.
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
            crate::services::tmux_common::write_tmux_channel_binding_under_source_authority(
                authority, channel_id,
            )
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
    if state
        .channel_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value)
        != Some(channel_id)
    {
        authority.invalidate();
    }
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
        if !same_runtime_source(replaced, Some(&binding)) {
            authority.invalidate();
        }
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

/// Return the expiry-based channel mirror for diagnostics and rehydration hints.
/// Relay routing uses the authoritative watcher registry instead of this cache.
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

/// Refresh only a live matching-channel submit anchor, without creating an anchor or purging other maps.
/// An expired matching anchor is removed; unrelated relay ledgers remain untouched on this hot path.
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

/// Remember completion until the matching submitted anchor appears, scoped to the nonzero turn generation.
/// A later turn cannot drain an older completion; recording the matching anchor drains it.
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
        pane_registration::forget_restore_scope(state, tmux_session_name);
        let removed_provider_sessions =
            state.remove_provider_session_mappings_for_tmux(tmux_session_name);
        removed_runtime || removed_provider_sessions
    });
    crate::services::claude_compact_context::clear_launch_provenance_for_tmux(tmux_session_name);
    crate::services::claude_compact_trigger::clear_for_tmux(tmux_session_name);
    removed
}

/// Forget both runtime and channel mirrors for an independently judged dead pane.
/// Return whether anything was removed; legitimate later registration can recreate the mirrors.
pub(crate) fn evict_dead_tmux_mirror(tmux_session_name: &str) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return false;
    }
    let removed = with_runtime_binding_authority(tmux_session_name, |state| {
        let removed_runtime = state.runtime_by_tmux.remove(tmux_session_name).is_some();
        pane_registration::forget_restore_scope(state, tmux_session_name);
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
mod s2a1_binding_tests {
    use super::*;
    use crate::services::tmux_common as tc;

    fn pin(session: &str) -> crate::services::cluster::stream_relay::source_read_proof::SourcePin {
        tc::with_tmux_source_authority(session, |authority| authority.pin()).expect("pin")
    }
    fn binding(path: &str) -> TuiRuntimeBinding {
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        }
    }
    #[test]
    fn s2a1_runtime_publish_read_offset_noop_clear_and_rehydrate_fence_identity() {
        let _host = crate::config::pin_runtime_host_for_test();
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        reset_state_for_tests();
        let session = "s2a1-runtime-binding";
        let before = pin(session);
        register_tmux_runtime_binding(session, binding("/s2a1-first"));
        assert!(!before.is_current());
        let same = pin(session);
        register_tmux_runtime_binding(session, binding("/s2a1-first"));
        assert!(same.is_current());
        assert!(runtime_binding_for_tmux_session(session).is_some());
        assert!(tc::with_tmux_source_authority(session, |authority| {
            advance_tmux_runtime_binding_offset_under_source_authority(authority, "/s2a1-first", 9)
        }));
        assert_eq!(
            runtime_binding_for_tmux_session(session)
                .expect("binding")
                .last_offset,
            9
        );
        assert!(same.is_current());
        register_tmux_runtime_binding(session, binding("/s2a1-next"));
        assert!(!same.is_current());
        let current = pin(session);
        assert!(clear_tmux_runtime_binding(session));
        assert!(!current.is_current());
        let cleared = pin(session);
        assert!(!clear_tmux_runtime_binding(session));
        assert!(cleared.is_current());
        assert!(register_rehydrated_tmux_runtime_binding(
            "claude",
            session,
            59_960_301,
            binding("/s2a1-next")
        ));
        assert!(!cleared.is_current());
        reset_state_for_tests();
    }
    #[test]
    fn s2a1_common_and_lock_held_expiry_invalidate_actual_removal_only() {
        let _host = crate::config::pin_runtime_host_for_test();
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        for under_authority in [false, true] {
            reset_state_for_tests();
            let session = if under_authority {
                "s2a1-expiry-held"
            } else {
                "s2a1-expiry-common"
            };
            register_tmux_runtime_binding(session, binding("/s2a1-expiry"));
            let before = pin(session);
            {
                let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
                let stamp =
                    state.aged_stamp_for_tests(SESSION_MAPPING_TTL + Duration::from_secs(1));
                state
                    .runtime_by_tmux
                    .get_mut(session)
                    .expect("binding")
                    .recorded_at = stamp;
            }
            if under_authority {
                assert!(runtime_binding_for_tmux_session(session).is_none());
            } else {
                assert!(runtime_bindings_for_kind(RuntimeHandoffKind::ClaudeTui).is_empty());
            }
            assert!(!before.is_current());
            let after = pin(session);
            assert!(runtime_binding_for_tmux_session(session).is_none());
            assert!(after.is_current());
        }
        reset_state_for_tests();
    }
}
