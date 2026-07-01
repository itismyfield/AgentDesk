use super::*;

#[derive(Debug)]
pub(super) struct TuiDirectSyntheticTurnClaim {
    pub(super) relay_owner: ExternalInputRelayOwner,
    pub(super) claimed: bool,
    // #3154 P1 (timestamp-anchor output loss): the post-drain EOF offset the claim
    // seeded into this turn's inflight `turn_start_offset`. The deferred-BridgeAdapter
    // worker anchors its bridge tail to THIS byte boundary instead of a `Utc::now()`
    // scan, which can skip bytes written during the deferred-claim wait window.
    pub(super) turn_start_offset: u64,
}

pub(super) async fn finish_tui_direct_synthetic_pre_save_failure(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    // This cleanup runs before the synthetic path increments global_active.
    let _ = super::super::mailbox_finish_turn(shared, provider, channel_id).await;
}

/// #3358 — offset-authority handover for synthetic inflight creation.
///
/// A synthetic is born at the lagging `relay_last_offset()`; when that lags the
/// watcher's delivered frontier (#3017), a later same-identity re-claim re-seeds
/// the row backward → trips the monotonicity guards (the incident). CARRY-FORWARD
/// the frontier so the synthetic is born at/above every delivered byte.
///
/// #3358 round 2 — GATED: `committed_relay_offset` is `Some` ONLY when the gating
/// accessor proved the watermark belongs to the CURRENT wrapper. After a restart
/// the stream resets to 0; a stale PREVIOUS-generation watermark must NOT clamp
/// forward — that marks future bytes below it as delivered → CONTENT SKIP (worse
/// than the original ERROR-only bug). On mismatch the frontier is `None` and we
/// fall back to pre-fix seeding (`relay_last_offset` only): the rare monotonicity
/// ERROR beats a skip, and backward writes outside this handover stay guarded.
pub(in crate::services::discord) fn synthetic_start_offset_carry_forward(
    relay_last_offset: u64,
    committed_relay_offset: Option<u64>,
) -> u64 {
    relay_last_offset.max(committed_relay_offset.unwrap_or(0))
}

pub(super) async fn claim_tui_direct_synthetic_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    prompt_text: &str,
    anchor_message_id: MessageId,
    lease: &ExternalInputRelayLease,
) -> TuiDirectSyntheticTurnClaim {
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    let binding =
        external_input_relay_binding(provider.as_str(), tmux_session_name, channel_id, binding);
    let output_path = external_input_relay_output_path(
        shared,
        provider.as_str(),
        tmux_session_name,
        channel_id,
        binding.as_ref(),
    );
    let relay_last_offset = external_input_relay_start_offset(provider, binding.as_ref());
    // #3358 round 2: carry the committed frontier forward, but ONLY for the
    // CURRENT wrapper generation (stale → `None` → no content skip).
    // The `tmux` module is `#[cfg(unix)]`; on non-unix targets (windows CI
    // cross-compile check) there is no committed frontier to carry forward, so
    // `None` (no carry-forward) is the correct, behavior-preserving default.
    #[cfg(unix)]
    let committed_relay_offset = super::super::tmux::committed_frontier_for_current_generation(
        shared,
        channel_id,
        tmux_session_name,
    );
    #[cfg(not(unix))]
    let committed_relay_offset: Option<u64> = None;
    let start_offset =
        synthetic_start_offset_carry_forward(relay_last_offset, committed_relay_offset);
    if start_offset > relay_last_offset {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            anchor_message_id = anchor_message_id.get(),
            relay_last_offset,
            committed_relay_offset = committed_relay_offset.unwrap_or(0),
            start_offset,
            "#3358 synthetic inflight offset-authority handover: carried committed relay frontier forward"
        );
    }
    // #3876 (codex rework): gate the SessionBoundRelay stamp on a LIVE per-session
    // producer — NOT the global session-bound flag. The sink only commits when a
    // production tmux watcher is feeding the supervisor-owned StreamRelay for this
    // session; with no registered producer the bridge tail must stay the deliverer.
    let live_producer_present =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .get_producer(tmux_session_name)
            .is_some();
    let relay_owner = tui_direct_synthetic_relay_owner(
        tui_direct_watcher_can_own_output(
            &shared.tmux_watchers,
            tmux_session_name,
            output_path.as_deref(),
        ),
        session_bound_discord_delivery_enabled(),
        live_producer_present,
    );
    let relay_owner_kind = match relay_owner {
        ExternalInputRelayOwner::TmuxWatcher => RelayOwnerKind::Watcher,
        ExternalInputRelayOwner::SessionBoundRelay => RelayOwnerKind::SessionBoundRelay,
        _ => RelayOwnerKind::None,
    };

    let cancel_token = Arc::new(CancelToken::new());
    super::super::turn_bridge::bind_cancel_token_tmux_runtime(
        provider,
        &cancel_token,
        tmux_session_name,
        "tui_direct_synthetic_inflight",
    );
    // #3167 — the self-paced TUI loop / TUI-direct turn is a low-priority
    // background turn; mark it `Background` so a queued external USER
    // intervention is not starved behind the continuously-cycling loop.
    let mut started = super::super::mailbox_try_start_turn_kinded(
        shared,
        channel_id,
        cancel_token.clone(),
        serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
        anchor_message_id,
        crate::services::turn_orchestrator::ActiveTurnKind::Background,
    )
    .await;
    if !started {
        let snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
        if snapshot.active_user_message_id != Some(anchor_message_id) {
            if let Some(active_user_message_id) = snapshot.active_user_message_id
                && release_stale_ownerless_tui_direct_mailbox_if_current(
                    shared,
                    provider,
                    channel_id,
                    tmux_session_name,
                    active_user_message_id,
                    anchor_message_id,
                )
                .await
            {
                started = super::super::mailbox_try_start_turn_kinded(
                    shared,
                    channel_id,
                    cancel_token.clone(),
                    serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                    anchor_message_id,
                    crate::services::turn_orchestrator::ActiveTurnKind::Background,
                )
                .await;
                if started {
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        anchor_message_id = anchor_message_id.get(),
                        "TUI-direct synthetic inflight claimed after releasing stale ownerless mailbox"
                    );
                }
            }
        }
        if !started {
            let snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
            if snapshot.active_user_message_id == Some(anchor_message_id) {
                started = true;
            } else {
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel_id = channel_id.get(),
                    tmux_session_name = %tmux_session_name,
                    active_user_message_id = snapshot
                        .active_user_message_id
                        .map(|id| id.get())
                        .unwrap_or(0),
                    anchor_message_id = anchor_message_id.get(),
                    "skipping TUI-direct synthetic inflight; mailbox already owns a different turn"
                );
                return TuiDirectSyntheticTurnClaim {
                    relay_owner,
                    claimed: false,
                    turn_start_offset: start_offset,
                };
            }
        }
    }

    // #3146 Part 1: a TUI-driven turn is now active for this channel (we either
    // just started it via `mailbox_try_start_turn` or already own the matching
    // turn). Clear any stale `📦 … idle N분` recap card the same way the
    // Discord-intake path does (`intake_gate` → `spawn_clear_idle_recap_for_channel`).
    // Without this, a turn that starts from the tmux TUI (user-typed OR the
    // autonomous self-drive loop) never goes through Discord intake, so the
    // recap card kept showing `idle N분` over a live turn.
    //
    // codex R2 P2: capture the recap card id THAT EXISTS NOW (the turn just
    // became active) and clear ONLY that captured id (compare-and-clear on the
    // pointer). The idle-recap policy posts at most once per idle period, so a
    // delayed clear that deleted a LATER legitimately-posted card would lose it
    // for the rest of the idle period (NOT self-healing). Binding the clear to
    // the captured id makes a delayed clear a no-op against any newer card.
    if let Some(pool) = shared.pg_pool.as_ref().cloned()
        && let Some(http) = shared.serenity_http_or_token_fallback()
    {
        // #3148: bump the per-channel turn generation BEFORE the clear. This is
        // the same claim-bump the Discord-intake path does — any idle-recap
        // POST job whose persist CAS captured the pre-bump generation now fails
        // to persist its card over this just-claimed TUI turn. The clear then
        // removes any card the POST already persisted before this claim.
        if let Err(e) = super::super::idle_recap::bump_turn_generation(
            &pool,
            channel_id.get(),
            provider,
            lease.session_key.as_deref(),
        )
        .await
        {
            tracing::warn!(
                error = %e,
                channel_id = channel_id.get(),
                "idle_recap: failed to bump turn generation on TUI claim"
            );
        }
        super::super::idle_recap::spawn_clear_captured_idle_recap_for_channel(
            http,
            pool,
            channel_id.get(),
        )
        .await;
    }

    if let Some(existing) = super::super::inflight::load_inflight_state(provider, channel_id.get())
        && existing.tmux_session_name.as_deref() == Some(tmux_session_name)
        && existing.turn_source == TurnSource::ExternalInput
        && existing.user_msg_id == anchor_message_id.get()
    {
        let mut existing = existing;
        existing.set_relay_owner_kind(relay_owner_kind);
        existing.session_key = lease.session_key.clone();
        existing.runtime_kind = lease.runtime_kind;
        existing.output_path = output_path
            .as_deref()
            .and_then(|path| path.to_str().map(str::to_string));
        existing.last_offset = start_offset;
        existing.turn_start_offset = Some(start_offset);
        // #3099 codex re-review (P2): keep this turn's own injected `⏳` message id
        // pinned so completion cleanup never reads a later injection's overwrite of
        // the shared prompt-anchor slot.
        existing.injected_prompt_message_id = Some(anchor_message_id.get());
        if let Err(error) = super::super::inflight::save_inflight_state(&existing) {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                error = %error,
                "failed to refresh TUI-direct synthetic inflight ownership"
            );
            if started {
                finish_tui_direct_synthetic_pre_save_failure(shared, provider, channel_id).await;
            }
            return TuiDirectSyntheticTurnClaim {
                relay_owner,
                claimed: false,
                turn_start_offset: start_offset,
            };
        }
        if started {
            super::super::increment_global_active(shared, "tui_direct_synthetic_refresh");
            shared
                .turn_start_times
                .insert(channel_id, std::time::Instant::now());
        }
        return TuiDirectSyntheticTurnClaim {
            relay_owner,
            claimed: true,
            turn_start_offset: start_offset,
        };
    }

    let mut inflight_state = build_tui_direct_synthetic_inflight_state(
        provider.clone(),
        channel_id,
        anchor_message_id,
        None,
        prompt_text,
        tmux_session_name,
        output_path.as_deref(),
        start_offset,
        lease,
        relay_owner_kind,
    );
    // #4002: mark a SystemContinuation (compact-resume) inflight relay-ownership-only so
    // watcher completion Path B (⏳→✅ + transcripts/analytics) skips it (inline+deferred).
    inflight_state.relay_ownership_only =
        classify_injected_prompt(prompt_text).suppresses_user_turn_lifecycle();
    if let Err(error) = super::super::inflight::save_inflight_state(&inflight_state) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            error = %error,
            "failed to save TUI-direct synthetic inflight"
        );
        if started {
            finish_tui_direct_synthetic_pre_save_failure(shared, provider, channel_id).await;
        }
        return TuiDirectSyntheticTurnClaim {
            relay_owner,
            claimed: false,
            turn_start_offset: start_offset,
        };
    }

    if started {
        super::super::increment_global_active(shared, "tui_direct_synthetic_save");
        shared
            .turn_start_times
            .insert(channel_id, std::time::Instant::now());
    }
    tracing::info!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        anchor_message_id = anchor_message_id.get(),
        relay_owner = relay_owner.as_str(),
        mailbox_started = started,
        "created TUI-direct synthetic inflight for already-submitted provider turn"
    );
    TuiDirectSyntheticTurnClaim {
        relay_owner,
        claimed: true,
        turn_start_offset: start_offset,
    }
}

pub(super) async fn release_stale_ownerless_tui_direct_mailbox_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    active_user_message_id: MessageId,
    anchor_message_id: MessageId,
) -> bool {
    let Some(state) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return false;
    };
    if state.user_msg_id != active_user_message_id.get()
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || !super::super::inflight::ownerless_external_input_inflight_is_stale(&state)
    {
        return false;
    }

    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        channel_id,
        active_user_message_id,
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            stale_user_message_id = active_user_message_id.get(),
            anchor_message_id = anchor_message_id.get(),
            "TUI-direct stale ownerless mailbox release skipped because mailbox identity changed"
        );
        return false;
    };
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let global_active_decremented = super::super::saturating_decrement_global_active(shared);
    tracing::warn!(
        provider = %provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        stale_user_message_id = active_user_message_id.get(),
        anchor_message_id = anchor_message_id.get(),
        global_active_decremented,
        had_pending_queue = finish.has_pending,
        "released stale ownerless TUI-direct mailbox before claiming new synthetic inflight"
    );
    true
}

// ===========================================================================
// #3154 — deferred synthetic turn-start (off the observer loop)
// ===========================================================================

/// Build the
/// [`PriorTurnObservation`](super::super::tui_direct_pending_start::PriorTurnObservation)
/// for the synthetic-start deferral decision: read inflight, mailbox, and the
/// fresh runtime binding. Besides the pure decision view it carries the live
/// FOREIGN inflight's identity (codex r2) so the worker can pin it on the
/// aborted-anchor marker even when the row vanishes before the ABORT cleanup.
pub(super) async fn synthetic_start_prior_turn_view(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    own_anchor_id: u64,
) -> super::super::tui_direct_pending_start::PriorTurnObservation {
    let inflight = super::super::inflight::load_inflight_state(provider, channel_id.get())
        .filter(|state| !super::super::inflight::ownerless_external_input_inflight_is_stale(state));
    let inflight_present = inflight.is_some();
    let inflight_is_own_anchor = inflight
        .as_ref()
        .map(|state| {
            state.turn_source == TurnSource::ExternalInput
                && state.tmux_session_name.as_deref() == Some(tmux_session_name)
                && state.user_msg_id == own_anchor_id
        })
        .unwrap_or(false);
    let foreign_inflight_identity = inflight
        .as_ref()
        .filter(|_| !inflight_is_own_anchor)
        .map(|state| (state.user_msg_id, state.started_at.clone()));

    let snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
    // A BACKGROUND turn (monitor relay / self-paced loop) does not block — only a
    // real (non-background) active turn is a blocking prior turn (mirrors the
    // idle-queue kickoff gate at mod.rs `idle_queue_snapshot_has_kickable_backlog`).
    let mailbox_blocking_turn_present =
        snapshot.cancel_token.is_some() && !snapshot.active_turn_kind.is_background();
    let mailbox_turn_is_own_anchor =
        snapshot.active_user_message_id == Some(MessageId::new(own_anchor_id));

    let runtime_binding_present =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .is_some();

    super::super::tui_direct_pending_start::PriorTurnObservation {
        view: super::super::tui_direct_pending_start::PriorTurnView {
            inflight_present,
            inflight_is_own_anchor,
            mailbox_blocking_turn_present,
            mailbox_turn_is_own_anchor,
            runtime_binding_present,
        },
        foreign_inflight_identity,
    }
}

/// Persist a durable pending-start record and spawn the detached per-channel
/// worker. Returns immediately (non-blocking for the observer loop).
pub(super) fn defer_synthetic_turn_start(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    anchor_message_id: MessageId,
    lease: &ExternalInputRelayLease,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let record = super::super::tui_direct_pending_start::TuiDirectPendingStart {
        provider: provider.as_str().to_string(),
        channel_id: channel_id.get(),
        tmux_session_name: prompt.tmux_session_name.clone(),
        prompt_text: prompt.prompt.clone(),
        anchor_message_id: anchor_message_id.get(),
        lease_relay_owner: lease.relay_owner.as_str().to_string(),
        lease_runtime_kind: lease.runtime_kind.map(|k| k.as_str().to_string()),
        lease_turn_id: lease.turn_id.clone(),
        lease_session_key: lease.session_key.clone(),
        generation: shared.restart.current_generation,
        created_at_ms: now_ms,
        observed_at_ms: prompt.observed_at.timestamp_millis().max(0) as u64,
        state: super::super::tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
    };
    if let Err(error) = super::super::tui_direct_pending_start::persist(&record) {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            error = %error,
            "failed to persist durable TUI-direct pending-start record; spawning worker anyway off the in-memory presence index"
        );
    }
    super::super::tui_direct_pending_start::spawn_worker(
        shared.clone(),
        record,
        pending_start_view_fn(),
        pending_start_claim_fn(),
        pending_start_abort_cleanup_fn(),
        super::synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
}

/// The worker's per-poll view builder (see [`synthetic_start_prior_turn_view`]).
pub(super) fn pending_start_view_fn() -> super::super::tui_direct_pending_start::ViewFn {
    Box::new(|shared, record| {
        Box::pin(async move {
            let provider = ProviderKind::from_str(&record.provider)?;
            let channel_id = ChannelId::new(record.channel_id);
            Some(
                synthetic_start_prior_turn_view(
                    shared,
                    &provider,
                    channel_id,
                    &record.tmux_session_name,
                    record.anchor_message_id,
                )
                .await,
            )
        })
    })
}

/// The worker's claim action: rehydrate the lease (in case a restart dropped the
/// in-memory map), then run the normal [`claim_tui_direct_synthetic_turn`] which
/// reads the runtime binding FRESH and seeds `turn_start_offset = relay_last_offset()`
/// (post-drain == EOF) with `response_sent_offset = 0`.
pub(super) fn pending_start_claim_fn() -> super::super::tui_direct_pending_start::ClaimFn {
    Box::new(|shared, record| {
        Box::pin(async move {
            let Some(provider) = ProviderKind::from_str(&record.provider) else {
                return false;
            };
            let channel_id = ChannelId::new(record.channel_id);
            let anchor_message_id = MessageId::new(record.anchor_message_id);

            // Rehydrate the external-input lease from the durable record's
            // fields (a restart clears the in-memory lease map). NEVER resubmit
            // the provider prompt — only the relay lease is restored.
            let mut lease = ExternalInputRelayLease::unassigned(Some(record.channel_id));
            lease.turn_id = record.lease_turn_id.clone();
            lease.session_key = record.lease_session_key.clone();
            lease.relay_owner = parse_external_input_relay_owner(&record.lease_relay_owner);
            lease.runtime_kind = record
                .lease_runtime_kind
                .as_deref()
                .and_then(RuntimeHandoffKind::from_str);
            let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                provider.as_str(),
                &record.tmux_session_name,
                lease,
            );

            // #3154 design point 6: register the turn with the single-authority
            // finalizer BEFORE the claim saves the inflight + (implicitly, via
            // the lease/inflight) releases the watcher gate — mirrors the bridge
            // register-before-unpause at turn_bridge/mod.rs.
            shared.turn_finalizer.register_start(
                super::super::turn_finalizer::TurnKey::new(
                    channel_id,
                    record.anchor_message_id,
                    shared.restart.current_generation,
                ),
                provider.clone(),
                super::super::inflight::RelayOwnerKind::Watcher,
                // #3016 phase-5a: prime the reconcile cache at register time.
                shared,
            );

            let claim = claim_tui_direct_synthetic_turn(
                shared,
                &provider,
                channel_id,
                &record.tmux_session_name,
                &record.prompt_text,
                anchor_message_id,
                &lease,
            )
            .await;

            // #3154 P1-3: adopt the claim's relay_owner into the in-memory lease
            // EXACTLY like the inline (non-deferred) path does (see
            // `relay_observed_prompt` lines ~854-865). The claim may decide the
            // tmux WATCHER owns this turn's output; if so, the persisted lease
            // (rehydrated as BridgeAdapter) is stale. Without re-recording it,
            // the observer-side `maybe_spawn_claude_idle_response_tail` / bridge
            // tail keeps running on the stale BridgeAdapter lease while the
            // watcher relays the same output → DUPLICATE relay (the original
            // bug). Re-record with the adopted owner so any bridge-tail guard
            // reading the lease sees the watcher owns it and stops.
            if claim_should_adopt_relay_owner(claim.claimed, lease.relay_owner, claim.relay_owner) {
                let mut adopted = lease.clone();
                adopted.relay_owner = claim.relay_owner;
                let _ = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                    provider.as_str(),
                    &record.tmux_session_name,
                    adopted,
                );
                tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    prior_relay_owner = lease.relay_owner.as_str(),
                    adopted_relay_owner = claim.relay_owner.as_str(),
                    "tui_direct_pending_start: deferred claim adopted watcher relay_owner into the in-memory lease (bridge tail will stand down)"
                );
            }

            // #3154 P1 (BridgeAdapter-GAP fix). The observer stood down for ALL
            // deferred starts because it could not know the RESOLVED owner before
            // the claim ran. Now that the claim has resolved it, the worker is the
            // single place that knows the owner kind, so it MIRRORS the inline path:
            // when the claim resolved to the BridgeAdapter (no watcher will relay
            // this turn), the worker spawns EXACTLY ONE bridge tail here — otherwise
            // the synthetic turn's output is never relayed (relayer_count == 0). When
            // the claim resolved to the watcher this predicate is false (the watcher
            // is the sole relayer; spawning would double-relay). The spawn is on the
            // detached worker task (unix), exactly like the observer's unix-only tail.
            #[cfg(unix)]
            if claim.claimed && deferred_claim_requires_bridge_tail_relayer(claim.relay_owner) {
                // The lease the bridge tail reads must reflect the resolved owner.
                // `claim_should_adopt_relay_owner` above is false for the BridgeAdapter
                // case (the rehydrated lease was already BridgeAdapter), so re-read the
                // stored lease (or fall back to the rehydrated one) and ensure it carries
                // the resolved owner before handing it to the self-gating tail.
                let mut tail_lease =
                    crate::services::tui_prompt_dedupe::external_input_relay_lease(
                        provider.as_str(),
                        &record.tmux_session_name,
                        record.channel_id,
                    )
                    .unwrap_or_else(|| lease.clone());
                tail_lease.relay_owner = claim.relay_owner;
                // #3154 P1 (timestamp-anchor output loss): `observed_at` is NO LONGER
                // used to anchor the tail's start offset for this deferred path — we
                // pass the claim's post-drain EOF `turn_start_offset` explicitly below
                // (see `explicit_start_offset`). It remains on the struct only for the
                // tail's tracing/lease bookkeeping; a `Utc::now()` timestamp scan here
                // would skip bytes written during the deferred-claim wait window.
                let observed = ObservedTuiPrompt {
                    provider: record.provider.clone(),
                    tmux_session_name: record.tmux_session_name.clone(),
                    prompt: record.prompt_text.clone(),
                    observed_at: chrono::Utc::now(),
                };
                let spawned = maybe_spawn_claude_idle_response_tail(
                    shared.clone(),
                    channel_id,
                    &observed,
                    &tail_lease,
                    Some(record.anchor_message_id),
                    // Anchor to the claim's post-drain EOF offset (source of truth
                    // for this synthetic turn's first byte) — NOT a timestamp scan.
                    Some(claim.turn_start_offset),
                )
                .await;
                tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    resolved_relay_owner = claim.relay_owner.as_str(),
                    bridge_tail_spawned = spawned,
                    "tui_direct_pending_start: deferred claim resolved to BridgeAdapter owner; worker spawned the bridge tail (no relay GAP)"
                );
            }
            claim.claimed
        })
    })
}

/// #3296 (supersedes the #3282 `⏳ → ⚠` swap): the worker's terminal-ABORT
/// reconcile hook. The input was ALREADY provider-submitted by ABORT time (the
/// abort drops only the synthetic OWNERSHIP claim), so the anchor's `⏳` is
/// still TRUE — the old `⚠` swap branded ANSWERED messages as failures. So:
/// KEEP the `⏳` and record a durable aborted-anchor marker pinning the
/// FOREIGN prior inflight's identity — the worker's LAST-VIEW identity first,
/// the cleanup-instant row only as the no-view fallback (codex r3,
/// `pin_abort_foreign_identity`). The marker stays uncovered unless a commit
/// tombstone proves the prior owner committed (`record_for_abort`'s 대조;
/// force-clear/stop/recovery deletions stay uncovered). The watcher drain
/// flips it `⏳ → ✅` ONLY when THAT turn commits; the sweeper flips it
/// `⏳ → ⚠` after the TTL with no holding inflight (hard-cap bounded).
/// Recorded even when http is unavailable; every later reaction op resolves
/// the shared http INSIDE the marker module — the add≡remove identity (#3164).
pub(super) fn pending_start_abort_cleanup_fn()
-> super::super::tui_direct_pending_start::AbortCleanupFn {
    Box::new(|_shared, record, last_view_foreign| {
        Box::pin(async move {
            // Defensive (I5): a corrupted durable record could carry a zero
            // anchor id — `MessageId::new(0)` panics and a zero-id marker could
            // never be reconciled. `record()` rejects it too; skip outright.
            if record.anchor_message_id == 0 {
                return;
            }
            // codex r3: LAST-VIEW first — a SUCCESSOR row may hold the slot by
            // now (prior row committed); the row read is a lazy fallback only.
            let foreign = super::super::tui_direct_pending_start::pin_abort_foreign_identity(
                last_view_foreign,
                || {
                    ProviderKind::from_str(&record.provider)
                        .and_then(|provider| {
                            super::super::inflight::load_inflight_state(
                                &provider,
                                record.channel_id,
                            )
                        })
                        .map(|state| (state.user_msg_id, state.started_at))
                },
            );
            match super::super::tui_direct_abort_marker::record_for_abort(
                record.provider.clone(),
                record.channel_id,
                record.anchor_message_id,
                record.tmux_session_name.clone(),
                foreign,
            ) {
                Ok(marker) => tracing::info!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    tmux_session_name = %record.tmux_session_name,
                    anchor_message_id = record.anchor_message_id,
                    foreign_user_msg_id = ?marker.foreign_user_msg_id,
                    tombstone_covered = marker.covered_at_ms.is_some(),
                    "tui_direct_pending_start: synthetic turn-start ABORTed; anchor keeps ⏳ and a durable aborted-anchor marker was recorded — reconcile lands ✅ on the recorded foreign turn's commit (tombstone-covered when it already committed) or ⚠ via the sweep bound (#3296)"
                ),
                Err(error) => tracing::warn!(
                    provider = %record.provider,
                    channel_id = record.channel_id,
                    anchor_message_id = record.anchor_message_id,
                    error = %error,
                    "tui_direct_pending_start: failed to persist the aborted-anchor marker; anchor ⏳ may linger until manual cleanup (#3296)"
                ),
            }
        })
    })
}

/// #3154 restart durability: restore durable pending-start records during
/// provider relay startup. Rehydrates the in-memory presence index (so the
/// watcher / idle-queue gates hold immediately) and respawns the worker for each
/// record whose provider matches.
pub(super) fn restore_pending_starts(shared: &Arc<SharedData>, provider: &ProviderKind) {
    for record in super::super::tui_direct_pending_start::load_all() {
        if !record.provider.eq_ignore_ascii_case(provider.as_str()) {
            continue;
        }
        // Re-mark present (load_all does not touch the index) so the gates hold
        // before the worker's first poll.
        super::super::tui_direct_pending_start::mark_present_on_restore(
            &record.provider,
            record.channel_id,
        );
        tracing::info!(
            provider = %record.provider,
            channel_id = record.channel_id,
            tmux_session_name = %record.tmux_session_name,
            anchor_message_id = record.anchor_message_id,
            "restored durable TUI-direct pending-start record on relay startup; respawning detached worker (prompt NOT resubmitted)"
        );
        super::super::tui_direct_pending_start::spawn_worker(
            shared.clone(),
            record,
            pending_start_view_fn(),
            pending_start_claim_fn(),
            pending_start_abort_cleanup_fn(),
            super::synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
        );
    }
}

// #3016 phase-5b2: `publish_tui_direct_watcher_finalize_debt` was removed. It
// stored the per-handle `mailbox_finalize_owed` flag (#1452) for TUI-direct
// synthetic turns, but that flag has no remaining finalize-decision readers —
// the watcher finalizes on the confirmed-completion / structural signal
// (`normal_completion = true`) and the ledger's `register_start` is the
// authority — so the producer was a dead write and is gone.

pub(super) fn tui_direct_watcher_can_own_output(
    watchers: &super::super::TmuxWatcherRegistry,
    tmux_session_name: &str,
    output_path: Option<&Path>,
) -> bool {
    let watcher_alive = watchers
        .tmux_session_is_stale(tmux_session_name)
        .is_some_and(|stale| !stale);
    if !watcher_alive {
        return false;
    }
    match output_path {
        Some(output_path) => watchers
            .watcher_output_path(tmux_session_name)
            .is_some_and(|watcher_path| Path::new(&watcher_path) == output_path),
        None => true,
    }
}

/// #3876: resolve the relay owner stamped on a freshly-created TUI-direct /
/// warm-followup synthetic inflight. PURE so the birth-site decision is
/// unit-testable; the call site supplies the three signals.
///
/// * `watcher_can_own` (from [`tui_direct_watcher_can_own_output`]) → the live
///   watcher relays this turn's output → `TmuxWatcher` (unchanged).
/// * else, when session-bound Discord delivery is enabled AND a LIVE
///   session-bound StreamRelay producer exists for this tmux session
///   (`live_producer_present`) → `SessionBoundRelay`. The synthetic inflight IS
///   being created here, so the session-bound relay sink can legitimately be its
///   terminal owner (the `relay_ownership` observer restriction that keeps
///   `BridgeAdapter` only applies BEFORE any inflight exists). The sink gate
///   `session_bound_discord_relay_can_own_terminal_delivery` then ACCEPTS the
///   row and commits the harvested body — fixing the prior `RelayOwnerKind::None`
///   drop (`route="none"`, `terminal_commit_ack=false`) that left a
///   placeholder-only delivery to be swept (data loss). Single-relayer invariant
///   holds: the bridge tail stands down (`bridge_adapter_owns_external_turn`
///   → false) and the watcher yields (`tmux::watcher_should_yield_to_inflight_state`
///   yields for a `SessionBoundRelay` owner), so the sink is the sole committer.
/// * else (no live producer, or session-bound delivery disabled) →
///   `BridgeAdapter`. CRITICAL regression guard (codex review): the session-bound
///   StreamRelay is a PASSIVE MPSC consumer fed ONLY by a live production
///   tmux watcher (`tmux_watcher::forward_chunk_to_supervisor_relay`); with no
///   live producer registered (`relay_producer_registry::get_producer` → `None`,
///   e.g. a STALL-WATCHDOG force-clean detached the watcher) a `SessionBoundRelay`
///   stamp would STARVE the sink AND stand the bridge tail down → answer loss.
///   `BridgeAdapter` keeps the watcher-INDEPENDENT transcript-direct bridge tail
///   (`claude_idle_tail.rs`, gated only on owner-kind) as the backstop deliverer.
pub(super) fn tui_direct_synthetic_relay_owner(
    watcher_can_own: bool,
    session_bound_discord_delivery_enabled: bool,
    live_producer_present: bool,
) -> ExternalInputRelayOwner {
    if watcher_can_own {
        ExternalInputRelayOwner::TmuxWatcher
    } else if session_bound_discord_delivery_enabled && live_producer_present {
        ExternalInputRelayOwner::SessionBoundRelay
    } else {
        ExternalInputRelayOwner::BridgeAdapter
    }
}

pub(super) fn tui_direct_synthetic_inflight_active_for_prompt(
    provider: &str,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let Some(provider) = ProviderKind::from_str(provider) else {
        return false;
    };
    tui_direct_synthetic_inflight_matches(
        super::super::inflight::load_inflight_state(&provider, channel_id.get()).as_ref(),
        tmux_session_name,
    )
}

pub(super) fn tui_direct_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
    })
}

pub(super) fn tui_direct_watcher_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher
    })
}

#[cfg(unix)]
pub(super) fn codex_ownerless_external_input_inflight_needs_rollout_recovery(
    state: &InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    if state.turn_source != TurnSource::ExternalInput
        || state.runtime_kind != Some(RuntimeHandoffKind::CodexTui)
        || state.effective_relay_owner_kind() != RelayOwnerKind::None
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
        || state.injected_prompt_message_id.is_none()
        || state.current_msg_id != 0
        || state.response_sent_offset != 0
        || !state.full_response.trim().is_empty()
        || state.last_watcher_relayed_offset.is_some()
        || state.terminal_delivery_committed
    {
        return false;
    }
    // At this point the inflight is ownerless and no Discord delivery has ever
    // started. Recovery must run whether `output_path` is stale/missing or
    // already points at the live rollout: an earlier deploy can interrupt after
    // repairing the path but before the bridge posts a response.
    true
}

#[cfg(unix)]
pub(super) async fn wait_for_tui_direct_watcher_synthetic_claim(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let deadline = tokio::time::Instant::now() + TUI_DIRECT_SYNTHETIC_CLAIM_WAIT;
    loop {
        if tui_direct_watcher_synthetic_inflight_matches(
            super::super::inflight::load_inflight_state(provider, channel_id.get()).as_ref(),
            tmux_session_name,
        ) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep(TUI_DIRECT_SYNTHETIC_CLAIM_POLL.min(deadline - now)).await;
    }
}

#[cfg(unix)]
pub(super) async fn finish_tui_direct_synthetic_turn_if_current(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    reason: &'static str,
) {
    let Some(state) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if !tui_direct_synthetic_inflight_matches(Some(&state), tmux_session_name) {
        return;
    }
    let snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
    // user_msg_id == 0 (a TUI-direct turn with no anchored Discord user
    // message) maps to `None`, matching the mailbox's `active_user_message_id`
    // for such turns; `MessageId::new(0)` would panic.
    if snapshot.active_user_message_id
        != super::super::inflight::optional_message_id(state.user_msg_id)
    {
        return;
    }
    super::super::inflight::clear_inflight_state(provider, channel_id.get());
    let finish = super::super::mailbox_finish_turn(shared, provider, channel_id).await;
    if finish.removed_token.is_some() {
        super::super::saturating_decrement_global_active(shared);
    }
    if finish.mailbox_online && finish.has_pending {
        super::super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            reason,
        );
    }
}

pub(super) fn build_tui_direct_synthetic_inflight_state(
    provider: ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: Option<MessageId>,
    prompt_text: &str,
    tmux_session_name: &str,
    output_path: Option<&Path>,
    start_offset: u64,
    lease: &ExternalInputRelayLease,
    relay_owner_kind: RelayOwnerKind,
) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        provider,
        channel_id.get(),
        None,
        TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
        user_msg_id.get(),
        current_msg_id.map(MessageId::get).unwrap_or(0),
        prompt_text.to_string(),
        None,
        Some(tmux_session_name.to_string()),
        output_path.and_then(|path| path.to_str().map(str::to_string)),
        None,
        start_offset,
    );
    state.current_msg_len = "...".len();
    state.session_key = lease.session_key.clone();
    state.runtime_kind = lease.runtime_kind;
    state.turn_source = TurnSource::ExternalInput;
    state.set_relay_owner_kind(relay_owner_kind);
    // #3099 codex re-review (P2): pin THIS turn's injected `⏳` message id onto
    // the inflight so the `user_msg_id == 0` completion cleanup can target this
    // turn's own message instead of whatever later injection has since
    // overwritten the single shared prompt-anchor slot.
    state.injected_prompt_message_id = Some(user_msg_id.get());
    state
}

// #3982 orphan-at-birth reclaim helpers + their unit tests live in the sibling
// `synthetic_orphan_reclaim` module (keeps this file focused and under the
// giant-file threshold); `defer_synthetic_turn_start` / `restore_pending_starts`
// inject `synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn()`.
