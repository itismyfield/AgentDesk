use super::*;

/// Persist a durable pending-start record and spawn the detached per-channel
/// worker. Returns immediately (non-blocking for the observer loop).
pub(in crate::services::discord::tui_prompt_relay) fn defer_synthetic_turn_start(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    anchor_message_id: MessageId,
    lease: &ExternalInputRelayLease,
    captured_source: Option<(String, u64)>,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let record = super::super::super::tui_direct_pending_start::TuiDirectPendingStart {
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
        state: super::super::super::tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
        captured_source,
        native_turn_id: prompt.native_turn_id.clone(),
        hosted_source: lease.hosted_source.clone(),
        hosted_source_checked: true,
    };
    if let Err(error) = super::super::super::tui_direct_pending_start::persist(&record) {
        tracing::warn!(
            provider = %record.provider,
            channel_id = record.channel_id,
            anchor_message_id = record.anchor_message_id,
            error = %error,
            "failed to persist durable TUI-direct pending-start record; spawning worker anyway off the in-memory presence index"
        );
    }
    super::super::super::tui_direct_pending_start::spawn_worker(
        shared.clone(),
        record,
        pending_start_view_fn(),
        pending_start_claim_fn(),
        pending_start_abort_cleanup_fn(),
        super::super::synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
}

/// The worker's per-poll view builder (see [`synthetic_start_prior_turn_view`]).
pub(in crate::services::discord::tui_prompt_relay) fn pending_start_view_fn()
-> super::super::super::tui_direct_pending_start::ViewFn {
    #[cfg(test)]
    if let Some(view) = RESTORE_VIEW_FOR_TEST.with(|slot| slot.borrow_mut().take()) {
        return view;
    }
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

/// Restore the original lease locally and run the common admission path.
/// The provider prompt is never resubmitted.
pub(in crate::services::discord::tui_prompt_relay) fn pending_start_claim_fn()
-> super::super::super::tui_direct_pending_start::ClaimFn {
    #[cfg(test)]
    if let Some(claim) = RESTORE_CLAIM_FOR_TEST.with(|slot| slot.borrow_mut().take()) {
        return claim;
    }
    let reported = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Box::new(move |shared, record| {
        let reported = reported.clone();
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
            lease.hosted_source = record.hosted_source.clone();
            if !record.hosted_source_checked
                && record.hosted_source.is_none()
                && record.captured_source.is_some()
            {
                lease.hosted_source = original_pending_source(&provider, record);
                if lease.hosted_source.is_none() && legacy_herdr_pending(shared, record).await {
                    // A worker owns one record; its bounded retries report the same Hold once.
                    if !reported.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        tracing::info!(
                            event = "legacy_herdr_pending_origin_unproven",
                            channel_id = record.channel_id,
                            pending_id = record.anchor_message_id,
                            captured_path_present = record.captured_source.is_some(),
                            "legacy_herdr_pending_origin_unproven"
                        );
                    }
                    return false;
                }
            }
            lease.turn_id = record.lease_turn_id.clone();
            lease.session_key = record.lease_session_key.clone();
            lease.relay_owner = parse_external_input_relay_owner(&record.lease_relay_owner);
            lease.runtime_kind = record
                .lease_runtime_kind
                .as_deref()
                .and_then(RuntimeHandoffKind::from_str);
            if record.captured_source.is_some()
                && crate::services::tui_prompt_dedupe::external_input_relay_lease(
                    provider.as_str(),
                    &record.tmux_session_name,
                    record.channel_id,
                )
                .is_some_and(|current| {
                    current.turn_id != lease.turn_id
                        && !matches!((&current.hosted_source, &lease.hosted_source),
                            (Some(previous), Some(original)) if previous != original)
                })
            {
                return false;
            }

            let claim = claim::claim_tui_direct_synthetic_turn_inner::<true>(
                shared,
                &provider,
                channel_id,
                &record.tmux_session_name,
                (&record.prompt_text, anchor_message_id),
                &lease,
                (
                    record.captured_source.as_ref(),
                    record.native_turn_id.as_deref(),
                ),
            )
            .await
            .0;

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
                // Admission published the validated owner before releasing its transition.
                // The tail reads that lease instead of the worker's unrecorded copy.
                let mut tail_lease =
                    crate::services::tui_prompt_dedupe::external_input_relay_lease(
                        provider.as_str(),
                        &record.tmux_session_name,
                        record.channel_id,
                    )
                    .unwrap_or_else(|| lease.clone());
                tail_lease.relay_owner = claim.relay_owner;
                // `observed_at` is bookkeeping only: the tail starts at the claim's post-drain EOF
                // offset below, because a timestamp scan would skip bytes written while deferred.
                let observed = ObservedTuiPrompt {
                    provider: record.provider.clone(),
                    tmux_session_name: record.tmux_session_name.clone(),
                    prompt: record.prompt_text.clone(),
                    source_event_id: None,
                    observed_at: chrono::Utc::now(),
                    external_input_lease_generation:
                        crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
                    ssh_direct_observation_generation:
                        crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
                    hook_prompt_id: None,
                    row_prompt_id: None,
                    native_turn_id: None,
                    steer_echo: false,
                    hosted_source: lease.hosted_source.clone(),
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

async fn legacy_herdr_pending(
    shared: &Arc<SharedData>,
    record: &super::super::super::tui_direct_pending_start::TuiDirectPendingStart,
) -> bool {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, load_hosted_execution_pg,
    };
    use crate::services::tui_prompt_dedupe::hosted_source::herdr_evidence;

    // A read error keeps the legacy origin unresolved rather than proving a different host.
    if herdr_evidence(record.channel_id, &record.tmux_session_name, None).unwrap_or(true) {
        return true;
    }
    let (Some(pool), Some(key)) = (shared.pg_pool.as_ref(), record.lease_session_key.as_deref())
    else {
        return false;
    };
    match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await {
        HostedLookup::Found(observed) => herdr_evidence(
            record.channel_id,
            &record.tmux_session_name,
            Some(&observed.record),
        )
        .unwrap_or(true),
        HostedLookup::Missing => false,
        _ => true,
    }
}

/// Only the same saved external episode can supply a missing legacy execution pin.
fn original_pending_source(
    provider: &ProviderKind,
    record: &super::super::super::tui_direct_pending_start::TuiDirectPendingStart,
) -> Option<crate::services::tui_prompt_dedupe::hosted_source::HostedSourcePin> {
    fn nonempty(value: Option<&str>) -> Option<&str> {
        value.filter(|value| !value.is_empty())
    }
    let turn = nonempty(record.lease_turn_id.as_deref())?;
    let key = nonempty(record.lease_session_key.as_deref())?;
    let captured = record.captured_source.as_ref()?;
    let original =
        super::super::super::inflight::load_inflight_state_read_only(provider, record.channel_id)?;
    if original.provider != provider.as_str()
        || original.channel_id != record.channel_id
        || original.turn_source != TurnSource::ExternalInput
        || original.tmux_session_name.as_deref() != Some(record.tmux_session_name.as_str())
        || original.user_msg_id != record.anchor_message_id
        || original.external_turn_id.as_deref() != Some(turn)
        || original.session_key.as_deref() != Some(key)
        || original.output_path.as_deref() != Some(captured.0.as_str())
        || !original.hosted_record_id.is_some_and(|id| id > 0)
    {
        return None;
    }
    crate::services::tui_prompt_dedupe::hosted_source::restore(
        provider.as_str(),
        &record.tmux_session_name,
        record.channel_id,
        captured,
        (
            nonempty(original.session_id.as_deref())?,
            nonempty(original.hosted_execution_nonce.as_deref())?,
        ),
    )
}
