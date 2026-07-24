use super::*;

pub fn record_discord_originated_prompt(provider: &str, tmux_session_name: &str, prompt: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .pending_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default()
        .push_back(TimedValue {
            value: prompt.to_string(),
            recorded_at: Instant::now(),
        });
}

pub fn remove_discord_originated_prompt(provider: &str, tmux_session_name: &str, prompt: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let Some(queue) = state.pending_by_tmux.get_mut(&key) else {
        return;
    };
    if let Some(index) = queue
        .iter()
        .position(|pending| prompts_match(&pending.value, prompt))
    {
        queue.remove(index);
    }
    if queue.is_empty() {
        state.pending_by_tmux.remove(&key);
    }
}

pub fn observe_prompt_by_provider_session(
    provider: &str,
    provider_session_id: &str,
    prompt: &str,
) -> PromptObservation {
    observe_prompt_by_provider_session_at(provider, provider_session_id, prompt, Utc::now())
}

pub fn observe_prompt_by_provider_session_at(
    provider: &str,
    provider_session_id: &str,
    prompt: &str,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    let tmux_session_name = resolve_tmux_session_name(provider, provider_session_id)
        .unwrap_or_else(|| provider_session_id.trim().to_string());
    observe_prompt_by_tmux_at(provider, &tmux_session_name, prompt, observed_at)
}

pub fn observe_prompt_by_tmux(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> PromptObservation {
    observe_prompt_by_tmux_at(provider, tmux_session_name, prompt, Utc::now())
}

pub fn observe_prompt_by_tmux_at(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        &[prompt.to_string()],
        None,
        PromptObservationEffect::NotifyAndLease,
        observed_at,
    )
}

/// #3540: same as [`observe_prompt_by_tmux_at`] but carries the prompt's stable
/// JSONL entry identity (`uuid`). When `entry_id` is `Some` AND that uuid was
/// already relayed for this `(provider, tmux)` pair the call returns
/// [`PromptObservation::SuppressedReplayedEntry`] BEFORE any synthetic turn is
/// minted — closing the watermark-reset / jsonl-head-rotation re-claim window
/// that the 30s content dedup leaves open. `entry_id == None` falls back to the
/// pre-#3540 content-keyed path (no behavior change).
pub fn observe_prompt_by_tmux_with_entry_id_at(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
    entry_id: Option<&str>,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        &[prompt.to_string()],
        entry_id,
        PromptObservationEffect::NotifyAndLease,
        observed_at,
    )
}

pub fn observe_prompt_candidates_by_tmux(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        prompts,
        None,
        PromptObservationEffect::NotifyAndLease,
        Utc::now(),
    )
}

pub(crate) fn observe_prompt_candidates_by_tmux_for_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
) -> PromptObservation {
    observe_prompt_candidates_by_tmux_inner(
        provider,
        tmux_session_name,
        prompts,
        None,
        PromptObservationEffect::RelayLeaseOnly,
        Utc::now(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptObservationEffect {
    NotifyAndLease,
    RelayLeaseOnly,
}

fn observe_prompt_candidates_by_tmux_inner(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
    entry_id: Option<&str>,
    effect: PromptObservationEffect,
    observed_at: DateTime<Utc>,
) -> PromptObservation {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    let entry_id = entry_id.map(str::trim).filter(|value| !value.is_empty());
    let mut candidates = Vec::new();
    for prompt in prompts {
        let prompt = prompt.trim();
        // #3527: skip AgentDesk's own `[User: … (ID: …)]` Discord-relay lines so a
        // re-observation (after the discord-originated ledger entry was consumed)
        // never publishes a spurious SSH-direct turn. Treated like other synthetic
        // prompts → candidates stay empty → `PromptObservation::Ignored`.
        if prompt.is_empty()
            || is_synthetic_tui_user_prompt_for_provider(&provider, prompt)
            || (is_discord_relayed_user_prompt(prompt)
                && !is_user_prefixed_subagent_notification_machine_event(prompt))
        {
            continue;
        }
        if !candidates
            .iter()
            .any(|candidate: &String| prompts_match(candidate, prompt))
        {
            candidates.push(prompt.to_string());
        }
    }
    if provider.is_empty() || tmux_session_name.is_empty() || candidates.is_empty() {
        return PromptObservation::Ignored;
    }
    // #4567: structured task lifecycle records are status events, not positive
    // user-input provenance. Publish them for the task-card/status observer, but
    // deliberately bypass entry-id, pending, recent, lease, and SSH markers.
    if candidates
        .iter()
        .any(|prompt| is_start_anchored_task_notification_prompt(prompt))
    {
        let prompt = candidates
            .iter()
            .find(|prompt| is_start_anchored_task_notification_prompt(prompt))
            .expect("task notification candidate")
            .to_string();
        let event = ObservedTuiPrompt {
            provider,
            tmux_session_name: tmux_session_name.to_string(),
            prompt,
            source_event_id: entry_id.map(str::to_string),
            observed_at,
            external_input_lease_generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation: SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        };
        let _ = OBSERVED_PROMPTS.send(event);
        return PromptObservation::PublishedTaskNotification;
    }
    // #3540 (root cause): suppress by STABLE entry identity BEFORE any pending /
    // recent / lease bookkeeping or synthetic-turn mint. If this JSONL entry
    // `uuid` was already relayed for this `(provider, tmux)` pair it is a
    // re-encounter from a watermark reset / jsonl head rotation, NOT a new
    // submission — return early so the scanner never mints a phantom synthetic
    // inflight. This check inspects ONLY the relayed-entry ledger; it never
    // reads inflight / EOF / current_msg_id, so it cannot mis-handle a slow
    // genuine turn. A genuinely new prompt has a fresh uuid (absent from the
    // ledger) and is never suppressed here.
    if let Some(entry_id) = entry_id {
        if relayed_entry_id_already_seen(&provider, tmux_session_name, entry_id) {
            return PromptObservation::SuppressedReplayedEntry;
        }
    }
    let local_only_control = candidates
        .first()
        .and_then(|prompt| classify_local_only_slash_control(prompt));
    if local_only_control.is_none() {
        for prompt in &candidates {
            if take_matching_pending_prompt(&provider, tmux_session_name, prompt) {
                return PromptObservation::SuppressedDiscordDuplicate;
            }
        }
        for prompt in &candidates {
            if take_or_record_recent_observed_prompt(&provider, tmux_session_name, prompt) {
                return PromptObservation::SuppressedRecentDuplicate;
            }
        }
    }
    // Generic direct input keeps the #3540 eager identity record: it is a real
    // relay at this point. A local-only note has no durable side effect until
    // Discord accepts its note, so its id is recorded only by the successful
    // delivery branch in `tui_prompt_relay`.
    if local_only_control.is_none() {
        if let Some(entry_id) = entry_id {
            record_relayed_entry_id(&provider, tmux_session_name, entry_id);
        }
    }
    if effect == PromptObservationEffect::RelayLeaseOnly {
        if local_only_control.is_none() {
            record_external_input_turn_lease(
                &provider,
                tmux_session_name,
                ExternalInputRelayLease::unassigned(None),
            );
        }
        return PromptObservation::PublishedSshDirect;
    }
    let (external_input_lease_generation, ssh_direct_observation_generation) =
        if local_only_control.is_some() {
            (
                EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
                SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
            )
        } else {
            let external_input_lease = record_external_input_turn_lease(
                &provider,
                tmux_session_name,
                ExternalInputRelayLease::unassigned(None),
            );
            (
                external_input_lease.generation,
                mark_ssh_direct_observation_pending(&provider, tmux_session_name),
            )
        };
    let prompt = candidates
        .first()
        .expect("non-empty candidates")
        .to_string();
    let event = ObservedTuiPrompt {
        provider,
        tmux_session_name: tmux_session_name.to_string(),
        prompt,
        source_event_id: entry_id.map(str::to_string),
        observed_at,
        external_input_lease_generation,
        ssh_direct_observation_generation,
    };
    let _ = OBSERVED_PROMPTS.send(event);
    PromptObservation::PublishedSshDirect
}

pub(crate) fn record_external_input_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    channel_id: Option<u64>,
) {
    record_external_input_turn_lease(
        provider,
        tmux_session_name,
        ExternalInputRelayLease::unassigned(channel_id),
    );
}

/// Record an external-input relay lease for `(provider, tmux_session)` and return
/// the EXACT lease that was stored, including the unique `generation` stamped at
/// record time. Callers that need to later release THIS lease (e.g. an RAII
/// guard) MUST capture the returned value, not the pre-record argument: only the
/// returned value carries the recorded generation that
/// [`clear_external_input_relay_lease_if_matches`] /
/// [`clear_external_input_relay_lease_if_generation_matches`] compare against, so
/// a guard never clobbers a newer (even value-identical `Unassigned`) lease.
pub(crate) fn record_external_input_turn_lease(
    provider: &str,
    tmux_session_name: &str,
    mut lease: ExternalInputRelayLease,
) -> ExternalInputRelayLease {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return lease;
    }
    // Stamp a UNIQUE generation at the moment of record so two otherwise
    // value-equal leases for the same key are distinguishable by identity.
    lease.generation = next_external_input_relay_lease_generation();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.external_input_relay_lease_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: lease.clone(),
            recorded_at: Instant::now(),
        },
    );
    lease
}

pub(crate) fn external_input_relay_lease(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<ExternalInputRelayLease> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .external_input_relay_lease_by_tmux
        .get(&PromptKey::new(&provider, tmux_session_name))
        .and_then(|entry| match entry.value.channel_id {
            Some(leased) if leased != channel_id => None,
            _ => Some(entry.value.clone()),
        })
}

pub(crate) fn external_input_relay_lease_present(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> bool {
    external_input_relay_lease(provider, tmux_session_name, channel_id).is_some()
}

pub(crate) fn clear_external_input_relay_lease(
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
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.external_input_relay_lease_by_tmux.get(&key) else {
        return false;
    };
    if entry
        .value
        .channel_id
        .is_some_and(|leased| leased != channel_id)
    {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

pub(crate) fn clear_external_input_relay_lease_if_matches(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    expected: &ExternalInputRelayLease,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.external_input_relay_lease_by_tmux.get(&key) else {
        return false;
    };
    if entry
        .value
        .channel_id
        .is_some_and(|leased| leased != channel_id)
    {
        return false;
    }
    if &entry.value != expected {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

/// Compare-and-clear the external-input relay lease for `(provider, tmux_session)`
/// by its UNIQUE `generation` (and channel scope) rather than by full value.
///
/// This is the no-clobber primitive for the RAII release guards: the guard
/// captures the generation of the EXACT lease it observed/recorded and on Drop
/// clears only that generation. Two value-identical `Unassigned` leases (all
/// trace fields `None`) for the same key receive distinct generations at record
/// time, so an OLD guard's Drop leaves a NEWER lease — with a different
/// generation — untouched. A guard whose captured lease was never recorded
/// (generation == [`EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED`]) clears
/// nothing.
pub(crate) fn clear_external_input_relay_lease_if_generation_matches(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    expected_generation: u64,
) -> bool {
    if expected_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED {
        return false;
    }
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let Some(entry) = state.external_input_relay_lease_by_tmux.get(&key) else {
        return false;
    };
    if entry
        .value
        .channel_id
        .is_some_and(|leased| leased != channel_id)
    {
        return false;
    }
    if entry.value.generation != expected_generation {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

/// Compare-and-clear an external-input lease by generation without requiring a
/// channel binding. Direct prompt observation records an initially unassigned
/// lease before relay ownership has been resolved, so a consumed machine
/// control needs this exact unscoped cleanup path.
fn clear_external_input_relay_lease_if_generation_matches_unscoped(
    provider: &str,
    tmux_session_name: &str,
    expected_generation: u64,
) -> bool {
    if expected_generation == EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED {
        return false;
    }
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    if state
        .external_input_relay_lease_by_tmux
        .get(&key)
        .is_none_or(|entry| entry.value.generation != expected_generation)
    {
        return false;
    }
    state.external_input_relay_lease_by_tmux.remove(&key);
    true
}

fn mark_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) -> u64 {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED;
    }
    let generation = next_ssh_direct_observation_generation();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.ssh_direct_observation_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: generation,
            recorded_at: Instant::now(),
        },
    );
    generation
}

/// True when an SSH-direct prompt has been observed for this
/// `(provider, tmux_session)` pair within `SSH_DIRECT_OBSERVATION_TTL` and
/// the matching anchor has not yet been consumed. Watchers use this to keep
/// the post-terminal suppress guard from killing legitimate direct-input
/// responses during the brief window before `record_prompt_anchor` lands.
pub(crate) fn is_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .ssh_direct_observation_by_tmux
        .contains_key(&PromptKey::new(&provider, tmux_session_name))
}

pub(super) fn clear_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .ssh_direct_observation_by_tmux
        .remove(&PromptKey::new(&provider, tmux_session_name));
}

fn clear_ssh_direct_observation_pending_if_generation_matches(
    provider: &str,
    tmux_session_name: &str,
    expected_generation: u64,
) -> bool {
    if expected_generation == SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED {
        return false;
    }
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    if state
        .ssh_direct_observation_by_tmux
        .get(&key)
        .is_none_or(|entry| entry.value != expected_generation)
    {
        return false;
    }
    state.ssh_direct_observation_by_tmux.remove(&key);
    true
}

pub(crate) fn record_suppressed_discord_origin_prompt(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.record_recent_observed_prompt(&provider, tmux_session_name, prompt);
}
