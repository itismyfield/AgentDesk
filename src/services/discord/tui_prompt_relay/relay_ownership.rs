use super::*;

pub(super) struct TuiDirectExternalInputLeaseGuard {
    provider: ProviderKind,
    tmux_session_name: String,
    channel_id: ChannelId,
    lease: ExternalInputRelayLease,
    active: bool,
}

impl TuiDirectExternalInputLeaseGuard {
    pub(super) fn new(
        provider: ProviderKind,
        tmux_session_name: &str,
        channel_id: ChannelId,
        lease: &ExternalInputRelayLease,
    ) -> Self {
        Self {
            provider,
            tmux_session_name: tmux_session_name.to_string(),
            channel_id,
            lease: lease.clone(),
            active: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.active = false;
    }

    fn clear_if_current(&self) -> bool {
        clear_external_input_bridge_lease_if_current(
            &self.provider,
            &self.tmux_session_name,
            self.channel_id,
            &self.lease,
        )
    }
}

impl Drop for TuiDirectExternalInputLeaseGuard {
    fn drop(&mut self) {
        // Match the exact lease so a slow timeout cannot clear a newer direct-input turn
        // that reused the same provider/session/channel after this tail started.
        if self.active {
            self.clear_if_current();
        }
    }
}

/// Early-return RAII guard for [`relay_observed_prompt`]: armed right after
/// [`record_observed_external_turn_lease`] records & stores a (possibly
/// `BridgeAdapter`-owned, hence delivery-blocking) lease, it clears that exact
/// lease BY GENERATION on every early-return between the record and the point
/// where the bridge legitimately takes ownership of the in-flight turn.
///
/// WHY clear-by-generation (not by full value): a NEWER turn may have re-taken
/// the same `(provider, tmux_session, channel)` lease while this relay was awaiting
/// the notify HTTP resolve / Discord POST; a by-key or by-value clear could clobber
/// that newer lease (the exact no-clobber race the per-record generation nonce was
/// added to kill, #3041 P1-4 codex). Clearing only the captured generation leaves a
/// newer (even value-identical `Unassigned`) lease untouched.
///
/// SUCCESS-PATH PERSISTENCE: on the path where the bridge posts the card/anchor and
/// retains the in-flight turn, the lease MUST persist so the watcher/sink does not
/// double-deliver. The caller therefore [`disarm`](Self::disarm)s this guard right
/// before the bridge-tail ownership block; only the genuinely-aborting early-returns
/// (registry None, notify resolve `Err`/503, task-card repeat, anchor POST failure)
/// leave the guard armed → its clear releases the lease so legitimate delivery can
/// proceed.
pub(super) struct TuiDirectObservedLeaseEarlyReturnGuard {
    provider: Option<ProviderKind>,
    tmux_session_name: String,
    channel_id: ChannelId,
    /// Generation of the lease this guard armed with; Drop clears ONLY this exact
    /// generation (sentinel `UNRECORDED` clears nothing).
    generation: u64,
    active: bool,
}

impl TuiDirectObservedLeaseEarlyReturnGuard {
    /// Arm a guard capturing the generation of the just-recorded `lease`. When the
    /// provider string is not a known [`ProviderKind`] the guard is inert (no key to
    /// clear by) but still constructed so callers keep a uniform disarm point.
    pub(super) fn arm(
        provider_str: &str,
        tmux_session_name: &str,
        channel_id: ChannelId,
        generation: u64,
    ) -> Self {
        Self {
            provider: ProviderKind::from_str(provider_str),
            tmux_session_name: tmux_session_name.to_string(),
            channel_id,
            generation,
            active: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TuiDirectObservedLeaseEarlyReturnGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        // Compare-and-clear by the captured generation: a newer same-key lease
        // recorded during a slow notify-resolve / POST await has a DIFFERENT
        // generation and survives this drop (no clobber).
        crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_generation_matches(
            provider.as_str(),
            &self.tmux_session_name,
            self.channel_id.get(),
            self.generation,
        );
    }
}

pub(super) fn clear_external_input_bridge_lease_if_current(
    provider: &ProviderKind,
    tmux_session_name: &str,
    channel_id: ChannelId,
    lease: &ExternalInputRelayLease,
) -> bool {
    if !bridge_adapter_owns_external_turn(lease.relay_owner) {
        return false;
    }
    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
        lease,
    )
}

pub(super) fn record_observed_external_turn_lease(
    shared: &Arc<SharedData>,
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
) -> ExternalInputRelayLease {
    let provider = ProviderKind::from_str(&prompt.provider);
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &prompt.tmux_session_name,
    );
    let runtime_kind = binding.as_ref().map(|binding| binding.runtime_kind);
    let relay_output_path = external_input_relay_output_path(
        shared,
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id,
        binding.as_ref(),
    );
    let relay_owner = external_input_relay_owner_for_output(
        shared,
        &prompt.tmux_session_name,
        relay_output_path.as_deref(),
    );
    let session_key = provider.as_ref().map(|provider| {
        super::super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            provider,
            &prompt.tmux_session_name,
        )
    });
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some(external_input_turn_id(
            &prompt.provider,
            channel_id,
            &prompt.tmux_session_name,
            prompt.observed_at,
        )),
        session_key,
        relay_owner,
        runtime_kind,
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    // Capture the RECORDED lease (with its stamped generation) so the caller's
    // later `clear_observed_external_turn_lease_if_current` matches the exact
    // stored identity and never clobbers a newer turn's lease.
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        &prompt.provider,
        &prompt.tmux_session_name,
        lease,
    );
    tracing::info!(
        provider = %prompt.provider,
        channel_id = channel_id.get(),
        tmux_session_name = %prompt.tmux_session_name,
        turn_id = lease.turn_id.as_deref().unwrap_or(""),
        session_key = lease.session_key.as_deref().unwrap_or(""),
        relay_owner = lease.relay_owner.as_str(),
        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
        "observed TUI-direct input as already-submitted external turn"
    );
    lease
}

/// Clear the external-input turn lease recorded by
/// [`record_observed_external_turn_lease`] for THIS observation, if it is still
/// the current lease (exact match).
///
/// Used by the `<task-notification>` edit-repeat path (#3075 codex P1 #2): a
/// repeat records a fresh lease before card resolution but then early-returns,
/// skipping the normal bridge-tail / lease-guard cleanup. Without this, that
/// stale non-`Unassigned` lease would block session-bound / bridge-tail delivery
/// (`session_relay_sink::session_bound_external_lease_blocks_delivery`). The
/// exact-match guard means a newer turn that reused the same
/// provider/session/channel after we recorded ours is left untouched.
pub(super) fn clear_observed_external_turn_lease_if_current(
    prompt: &ObservedTuiPrompt,
    channel_id: ChannelId,
    lease: &ExternalInputRelayLease,
) -> bool {
    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id.get(),
        lease,
    )
}

pub(super) fn external_input_relay_binding(
    provider: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let binding = binding?;
    #[cfg(unix)]
    {
        if provider
            .trim()
            .eq_ignore_ascii_case(ProviderKind::Codex.as_str())
            && binding.runtime_kind == RuntimeHandoffKind::CodexTui
            && let Some(fresh) =
                resolved_codex_idle_relay_binding(tmux_session_name, channel_id, &binding)
        {
            return Some(fresh);
        }
    }
    Some(binding)
}

#[cfg(unix)]
pub(super) fn resolved_codex_idle_relay_binding(
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let marker =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name);
    if let Some(marker) = marker
        && marker.rollout_path.exists()
    {
        let marker_path = std::fs::canonicalize(&marker.rollout_path)
            .unwrap_or_else(|_| marker.rollout_path.clone());
        let binding_path = std::fs::canonicalize(&binding.output_path)
            .unwrap_or_else(|_| PathBuf::from(&binding.output_path));
        if marker_path != binding_path {
            let fresh = codex_tui_rehydrated_binding_from_rollout_path(
                tmux_session_name,
                &marker.rollout_path,
                marker.session_id,
            )?;
            crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                ProviderKind::Codex.as_str(),
                tmux_session_name,
                channel_id.get(),
                fresh.clone(),
            );
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                channel_id = channel_id.get(),
                stale_output_path = %binding.output_path,
                rollout_path = %fresh.output_path,
                "refreshed Codex TUI direct relay binding from live rollout marker"
            );
            return Some(fresh);
        }
    }
    Path::new(&binding.output_path)
        .exists()
        .then(|| binding.clone())
}

pub(super) fn external_input_relay_output_path(
    shared: &Arc<SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<PathBuf> {
    let binding = binding?;
    #[cfg(unix)]
    {
        if provider
            .trim()
            .eq_ignore_ascii_case(ProviderKind::Claude.as_str())
            && binding.runtime_kind == RuntimeHandoffKind::ClaudeTui
            && let Some(transcript_path) = resolved_claude_idle_relay_transcript_path(
                shared,
                tmux_session_name,
                channel_id,
                binding,
            )
        {
            return Some(transcript_path);
        }
        if provider
            .trim()
            .eq_ignore_ascii_case(ProviderKind::Codex.as_str())
            && binding.runtime_kind == RuntimeHandoffKind::CodexTui
        {
            return Some(PathBuf::from(&binding.output_path));
        }
    }
    Some(PathBuf::from(binding.relay_output_path()))
}

pub(super) fn external_input_relay_start_offset(
    provider: &ProviderKind,
    binding: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> u64 {
    let Some(binding) = binding else {
        return 0;
    };
    if provider == &ProviderKind::Codex && binding.runtime_kind == RuntimeHandoffKind::CodexTui {
        return binding.last_offset;
    }
    binding.relay_last_offset()
}

pub(super) fn record_external_turn_lease_for_output(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    runtime_kind: RuntimeHandoffKind,
    output_path: &Path,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> ExternalInputRelayLease {
    let relay_owner =
        external_input_relay_owner_for_output(shared, tmux_session_name, Some(output_path));
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some(external_input_turn_id(
            provider.as_str(),
            channel_id,
            tmux_session_name,
            observed_at,
        )),
        session_key: Some(super::super::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            provider,
            tmux_session_name,
        )),
        relay_owner,
        runtime_kind: Some(runtime_kind),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    // Return the RECORDED lease (with its stamped generation) so a later
    // exact-match clear targets the precise stored identity.
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        provider.as_str(),
        tmux_session_name,
        lease,
    )
}

pub(super) fn external_input_turn_id(
    provider: &str,
    channel_id: ChannelId,
    tmux_session_name: &str,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "external:{}:{}:{}:{}",
        provider.trim(),
        channel_id.get(),
        tmux_session_name.trim(),
        observed_at.timestamp_millis()
    )
}

pub(super) fn external_input_relay_owner_for_output(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: Option<&Path>,
) -> ExternalInputRelayOwner {
    external_input_relay_owner_for_watchers(
        &shared.tmux_watchers,
        tmux_session_name,
        output_path,
        session_bound_discord_delivery_enabled(),
    )
}

pub(super) fn session_bound_discord_delivery_enabled() -> bool {
    #[cfg(unix)]
    {
        super::super::session_relay_sink::session_bound_discord_delivery_enabled()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(super) fn external_input_relay_owner_for_watchers(
    watchers: &super::super::TmuxWatcherRegistry,
    tmux_session_name: &str,
    output_path: Option<&Path>,
    session_bound_discord_delivery_enabled: bool,
) -> ExternalInputRelayOwner {
    let watcher_alive = watchers
        .tmux_session_live_for_relay(tmux_session_name)
        .is_some_and(|live| live);
    if !watcher_alive {
        return ExternalInputRelayOwner::BridgeAdapter;
    }

    let watcher_covers_output = match output_path {
        Some(output_path) => watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == output_path),
        None => true,
    };
    if !watcher_covers_output {
        return ExternalInputRelayOwner::BridgeAdapter;
    }

    if session_bound_discord_delivery_enabled {
        // TUI-direct observations do not create a foreground inflight row yet.
        // A session-bound StreamRelay can only be the terminal owner for an
        // external-input turn once such an inflight exists; otherwise the
        // watcher can acknowledge frames without a Discord terminal commit.
        ExternalInputRelayOwner::BridgeAdapter
    } else {
        ExternalInputRelayOwner::TmuxWatcher
    }
}

pub(super) fn bridge_adapter_owns_external_turn(owner: ExternalInputRelayOwner) -> bool {
    matches!(owner, ExternalInputRelayOwner::BridgeAdapter)
}

/// #3154 P1-3 no-relay-GAP guard: may the OBSERVER loop spawn its own BridgeAdapter
/// idle-response tail? The output must come from EXACTLY ONE owner (never a GAP, never
/// a DUPLICATE). DEFERRED ⇒ the observer cannot yet know the RESOLVED owner (the claim
/// runs later in the detached worker), so it STANDS DOWN unconditionally and the worker
/// re-runs [`deferred_claim_requires_bridge_tail_relayer`] against the resolved owner.
/// NOT deferred ⇒ spawn iff the lease still owns as BridgeAdapter (the inline claim
/// already adopted any watcher handoff, so a watcher-owned lease means the observer
/// stands down). Pairing this with the worker's owner-kind-aware spawn is the proof.
pub(super) fn observer_should_spawn_bridge_tail(
    deferred_synthetic_start: bool,
    lease_owner: ExternalInputRelayOwner,
) -> bool {
    !deferred_synthetic_start && bridge_adapter_owns_external_turn(lease_owner)
}

/// #3154 P1 (BridgeAdapter-GAP fix). The OWNER-KIND-AWARE decision the deferred
/// worker runs AFTER its claim resolves the relay owner, mirroring the inline path:
/// TmuxWatcher ⇒ the watcher relays so the bridge tail STANDS DOWN (else DUPLICATE);
/// BridgeAdapter ⇒ no watcher relays and the observer already stood down, so the
/// bridge tail MUST run exactly once here (else `relayer_count == 0`, the GAP). The
/// downstream [`maybe_spawn_claude_idle_response_tail`] re-checks
/// `bridge_adapter_owns_external_turn`, so a stale/watcher lease can never spawn a
/// second relayer even if this predicate were called too eagerly.
pub(super) fn deferred_claim_requires_bridge_tail_relayer(
    resolved_owner: ExternalInputRelayOwner,
) -> bool {
    bridge_adapter_owns_external_turn(resolved_owner)
}

/// #3154 P1-3 relay-owner adoption decision. The in-memory lease adopts the claim's
/// `relay_owner` iff the claim SUCCEEDED and the owner changed; re-recording with
/// the claimed (watcher) owner makes [`observer_should_spawn_bridge_tail`] read a
/// watcher-owned lease and stand down (the claimed owner is the SINGLE relayer, no
/// GAP/dup). Shared by the inline and deferred paths so both adopt identically.
pub(super) fn claim_should_adopt_relay_owner(
    claimed: bool,
    current_owner: ExternalInputRelayOwner,
    claimed_owner: ExternalInputRelayOwner,
) -> bool {
    claimed && current_owner != claimed_owner
}

pub(super) fn parse_external_input_relay_owner(value: &str) -> ExternalInputRelayOwner {
    match value {
        "bridge_adapter" => ExternalInputRelayOwner::BridgeAdapter,
        "tui_prompt_relay" => ExternalInputRelayOwner::TuiPromptRelay,
        "tmux_watcher" => ExternalInputRelayOwner::TmuxWatcher,
        "session_bound_relay" => ExternalInputRelayOwner::SessionBoundRelay,
        _ => ExternalInputRelayOwner::Unassigned,
    }
}
