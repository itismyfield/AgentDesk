use super::*;

#[cfg(test)]
mod tests;

pub(super) async fn claim_tui_direct_synthetic_turn_prepared(
    preparation: SyntheticClaimPreparation<'_>,
) -> TuiDirectSyntheticTurnClaim {
    let SyntheticClaimPreparation {
        identity,
        output_path,
        start_offset,
        relay_owner,
        relay_owner_kind,
    } = preparation;

    let SyntheticClaimIdentity {
        shared,
        provider,
        channel_id,
        tmux_session_name,
        prompt_text,
        anchor_message_id,
        lease,
        ..
    } = identity;

    let admission = match bridge_handoff::prepare_admission(
        shared,
        provider,
        channel_id,
        tmux_session_name,
        anchor_message_id,
        lease,
        identity.transition,
    )
    .await
    {
        Ok(admission) => admission,
        Err(error) => {
            tracing::warn!(%error, "synthetic ownership capture failed before admission");
            return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
        }
    };
    let cancel_token = admission.token.clone();
    let pg_pin = admission.pg_pin.clone();
    let class = admission.class.clone();
    if !original_source_matches(&admission.fence, output_path.as_deref(), lease).await {
        return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
    }
    super::super::super::super::turn_bridge::bind_cancel_token_tmux_runtime(
        provider,
        &cancel_token,
        tmux_session_name,
        "tui_direct_synthetic_inflight",
    );
    // #3167 — the self-paced TUI loop / TUI-direct turn is a low-priority
    // background turn; mark it `Background` so a queued external USER
    // intervention is not starved behind the continuously-cycling loop.
    let mut started = bridge_handoff::admit_kinded(
        shared,
        channel_id,
        &admission,
        serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
        anchor_message_id,
    )
    .await;
    let mut mailbox_activation_occurred = started;
    if !started {
        let snapshot = super::super::super::super::mailbox_snapshot(shared, channel_id).await;
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
                started = bridge_handoff::admit_kinded(
                    shared,
                    channel_id,
                    &admission,
                    serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                    anchor_message_id,
                )
                .await;
                if started {
                    mailbox_activation_occurred = true;
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        anchor_message_id = anchor_message_id.get(),
                        "TUI-direct synthetic inflight claimed after releasing stale ownerless mailbox"
                    );
                }
            }
            if !started
                && let Some(active_user_message_id) = snapshot.active_user_message_id
                && release_reclaimable_stale_synthetic_mailbox_owner_if_current(
                    shared,
                    provider,
                    channel_id,
                    tmux_session_name,
                    active_user_message_id,
                    snapshot.active_request_owner,
                    snapshot.active_turn_kind,
                    snapshot.turn_started_at,
                    snapshot.active_turn_nonce.clone(),
                    anchor_message_id,
                )
                .await
            {
                started = bridge_handoff::admit_kinded(
                    shared,
                    channel_id,
                    &admission,
                    serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                    anchor_message_id,
                )
                .await;
                if started {
                    mailbox_activation_occurred = true;
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        anchor_message_id = anchor_message_id.get(),
                        "TUI-direct synthetic inflight claimed after reclaiming stale synthetic mailbox owner"
                    );
                }
            }
        }
        if !started {
            let snapshot = super::super::super::super::mailbox_snapshot(shared, channel_id).await;
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
                return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
            }
        }
    }

    #[cfg(test)]
    bridge_handoff::pause_after_admission_for_test(channel_id).await;

    // Capture the actor-owned episode identity after admission. If this call
    // observed an already-active matching synthetic turn, the fresh local token
    // was not admitted; the mailbox snapshot, not that unused token, is the
    // authority for the nonce persisted below and the allocation handed to the bridge.
    let active_snapshot = super::super::super::super::mailbox_snapshot(shared, channel_id).await;
    if active_snapshot.active_user_message_id != Some(anchor_message_id)
        || active_snapshot.cancel_token.as_ref().is_none_or(|active| {
            (mailbox_activation_occurred || relay_owner == ExternalInputRelayOwner::BridgeAdapter)
                && !Arc::ptr_eq(active, &cancel_token)
        })
    {
        return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
    }
    let admitted_actor = if mailbox_activation_occurred {
        cancel_token
    } else {
        active_snapshot
            .cancel_token
            .clone()
            .expect("checked mailbox actor")
    };
    let active_turn_nonce = admitted_actor.turn_nonce().map(str::to_owned);
    identity.register_episode(active_turn_nonce.as_deref());

    // #3146/#3148: clear only the captured idle recap and bump its generation
    // before clearing, so an older asynchronous POST cannot restore stale chrome.
    if let Some(pool) = shared.pg_pool.as_ref().cloned()
        && let Some(http) = shared.serenity_http_or_token_fallback()
    {
        if let Err(e) = super::super::super::super::idle_recap::bump_turn_generation(
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
        super::super::super::super::idle_recap::spawn_clear_captured_idle_recap_for_channel(
            http,
            pool,
            channel_id.get(),
        )
        .await;
    }

    // The recap work above can yield to a successor. The original allocation,
    // never a later same-nonce snapshot, owns this save and its failure cleanup.
    if !bridge_handoff::actor_is_current(shared, channel_id, anchor_message_id, &admitted_actor)
        .await
    {
        return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
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
    inflight_state.turn_nonce = active_turn_nonce;
    inflight_state.native_turn_id = identity.native_turn.map(str::to_string);
    // #4002/#4082: lower-level safety. Normal wiring gates neutral continuation
    // records before this point; if a suppressing class reaches the raw claim API,
    // keep it relay-ownership-only so watcher completion Path B skips it.
    inflight_state.relay_ownership_only =
        classify_injected_prompt(prompt_text).suppresses_user_turn_lifecycle();
    if let Some(pin) = lease.hosted_source.as_ref() {
        inflight_state.session_id = Some(pin.source.session_id.clone());
    }
    if let Some((row_id, nonce)) = admission.fence.hosted_identity() {
        inflight_state.hosted_record_id = Some(row_id);
        inflight_state.hosted_execution_nonce = Some(nonce);
    }

    if let Some(existing) =
        super::super::super::super::inflight::load_inflight_state(provider, channel_id.get())
        && existing.tmux_session_name.as_deref() == Some(tmux_session_name)
        && existing.turn_source == TurnSource::ExternalInput
        && existing.user_msg_id == anchor_message_id.get()
        && existing.output_path.as_deref().map(Path::new) == output_path.as_deref()
        && existing.external_turn_id == lease.turn_id
        && existing.session_key == lease.session_key
        && bridge_handoff::refresh_actor_matches(
            &existing,
            Some(&admitted_actor),
            (mailbox_activation_occurred, &class),
        )
    {
        return bridge_handoff::refresh_existing(
            shared,
            existing,
            lease,
            relay_owner,
            relay_owner_kind,
            (
                Some(&admitted_actor),
                mailbox_activation_occurred,
                &admission.fence,
            ),
            pg_pin,
        )
        .await;
    }

    if !original_source_matches(&admission.fence, output_path.as_deref(), lease).await
        || admission.fence.revalidate().await.is_err()
    {
        if mailbox_activation_occurred {
            bridge_handoff::release_unrecorded_actor(
                shared,
                &inflight_state,
                Some(&admitted_actor),
                false,
            )
            .await;
        }
        return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
    }

    match super::super::super::super::tui_direct_pending_start::save_row_unless_retired(
        &inflight_state,
    ) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                "skipped TUI-direct synthetic inflight: a durable row exists or the pending start retired"
            );
            if mailbox_activation_occurred {
                bridge_handoff::release_unrecorded_actor(
                    shared,
                    &inflight_state,
                    Some(&admitted_actor),
                    false,
                )
                .await;
            }
            return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                error = %error,
                "failed to save TUI-direct synthetic inflight"
            );
            if mailbox_activation_occurred {
                bridge_handoff::release_unrecorded_actor(
                    shared,
                    &inflight_state,
                    Some(&admitted_actor),
                    false,
                )
                .await;
            }
            return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
        }
    }

    if !bridge_handoff::record_admitted(
        shared,
        &inflight_state,
        Some(&admitted_actor),
        pg_pin,
        mailbox_activation_occurred,
    )
    .await
    {
        return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
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
    TuiDirectSyntheticTurnClaim::new(relay_owner, true, start_offset)
}

async fn original_source_matches(
    fence: &super::super::admission_fence::AdmissionFence<'_>,
    output: Option<&Path>,
    lease: &ExternalInputRelayLease,
) -> bool {
    match (lease.hosted_source.as_ref(), output) {
        (Some(pin), Some(path)) => {
            fence
                .matches_source(path, Some(&pin.source.session_id))
                .await
        }
        (Some(_), None) => false,
        (None, _) => true,
    }
}
