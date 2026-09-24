use super::*;

pub(super) mod bridge_handoff;
pub(in crate::services::discord) mod claim;
mod stale_reclaim;
pub(in crate::services::discord) use claim::build_tui_direct_synthetic_inflight_state;
pub(super) use claim::{claim_tui_direct_synthetic_turn, claim_tui_direct_synthetic_turn_inner};

use stale_reclaim::release_reclaimable_stale_synthetic_mailbox_owner_if_current;
pub(super) use stale_reclaim::{
    finish_tui_direct_synthetic_pre_save_failure,
    release_stale_ownerless_tui_direct_mailbox_if_current,
};

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

async fn claim_tui_direct_synthetic_turn_prepared(
    preparation: claim::SyntheticClaimPreparation<'_>,
) -> TuiDirectSyntheticTurnClaim {
    let claim::SyntheticClaimPreparation {
        identity,
        output_path,
        start_offset,
        relay_owner,
        relay_owner_kind,
    } = preparation;

    let claim::SyntheticClaimIdentity {
        shared,
        provider,
        channel_id,
        tmux_session_name,
        prompt_text,
        anchor_message_id,
        lease,
        register_deferred_start: _,
    } = identity;

    let (cancel_token, pg_pin) = match bridge_handoff::prepare_admission(
        shared,
        provider,
        channel_id,
        anchor_message_id,
        lease,
    )
    .await
    {
        Ok(admission) => admission,
        Err(error) => {
            tracing::warn!(%error, "synthetic ownership capture failed before admission");
            return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
        }
    };
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
    let mut mailbox_activation_occurred = started;
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
    let active_snapshot = super::super::mailbox_snapshot(shared, channel_id).await;
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

    // The recap work above can yield to a successor. The original allocation,
    // never a later same-nonce snapshot, owns this save and its failure cleanup.
    if !bridge_handoff::actor_is_current(shared, channel_id, anchor_message_id, &admitted_actor)
        .await
    {
        return TuiDirectSyntheticTurnClaim::new(relay_owner, false, start_offset);
    }

    if let Some(existing) = super::super::inflight::load_inflight_state(provider, channel_id.get())
        && existing.tmux_session_name.as_deref() == Some(tmux_session_name)
        && existing.turn_source == TurnSource::ExternalInput
        && existing.user_msg_id == anchor_message_id.get()
        && existing.output_path.as_deref().map(Path::new) == output_path.as_deref()
        && existing.external_turn_id == lease.turn_id
        && existing.session_key == lease.session_key
        && bridge_handoff::refresh_actor_matches(
            &existing,
            Some(&admitted_actor),
            mailbox_activation_occurred,
        )
    {
        return bridge_handoff::refresh_existing(
            shared,
            existing,
            lease,
            relay_owner,
            relay_owner_kind,
            (Some(&admitted_actor), mailbox_activation_occurred),
            pg_pin,
        )
        .await;
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
    // #4002/#4082: lower-level safety. Normal wiring gates neutral continuation
    // records before this point; if a suppressing class reaches the raw claim API,
    // keep it relay-ownership-only so watcher completion Path B skips it.
    inflight_state.relay_ownership_only =
        classify_injected_prompt(prompt_text).suppresses_user_turn_lifecycle();
    match super::super::inflight::save_inflight_state_if_absent(&inflight_state) {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                "skipped TUI-direct synthetic inflight because a durable row already exists"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{self, InflightTurnState, RelayOwnerKind, TurnSource};
    use crate::services::discord::mailbox_try_start_turn_kinded;
    use crate::services::turn_orchestrator::ActiveTurnKind;
    use ::serenity::model::id::{MessageId, UserId};

    fn synthetic_owner() -> UserId {
        UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID)
    }

    fn old_owner_started_at() -> Option<chrono::DateTime<chrono::Utc>> {
        Some(
            chrono::Utc::now()
                - chrono::Duration::seconds(
                    stale_reclaim::STALE_SYNTHETIC_MAILBOX_OWNER_MIN_AGE_SECS + 1,
                ),
        )
    }

    fn young_owner_started_at() -> Option<chrono::DateTime<chrono::Utc>> {
        Some(chrono::Utc::now())
    }

    fn synthetic_state(
        channel_id: ChannelId,
        user_msg_id: MessageId,
        tmux_session_name: &str,
        terminal_delivery_committed: bool,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            None,
            TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
            user_msg_id.get(),
            0,
            "This session is being continued from a previous conversation".to_string(),
            Some("session-4018".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-4018.jsonl".to_string()),
            None,
            0,
        );
        state.turn_source = TurnSource::ExternalInput;
        state.relay_ownership_only = true;
        state.terminal_delivery_committed = terminal_delivery_committed;
        state.injected_prompt_message_id = Some(user_msg_id.get());
        state
    }

    /// #5981 — a stream tick can be the first write to land the native SID on
    /// the durable row. The allocation witness has to ride along with it: once
    /// the row is ahead of the birth-time witness, every later
    /// `preserve_admitted_source` compares an already-advanced row, declines,
    /// and the dormant claim a lost source falls back on refuses the very
    /// episode it owns.
    #[test]
    fn stream_tick_stamp_carries_the_allocation_witness() {
        use inflight::InflightEpisodePin;
        let root = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(root.path());

        let channel = ChannelId::new(5_981_101);
        let anchor = MessageId::new(5_981_102);
        let mut admitted = synthetic_state(channel, anchor, "witness-5981", false);
        admitted.relay_ownership_only = false;
        admitted.session_id = None;
        admitted.current_msg_id = 5_981_103;
        admitted.external_turn_id = Some("external-witness-5981".into());
        let actor = Arc::new(CancelToken::new());
        admitted.turn_nonce = actor.turn_nonce().map(str::to_owned);
        inflight::save_inflight_state(&admitted).expect("seed the admitted row");
        assert!(bridge_handoff::record(&admitted, Some(&actor), None));

        let before_pin = InflightEpisodePin::from_state(&admitted);
        let mut baseline = admitted.clone();
        let mut local = admitted.clone();
        local.session_id = Some("native-session-5981".into());
        assert_eq!(
            inflight::save_stream_tick_state_if_bridge_authority(
                &mut baseline,
                &mut local,
                &inflight::InflightTurnIdentity::from_state(&admitted),
                admitted.current_msg_id,
                admitted.current_msg_len,
                "test::5981_stream_tick_witness",
            ),
            inflight::GuardedSaveOutcome::Saved,
        );
        assert_eq!(local.session_id.as_deref(), Some("native-session-5981"));
        assert!(!before_pin.matches_state(&local));
        assert!(
            Arc::ptr_eq(
                &bridge_handoff::retained_actor(&local).unwrap().unwrap(),
                &actor
            ),
            "the witness follows its own episode across the stream-tick stamp"
        );

        // The same carry cannot hand a successor allocation A's proof. A
        // restart re-admits through `CancelToken::from_persisted_turn_nonce`,
        // so the nonce is the one birth axis a successor shares; each of the
        // rest has to refuse the carry on its own.
        let current = InflightEpisodePin::from_state(&local);
        let successors: [(&str, fn(&mut InflightTurnState)); 5] = [
            ("turn_nonce", |row| {
                row.turn_nonce = Some("5981-successor-nonce".into());
            }),
            ("born_generation", |row| row.born_generation += 1),
            ("started_at", |row| {
                row.started_at = "1970-01-01 00:00:01".into();
            }),
            ("user_msg_id", |row| row.user_msg_id += 1),
            ("request_owner_user_id", |row| {
                row.request_owner_user_id += 1
            }),
        ];
        for (axis, mutate) in successors {
            let mut successor = local.clone();
            mutate(&mut successor);
            bridge_handoff::preserve_stamped_source(&current, &successor);
            assert!(
                bridge_handoff::retained_actor(&successor).is_err(),
                "a successor differing only in {axis} adopted the live witness"
            );
            assert!(
                Arc::ptr_eq(
                    &bridge_handoff::retained_actor(&local).unwrap().unwrap(),
                    &actor
                ),
                "a successor differing only in {axis} displaced the live witness"
            );
        }
    }

    /// #5981 follow-up — `CLAIMS` is keyed by `(provider, channel_id)` alone, so
    /// a witness can outlive the allocation that installed it and still be the
    /// only entry a later allocation on that channel finds. Repainting it onto
    /// the newcomer's row would leave the first allocation's actor attached,
    /// and `retained_actor` would then hand that actor to the newcomer.
    #[test]
    fn stamped_source_witness_refuses_a_foreign_allocation_on_the_same_channel() {
        use inflight::InflightEpisodePin;
        let channel = ChannelId::new(5_984_101);
        let mut first =
            synthetic_state(channel, MessageId::new(5_984_102), "witness-5984-a", false);
        first.relay_ownership_only = false;
        first.current_msg_id = 5_984_103;
        first.external_turn_id = Some("external-witness-5984-a".into());
        let first_actor = Arc::new(CancelToken::new());
        first.turn_nonce = first_actor.turn_nonce().map(str::to_owned);
        assert!(bridge_handoff::record(&first, Some(&first_actor), None));

        // The second allocation never lands a witness of its own: `record`
        // refuses a row whose external turn id is not published yet.
        let mut second =
            synthetic_state(channel, MessageId::new(5_984_202), "witness-5984-b", false);
        second.relay_ownership_only = false;
        second.current_msg_id = 5_984_203;
        second.born_generation = first.born_generation + 1;
        let second_actor = Arc::new(CancelToken::new());
        second.turn_nonce = second_actor.turn_nonce().map(str::to_owned);
        assert!(!bridge_handoff::record(&second, Some(&second_actor), None));

        let before = InflightEpisodePin::from_state(&second);
        let mut stamped = second.clone();
        stamped.session_id = Some("native-session-5984-b".into());
        assert!(
            before.is_same_episode_as(&InflightEpisodePin::from_state(&stamped)),
            "the second allocation's own tick must reach the witness filter"
        );
        bridge_handoff::preserve_stamped_source(&before, &stamped);
        assert!(
            bridge_handoff::retained_actor(&stamped).is_err(),
            "the second allocation adopted the first one's surviving actor"
        );
        assert!(Arc::ptr_eq(
            &bridge_handoff::retained_actor(&first).unwrap().unwrap(),
            &first_actor
        ));
    }

    /// #5981 follow-up — `is_same_episode_as` deliberately leaves relay
    /// ownership out, so a tick that hands the relay to a watcher still reads
    /// as the same episode. `record` declines to witness a delegated row at
    /// all; the stamped carry has to decline it too, or a witness rides across
    /// the handoff onto a row that never earned one.
    #[test]
    fn stamped_source_witness_refuses_a_delegated_relay_row() {
        use inflight::InflightEpisodePin;
        let channel = ChannelId::new(5_984_301);
        let mut row = synthetic_state(channel, MessageId::new(5_984_302), "witness-5984-c", false);
        row.relay_ownership_only = false;
        row.current_msg_id = 5_984_303;
        row.external_turn_id = Some("external-witness-5984-c".into());
        let actor = Arc::new(CancelToken::new());
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        assert!(bridge_handoff::record(&row, Some(&actor), None));
        let before = InflightEpisodePin::from_state(&row);

        let mut delegated = row.clone();
        delegated.session_id = Some("native-session-5984-c".into());
        delegated.watcher_owns_live_relay = true;
        assert_eq!(
            delegated.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert!(
            before.is_same_episode_as(&InflightEpisodePin::from_state(&delegated)),
            "the handoff tick must reach the witness filter"
        );
        bridge_handoff::preserve_stamped_source(&before, &delegated);
        assert!(
            bridge_handoff::retained_actor(&delegated).is_err(),
            "the witness followed the relay handoff onto a delegated row"
        );
        assert!(Arc::ptr_eq(
            &bridge_handoff::retained_actor(&row).unwrap().unwrap(),
            &actor
        ));
    }

    #[test]
    fn admitted_source_witness_advances_only_the_original_allocation() {
        use inflight::InflightEpisodePin;
        let mut before = synthetic_state(
            ChannelId::new(5_071_581),
            MessageId::new(5_071_582),
            "witness-5071",
            false,
        );
        before.relay_ownership_only = false;
        before.session_id = None;
        before.output_path = Some("/var/tmp/witness-5071.jsonl".into());
        before.external_turn_id = Some("external-witness-5071".into());
        let original = Arc::new(CancelToken::new());
        before.turn_nonce = original.turn_nonce().map(str::to_owned);
        let before_pin = InflightEpisodePin::from_state(&before);
        let mut admitted = before.clone();
        admitted.session_id = Some("native-session-5071".into());
        admitted.output_path = Some("/private/var/tmp/witness-5071.jsonl".into());

        bridge_handoff::preserve_admitted_source(&before_pin, &admitted, &original);
        assert!(bridge_handoff::retained_actor(&admitted).unwrap().is_none());
        assert!(bridge_handoff::record(&before, Some(&original), None));
        bridge_handoff::preserve_admitted_source(&before_pin, &admitted, &original);
        assert!(Arc::ptr_eq(
            &bridge_handoff::retained_actor(&admitted).unwrap().unwrap(),
            &original
        ));
        assert!(matches!(
            bridge_handoff::original_session_pin(&admitted, Some(&original)),
            Some(None)
        ));

        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(
            before.turn_nonce.clone(),
        ));
        let mut next = admitted.clone();
        next.current_msg_id = 5_071_583;
        // Even equal nonces cannot let B advance A's current witness.
        bridge_handoff::preserve_admitted_source(
            &InflightEpisodePin::from_state(&admitted),
            &next,
            &successor,
        );
        assert!(Arc::ptr_eq(
            &bridge_handoff::retained_actor(&admitted).unwrap().unwrap(),
            &original
        ));
        // A also cannot reuse its stale before-pin after admission.
        bridge_handoff::preserve_admitted_source(&before_pin, &next, &original);
        assert!(Arc::ptr_eq(
            &bridge_handoff::retained_actor(&admitted).unwrap().unwrap(),
            &original
        ));
        assert!(bridge_handoff::record(&next, Some(&successor), None));
        bridge_handoff::preserve_admitted_source(
            &InflightEpisodePin::from_state(&next),
            &admitted,
            &original,
        );
        assert!(Arc::ptr_eq(
            &bridge_handoff::retained_actor(&next).unwrap().unwrap(),
            &successor
        ));
    }

    async fn seed_synthetic_mailbox_owner(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn_kinded(
                shared,
                channel_id,
                token.clone(),
                synthetic_owner(),
                user_msg_id,
                ActiveTurnKind::Background,
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        token
    }

    #[tokio::test(flavor = "current_thread")]
    async fn young_rowless_synthetic_owner_is_not_reclaimed() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_200);
        let tmux = "AgentDesk-claude-4018-young";
        let stale_id = MessageId::new(4_018_300);
        let next_id = MessageId::new(4_018_400);
        let stale_token = seed_synthetic_mailbox_owner(&shared, channel_id, stale_id).await;

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            stale_id,
            Some(synthetic_owner()),
            ActiveTurnKind::Background,
            young_owner_started_at(),
            stale_token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(!reclaimed, "young row-less owner must not be reclaimed");
        assert!(!stale_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);

        let next_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            next_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(!next_claimed, "young owner must keep the mailbox slot");
    }

    /// #5996 — build the EXACT incident shape: a synthetic-owned mailbox whose
    /// durable inflight row is gone. The allocation is admitted through the real
    /// deferred claim path (an unrelated preseeded mailbox token cannot be
    /// adopted by the bridge), then the row is removed through
    /// `clear_inflight_state_for_captured_episode` — the same primitive
    /// `turn_finalizer::episode::claim_normal_episode`'s `clear_inflight` arm
    /// uses, which is the production shape that strands a held mailbox with no
    /// row. (The watcher's terminal-commit pass reaches the same
    /// `..._turn_nonce_impl_in_root` body through
    /// `clear_inflight_state_if_matches_identity_turn_nonce`, differing only in
    /// `exact_nonce`.)
    async fn seed_rowless_synthetic_owner(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        tmux: &str,
        stale_id: MessageId,
        lease_turn_id: &str,
    ) -> Arc<CancelToken> {
        let record = crate::services::discord::tui_direct_pending_start::TuiDirectPendingStart {
            provider: provider.as_str().into(),
            channel_id: channel_id.get(),
            tmux_session_name: tmux.into(),
            prompt_text: "continue".into(),
            anchor_message_id: stale_id.get(),
            lease_relay_owner: ExternalInputRelayOwner::BridgeAdapter.as_str().into(),
            lease_runtime_kind: None,
            lease_turn_id: Some(lease_turn_id.into()),
            lease_session_key: None,
            generation: shared.restart.current_generation,
            created_at_ms: 0,
            observed_at_ms: 0,
            state: crate::services::discord::tui_direct_pending_start::PendingStartState::Waiting,
            attempt_count: 0,
            captured_source: None,
        };
        assert!(pending_start_claim_fn()(shared, &record).await);
        let stale_token = crate::services::discord::mailbox_snapshot(shared, channel_id)
            .await
            .cancel_token
            .expect("deferred claim owns the mailbox");
        let row = inflight::load_inflight_state(provider, channel_id.get()).unwrap();
        assert_eq!(row.turn_nonce.as_deref(), stale_token.turn_nonce());
        assert_eq!(
            inflight::clear_inflight_state_for_captured_episode(
                provider,
                channel_id.get(),
                &inflight::InflightTurnIdentity::from_state(&row),
                stale_token.turn_nonce(),
            ),
            inflight::GuardedClearOutcome::Cleared
        );
        assert!(
            inflight::load_inflight_state(provider, channel_id.get()).is_none(),
            "#5996 precondition: the mailbox is synthetic-owned and ROWLESS"
        );
        stale_token
    }

    /// #5996 / contract §I20: a synthetic-owned ROWLESS mailbox carries NO
    /// readable progress witness — the row's `terminal_delivery_committed` bit
    /// went with the row, and `readopted_mailbox_ledger` can never hold the
    /// #4018 owner (`readopt_marker_eligible_real_user` excludes it at every
    /// insert). Absence is the UNMEASURED case, so the reclaim is REFUSED and
    /// the mailbox stays held. Until #5996 this same aged fixture retired the
    /// owner on the `>= 120s` clock alone.
    ///
    /// What recovers the held mailbox in PRODUCTION is
    /// `turn_finalizer::reconcile::reconcile_guarded_finish_residues`, on the
    /// finalizer actor's 1s `RECONCILE_INTERVAL` — not anything this fixture
    /// leaves standing. For a strand produced by `claim_normal_episode`'s
    /// `clear_inflight` arm (its two earlier arms return `Err(())` WITHOUT
    /// clearing, so reaching the clear is by construction a SAME-EPISODE miss)
    /// `do_finalize` recorded a `GuardedFinishResidue`, the reconciler's
    /// `release_authorized = same_terminal_episode || token.cancelled` holds on
    /// its first conjunct, and only `tui_structurally_idle` is conditional —
    /// holding a BUSY pane being the correct answer. Strands produced by row
    /// deletes that never consult the mailbox leave no residue; that is a gap
    /// in the reaper, not in this refusal, and it is reported separately.
    #[tokio::test(flavor = "current_thread")]
    async fn aged_rowless_synthetic_owner_is_refused_for_want_of_a_witness() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Codex;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_201);
        let tmux = "AgentDesk-codex-4018-aged";
        let stale_id = MessageId::new(4_018_301);
        let next_id = MessageId::new(4_018_401);
        let stale_token = seed_rowless_synthetic_owner(
            &shared,
            &provider,
            channel_id,
            tmux,
            stale_id,
            "external:codex:4018-aged",
        )
        .await;
        let active_before = shared.restart.global_active.load(Ordering::Relaxed);
        assert_eq!(active_before, 1, "the deferred claim holds one active slot");

        // #5996 r3 / FORWARD direction of the mutant pair. Put on disk the one
        // thing production most plausibly HAS in this shape and that an
        // exhaustiveness claim over readable witnesses must account for: the
        // provider's own JSONL transcript carrying the authoritative turn-END
        // terminator. The reader for it is reachable from the reclaim's module
        // (`pub(in crate::services::discord)`), and it reads `Done` right here —
        // asserted, not assumed. The refusal below must nevertheless stay GREEN,
        // because that scan is a REVERSE walk for the most recent terminator
        // over a tail window with no turn key: on a session shared with a
        // successor it reports the SUCCESSOR's state, and the attribution its own
        // doc points at (`pinned_finalize_user_msg_id` /
        // `committed_completion_is_stale_for_newer_turn`, both taking the
        // "pre-cleanup inflight snapshot") is exactly what this shape destroyed.
        // If a later change ever wires this signal in as a witness, THIS
        // assertion pair is what must be revisited first.
        let transcript = root.path().join("4018-aged-turn-completed.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"cwd\":\"/repo\"}}\n\
             {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}\n",
        )
        .expect("write a Done provider transcript");
        assert_eq!(
            crate::services::discord::turn_finalizer::completion_signal_from_transcript(
                &provider,
                Some(crate::services::agent_protocol::RuntimeHandoffKind::CodexTui),
                &transcript,
            ),
            crate::services::discord::turn_finalizer::CompletionSignal::Done,
            "the transcript axis is readable from here and says Done — the refusal below \
             must not be resting on that axis being unreachable"
        );

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            stale_id,
            Some(synthetic_owner()),
            ActiveTurnKind::Background,
            old_owner_started_at(),
            stale_token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(
            !reclaimed,
            "an aged row-less SYNTHETIC owner has no witness to retire it on (#5996 §I20)"
        );
        assert!(
            !stale_token.cancelled.load(Ordering::Relaxed),
            "the refusal must not cancel a turn it cannot prove finished"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(
            shared
                .turn_finalizer
                .has_live_watcher_pending(channel_id, shared.restart.current_generation)
                .await,
            "FIXTURE STATE ONLY: the deferred claim's Pending entry outlives the refusal \
             here, but it is NOT what recovers production — the residue reconciler is \
             (see this test's doc comment)"
        );

        let next_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            next_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(
            !next_claimed,
            "the refused mailbox stays held; the new synthetic turn is deferred, not admitted"
        );
    }

    /// #5996 / §I20 — a row whose `user_msg_id` has moved on belongs to ANOTHER
    /// turn and witnesses nothing about this owner, so `OwnerInflightReplaced`
    /// is refused for the synthetic owner too. The reason variant is kept
    /// (deleting it would let the successor's `terminal_delivery_committed` bit
    /// retire this owner instead).
    #[tokio::test(flavor = "current_thread")]
    async fn aged_synthetic_owner_replaced_row_is_refused_for_want_of_a_witness() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_205);
        let tmux = "AgentDesk-claude-4018-replaced-same";
        let stale_id = MessageId::new(4_018_306);
        let replacement_id = MessageId::new(4_018_307);
        let next_id = MessageId::new(4_018_405);
        let stale_token = seed_synthetic_mailbox_owner(&shared, channel_id, stale_id).await;
        // Same tmux session, a successor `user_msg_id`, and the successor's own
        // terminal bit set — none of which is evidence about THIS owner.
        let state = synthetic_state(channel_id, replacement_id, tmux, true);
        inflight::save_inflight_state(&state).expect("save same-session replacement row");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            stale_id,
            Some(synthetic_owner()),
            ActiveTurnKind::Background,
            old_owner_started_at(),
            stale_token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(
            !reclaimed,
            "another turn's row is not this owner's progress witness (#5996 §I20)"
        );
        assert!(!stale_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    // Keep the global event ring isolated while checking both invariant outcomes.
    #[test]
    fn witnessless_reclaim_records_the_i20_violation_and_a_witnessed_one_does_not() {
        let _telemetry = crate::services::observability::lock_env_then_runtime();
        crate::services::observability::reset_for_tests();
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime")
            .block_on(witnessless_reclaim_violation_body());
    }

    async fn witnessless_reclaim_violation_body() {
        let provider = ProviderKind::Codex;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let refused_channel = ChannelId::new(4_018_206);
        let witnessed_channel = ChannelId::new(4_018_207);
        let tmux = "AgentDesk-codex-4018-i20";
        let stale_id = MessageId::new(4_018_308);
        let next_id = MessageId::new(4_018_406);

        let stale_token = seed_rowless_synthetic_owner(
            &shared,
            &provider,
            refused_channel,
            tmux,
            stale_id,
            "external:codex:4018-i20",
        )
        .await;
        assert!(
            !release_reclaimable_stale_synthetic_mailbox_owner_if_current(
                &shared,
                &provider,
                refused_channel,
                tmux,
                stale_id,
                Some(synthetic_owner()),
                ActiveTurnKind::Background,
                old_owner_started_at(),
                stale_token.turn_nonce().map(str::to_owned),
                next_id,
            )
            .await
        );

        // #5996 M1 pin. The same owner, the same rowless state, the same absent
        // witness as the call above — only the clock differs. The observation
        // block sits AFTER the age gate, so this call is refused upstream of the
        // grade and must contribute ZERO violations. Hoisting the block above
        // the age gate makes this call emit one, and the count below goes to 2:
        // that is the burial §I20 forbids when it requires an age fallback
        // "must be countable apart from a witness". The placement is held by a
        // number here, not by a comment at the call site.
        assert!(
            !release_reclaimable_stale_synthetic_mailbox_owner_if_current(
                &shared,
                &provider,
                refused_channel,
                tmux,
                stale_id,
                Some(synthetic_owner()),
                ActiveTurnKind::Background,
                young_owner_started_at(),
                stale_token.turn_nonce().map(str::to_owned),
                next_id,
            )
            .await,
            "a young owner is refused by the age gate, upstream of the witness grade"
        );

        // `synthetic_state` builds a Claude row, so the witnessed arm reads
        // Claude; the rowless arm above keeps Codex. Distinct channels either
        // way, and the point is the reason, not the provider.
        let witnessed_provider = ProviderKind::Claude;
        let witnessed_tmux = "AgentDesk-claude-4018-i20-witnessed";
        let witnessed_token =
            seed_synthetic_mailbox_owner(&shared, witnessed_channel, stale_id).await;
        let committed = synthetic_state(witnessed_channel, stale_id, witnessed_tmux, true);
        inflight::save_inflight_state(&committed).expect("save finalized synthetic inflight");
        assert!(
            release_reclaimable_stale_synthetic_mailbox_owner_if_current(
                &shared,
                &witnessed_provider,
                witnessed_channel,
                witnessed_tmux,
                stale_id,
                Some(synthetic_owner()),
                ActiveTurnKind::Background,
                old_owner_started_at(),
                witnessed_token.turn_nonce().map(str::to_owned),
                next_id,
            )
            .await,
            "the row bit is a readable witness and must still retire without a clock"
        );

        let events = crate::services::observability::events::recent(200);
        // Scope to the two channels THIS test drives. `recent` reads the
        // process-global ring that sibling fixtures also write, so an unscoped
        // count asserts over their output too: it dies for reasons that are not
        // this test's, and a mutant it should survive kills it anyway.
        let violations: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "invariant_violation")
            .filter(|event| {
                event.payload["invariant"] == stale_reclaim::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT
            })
            .filter(|event| {
                event.channel_id == Some(refused_channel.get())
                    || event.channel_id == Some(witnessed_channel.get())
            })
            .collect();
        assert_eq!(
            violations.len(),
            1,
            "exactly the aged witnessless call violates — the witnessed call has a \
             readable witness and the young call never reaches the grade; present: {events:?}"
        );
        let violation = violations[0];
        assert_eq!(violation.channel_id, Some(refused_channel.get()));
        assert_eq!(
            violation.payload["details"]["decided_by"],
            "no_readable_witness"
        );
        assert_eq!(
            violation.payload["details"]["reclaim_reason"],
            "owner_inflight_absent"
        );
        assert_eq!(
            violation.payload["details"]["reclaimable_owner"],
            "synthetic_owner"
        );
        assert_eq!(violation.payload["details"]["retired"], false);
        assert_eq!(
            stale_reclaim::LIVE_TURN_PROVEN_BY_PROGRESS_INVARIANT,
            "live_turn_proven_by_progress_not_presence",
            "the invariant key is the alert table's status filter and §I20's key; a status \
             absent from `RELAY_SIGNAL_DEFINITIONS` counts zero forever (#5175)"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn monitor_auto_turn_slot_is_not_reclaimed_even_when_aged() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_202);
        let tmux = "AgentDesk-claude-4018-monitor";
        let monitor_id = MessageId::new(4_018_302);
        let next_id = MessageId::new(4_018_402);
        let token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                token.clone(),
                synthetic_owner(),
                monitor_id,
                ActiveTurnKind::MonitorAutoTurn,
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            monitor_id,
            Some(synthetic_owner()),
            ActiveTurnKind::MonitorAutoTurn,
            old_owner_started_at(),
            token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(
            !reclaimed,
            "monitor auto-turn must not be reclaimed by the synthetic stale-owner path"
        );
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_synthetic_owner_finalized_row_reclaims_for_new_turn() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_203);
        let tmux = "AgentDesk-claude-4018-finalized";
        let stale_id = MessageId::new(4_018_303);
        let next_id = MessageId::new(4_018_403);
        let stale_token = seed_synthetic_mailbox_owner(&shared, channel_id, stale_id).await;
        let state = synthetic_state(channel_id, stale_id, tmux, true);
        inflight::save_inflight_state(&state).expect("save finalized synthetic inflight");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            stale_id,
            Some(synthetic_owner()),
            ActiveTurnKind::Background,
            young_owner_started_at(),
            stale_token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(
            reclaimed,
            "finalized synthetic inflight row remains positively stale without age gate"
        );
        assert!(stale_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

        let next_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            next_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(next_claimed, "new synthetic turn must claim after reclaim");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_synthetic_owner_replaced_requires_same_tmux_session() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_204);
        let tmux = "AgentDesk-claude-4018-replaced";
        let stale_id = MessageId::new(4_018_304);
        let replacement_id = MessageId::new(4_018_305);
        let next_id = MessageId::new(4_018_404);
        let stale_token = seed_synthetic_mailbox_owner(&shared, channel_id, stale_id).await;
        let state = synthetic_state(channel_id, replacement_id, "AgentDesk-other-session", false);
        inflight::save_inflight_state(&state).expect("save foreign-session replacement row");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            stale_id,
            Some(synthetic_owner()),
            ActiveTurnKind::Background,
            old_owner_started_at(),
            stale_token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(
            !reclaimed,
            "replacement in a different tmux session must not declare this owner stale"
        );
        assert!(!stale_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_synthetic_owner_live_inflight_still_skips_reclaim() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_260);
        let tmux = "AgentDesk-claude-4018-live";
        let live_id = MessageId::new(4_018_360);
        let next_id = MessageId::new(4_018_460);
        let live_token = seed_synthetic_mailbox_owner(&shared, channel_id, live_id).await;
        let state = synthetic_state(channel_id, live_id, tmux, false);
        inflight::save_inflight_state(&state).expect("save live synthetic inflight");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            live_id,
            Some(synthetic_owner()),
            ActiveTurnKind::Background,
            old_owner_started_at(),
            live_token.turn_nonce().map(str::to_owned),
            next_id,
        )
        .await;
        assert!(!reclaimed, "live synthetic owner must not be reclaimed");
        assert!(!live_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);

        let next_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            next_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(
            !next_claimed,
            "new synthetic turn must still skip while owner is live"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claim_adopting_existing_mailbox_does_not_increment_global_active() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_019_230);
        let tmux = "AgentDesk-claude-4019-adopt";
        let anchor_id = MessageId::new(4_019_330);
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
        lease.session_key = Some("session-4019-adopt".to_string());
        lease.turn_id = Some("external:claude:4019-adopt".to_string());
        // The first admission records the exact actor/episode witness that a
        // subsequent claim must reuse; matching message IDs alone are insufficient.
        assert!(
            claim_tui_direct_synthetic_turn(
                &shared, &provider, channel_id, tmux, "continue", anchor_id, &lease,
            )
            .await
            .claimed
        );
        let token = crate::services::discord::mailbox_snapshot(&shared, channel_id)
            .await
            .cancel_token
            .expect("original admitted actor");
        let original = inflight::load_inflight_state(&provider, channel_id.get()).unwrap();
        shared.restart.global_active.store(0, Ordering::Relaxed);
        let claim = claim_tui_direct_synthetic_turn(
            &shared, &provider, channel_id, tmux, "continue", anchor_id, &lease,
        )
        .await;

        assert!(
            claim.claimed,
            "claim should adopt the existing matching mailbox"
        );
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            0,
            "adoption must not increment global_active without a mailbox activation"
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.active_user_message_id, Some(anchor_id));
        assert_eq!(snapshot.active_turn_nonce.as_deref(), token.turn_nonce());
        assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &token));
        let persisted = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("adopted synthetic inflight must persist");
        assert_eq!(persisted.turn_nonce.as_deref(), token.turn_nonce());
        assert_eq!(persisted.external_turn_id, original.external_turn_id);
        assert_eq!(persisted.turn_start_offset, original.turn_start_offset);
        assert_eq!(persisted.last_offset, original.last_offset);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claim_rejects_existing_mailbox_without_original_actor_witness() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(5_071_923);
        let tmux = "AgentDesk-5071-foreign-adoption";
        let anchor_id = MessageId::new(5_071_924);
        let token = seed_synthetic_mailbox_owner(&shared, channel_id, anchor_id).await;
        let mut state = synthetic_state(channel_id, anchor_id, tmux, false);
        state.turn_nonce = Some("stale-row-episode".to_string());
        inflight::save_inflight_state(&state).expect("save foreign synthetic inflight");
        let original = inflight::load_inflight_state(&provider, channel_id.get()).unwrap();
        let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
        lease.session_key = Some("session-4019-adopt".to_string());

        let claim = claim_tui_direct_synthetic_turn(
            &shared, &provider, channel_id, tmux, "continue", anchor_id, &lease,
        )
        .await;

        assert!(
            !claim.claimed,
            "same message ID is not original actor authority"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert!(!token.cancelled.load(Ordering::Relaxed));
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &token));
        let persisted = inflight::load_inflight_state(&provider, channel_id.get()).unwrap();
        assert_eq!(
            serde_json::to_value(persisted).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn synthetic_finish_session_key_mismatch_preserves_newer_turn() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_019_231);
        let tmux = "AgentDesk-claude-4019-newer";
        let newer_id = MessageId::new(4_019_331);
        let newer_token = seed_synthetic_mailbox_owner(&shared, channel_id, newer_id).await;
        let mut state = synthetic_state(channel_id, newer_id, tmux, false);
        state.session_key = Some("session-4019-newer".to_string());
        inflight::save_inflight_state(&state).expect("save newer synthetic inflight");

        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            Some("session-4019-old"),
            "test_stale_tail_cleanup",
        )
        .await;

        let loaded = inflight::load_inflight_state(&provider, channel_id.get())
            .expect("newer inflight must survive stale cleanup");
        assert_eq!(loaded.user_msg_id, newer_id.get());
        assert_eq!(loaded.session_key.as_deref(), Some("session-4019-newer"));
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.active_user_message_id, Some(newer_id));
        assert!(!newer_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn synthetic_finish_routes_release_through_finalizer() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_019_232);
        let tmux = "AgentDesk-claude-4019-release";
        let turn_id = MessageId::new(4_019_332);
        let token = seed_synthetic_mailbox_owner(&shared, channel_id, turn_id).await;
        let mut state = synthetic_state(channel_id, turn_id, tmux, false);
        state.session_key = Some("session-4019-release".to_string());
        state.turn_nonce = token.turn_nonce().map(str::to_owned);
        inflight::save_inflight_state(&state).expect("save synthetic inflight");

        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            Some("session-4019-release"),
            "test_finalizer_release",
        )
        .await;

        assert!(
            inflight::load_inflight_state(&provider, channel_id.get()).is_none(),
            "identity-matched synthetic finish should clear its inflight row"
        );
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(snapshot.active_user_message_id, None);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }

    // ===================================================================
    // #4370 — restart-resume path. Generalises #4018's synthetic-owner-only
    // stale reclaim so a REAL user turn re-adopted across a dcserver restart
    // (owner == request_owner_user_id, `readopted_from_inflight` marker) can also
    // yield its stale mailbox to a starved injection / task-notification
    // synthetic relay turn — while a genuinely live re-adopted turn is never
    // stolen.
    // ===================================================================

    /// A non-synthetic (real) Discord user id. Must differ from
    /// `TUI_DIRECT_SYNTHETIC_OWNER_USER_ID` so the #4018 synthetic branch does
    /// not apply.
    fn real_owner() -> UserId {
        UserId::new(4_370_007)
    }

    /// A real-user in-flight turn (NOT relay-ownership-only): it owns its own
    /// completion lifecycle. `readopted_from_inflight` starts `false`; callers set it
    /// explicitly (unit tests) or let `reregister_active_turn_from_inflight`
    /// stamp it (the re-adopt integration test).
    fn real_user_inflight_state(
        channel_id: ChannelId,
        user_msg_id: MessageId,
        tmux_session_name: &str,
        terminal_delivery_committed: bool,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id.get(),
            None,
            real_owner().get(),
            user_msg_id.get(),
            user_msg_id.get(),
            "real user turn spanning a dcserver restart".to_string(),
            Some("session-4370".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/agentdesk-4370.jsonl".to_string()),
            None,
            0,
        );
        state.turn_source = TurnSource::ExternalInput;
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        state.terminal_delivery_committed = terminal_delivery_committed;
        // Invariant the fix relies on: a real re-adopted turn is NOT muted like a
        // relay-ownership-only compact-resume note.
        assert!(!state.relay_ownership_only);
        state
    }

    async fn seed_real_owner_mailbox(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        user_msg_id: MessageId,
    ) -> Arc<CancelToken> {
        let token = Arc::new(CancelToken::new());
        assert!(
            mailbox_try_start_turn_kinded(
                shared,
                channel_id,
                token.clone(),
                real_owner(),
                user_msg_id,
                ActiveTurnKind::UserOrAgent,
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        token
    }

    // Positive: a restart-re-adopted real-user owner that has committed its
    // terminal delivery (turn finished) but whose mailbox was never released is
    // reclaimable WITHOUT the age gate — exactly the #4018 finalized-row rule,
    // now reachable for a real owner. This is the transition that lets the
    // starved injection / task-notification turn win relay ownership.
    #[tokio::test(flavor = "current_thread")]
    async fn readopted_from_inflight_real_owner_committed_row_reclaims_for_starved_synthetic() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_050);
        let tmux = "AgentDesk-claude-4370-committed";
        let real_id = MessageId::new(4_370_150);
        let synth_id = MessageId::new(4_370_250);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        let mut state = real_user_inflight_state(channel_id, real_id, tmux, true);
        state.readopted_from_inflight = true;
        inflight::save_inflight_state(&state).expect("save committed re-adopted inflight");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            young_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            reclaimed,
            "a committed restart-re-adopted real-user owner must be reclaimable (#4370)"
        );
        assert!(real_token.cancelled.load(Ordering::Relaxed));

        // Mailbox-ownership transition: the synthetic relay turn now wins the
        // mailbox (relay ownership yielded) — its prose is no longer dropped.
        let synth_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            synth_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(
            synth_claimed,
            "the starved synthetic relay turn must claim the freed mailbox"
        );
        let after = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(after.active_request_owner, Some(synthetic_owner()));
        assert_eq!(after.active_user_message_id, Some(synth_id));
    }

    // Negative (REQUIRED): a genuinely live, progressing re-adopted turn —
    // matching `user_msg_id`, NOT `terminal_delivery_committed` — is never
    // stolen, even when aged and marked. Reclaim reason is `None`, so the
    // widened eligibility gate cannot cancel it. This is the guard against the
    // fix regressing into a live-turn thief.
    #[tokio::test(flavor = "current_thread")]
    async fn readopted_from_inflight_real_owner_live_turn_is_never_stolen() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_060);
        let tmux = "AgentDesk-claude-4370-live";
        let real_id = MessageId::new(4_370_160);
        let synth_id = MessageId::new(4_370_260);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        let mut state = real_user_inflight_state(channel_id, real_id, tmux, false);
        state.readopted_from_inflight = true;
        inflight::save_inflight_state(&state).expect("save live re-adopted inflight");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            old_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "a live progressing restart-re-adopted turn must never be stolen (#4370)"
        );
        assert!(!real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);

        let synth_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            synth_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(
            !synth_claimed,
            "the live re-adopted turn must keep the mailbox slot"
        );
    }

    // #4370 (fresh-Claude r3 #1). The mailbox's `active_user_message_id` is the turn's
    // `effective_finalizer_turn_id()`, which equals `user_msg_id` only when that id is
    // NON-zero. An id-0 marked row makes the two diverge, so the reason function would
    // read `state.user_msg_id != active_user_message_id` and misfire
    // `OwnerInflightReplaced` on a turn that is still LIVE, stealing it once aged past
    // 120s. `classify_reclaimable_mailbox_owner` must refuse id-0 rows outright — the
    // consumption-site half of the invariant that `readopted_ledger_record_allowed`
    // enforces at the recovery site.
    #[tokio::test(flavor = "current_thread")]
    async fn readopted_from_inflight_id0_row_is_never_reclaimed() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_070);
        let tmux = "AgentDesk-claude-4370-id0";
        let real_id = MessageId::new(4_370_170);
        let synth_id = MessageId::new(4_370_270);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;

        // A LIVE re-adopted row that carries the marker but has `user_msg_id == 0`
        // (an injected / task-notification shape). Aged well past the 120s gate.
        let mut state = real_user_inflight_state(channel_id, real_id, tmux, false);
        state.readopted_from_inflight = true;
        state.user_msg_id = 0;
        state.injected_prompt_message_id = Some(real_id.get());
        inflight::save_inflight_state(&state).expect("save id-0 re-adopted inflight");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            old_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "an id-0 re-adopted row must never be reclaimed — `Replaced` would misfire on a live turn (#4370)"
        );
        assert!(!real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    // Negative (narrow scoping): a real-user owner WITHOUT the `readopted_from_inflight`
    // marker is never reclaimable — even when committed and aged. Only turns this
    // process re-adopted from disk after a restart are eligible; an ordinary
    // freshly-started real-user turn stays untouched.
    #[tokio::test(flavor = "current_thread")]
    async fn real_owner_without_readopted_marker_is_never_reclaimed() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_070);
        let tmux = "AgentDesk-claude-4370-unmarked";
        let real_id = MessageId::new(4_370_170);
        let synth_id = MessageId::new(4_370_270);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        // committed + aged, but readopted_from_inflight stays false.
        let state = real_user_inflight_state(channel_id, real_id, tmux, true);
        assert!(!state.readopted_from_inflight);
        inflight::save_inflight_state(&state).expect("save unmarked real inflight");

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            old_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "an unmarked real-user owner must never be reclaimable (#4370 narrow scoping)"
        );
        assert!(!real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    // Integration (REQUIRED): re-adopt an in-flight real-user turn across a
    // simulated dcserver restart via the production
    // `reregister_active_turn_from_inflight`, then drive BOTH an injection turn
    // and a task-notification turn, asserting each acquires relay ownership
    // (zero loss). Assertions are on the mailbox-ownership transition
    // (real user → synthetic relay owner), not a string match. A 24% drop
    // (#4018 recurrence on the restart path) is caught here because the
    // synthetic claim would fail — the mailbox would stay owned by the stale
    // real-user turn — and `active_request_owner` would never transition.
    #[tokio::test(flavor = "current_thread")]
    async fn readopted_from_inflight_turn_yields_mailbox_to_injection_and_task_notification() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();

        // Two independent channels stand in for the two lost turn classes in the
        // incident: an injection (/loop heartbeat) turn and a task-notification
        // turn. Both are blocked by a restart-re-adopted real-user turn.
        for (label, channel_id, real_id, synth_id, tmux) in [
            (
                "injection",
                ChannelId::new(4_370_010),
                MessageId::new(4_370_110),
                MessageId::new(4_370_210),
                "AgentDesk-claude-4370-inject",
            ),
            (
                "task_notification",
                ChannelId::new(4_370_020),
                MessageId::new(4_370_120),
                MessageId::new(4_370_220),
                "AgentDesk-claude-4370-tasknote",
            ),
        ] {
            // Pre-restart: a live real-user turn, marker NOT yet set.
            let pre = real_user_inflight_state(channel_id, real_id, tmux, false);
            assert!(
                !pre.readopted_from_inflight,
                "{label}: marker must be absent before re-adopt"
            );
            inflight::save_inflight_state(&pre).expect("seed pre-restart inflight");

            // Simulated restart: fresh (empty) in-memory mailbox, re-adopt the
            // real turn via the production entrypoint.
            let readopted =
                crate::services::discord::recovery::reregister_active_turn_from_inflight(
                    &shared, &pre,
                )
                .await;
            assert!(readopted, "{label}: restart must re-adopt the live turn");

            // Transition #1: the REAL user owns the mailbox post-restart.
            let snap = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(
                snap.active_request_owner,
                Some(real_owner()),
                "{label}: re-adopted turn owns the mailbox"
            );
            assert_eq!(snap.active_user_message_id, Some(real_id));

            // reregister must persist the marker (not our builder), and must NOT
            // mute the re-adopted turn like a relay-ownership-only note.
            let reloaded = inflight::load_inflight_state(&provider, channel_id.get())
                .expect("re-adopted row survives on disk");
            assert!(
                reloaded.readopted_from_inflight,
                "{label}: reregister must stamp readopted_from_inflight (#4370)"
            );
            assert!(
                !reloaded.relay_ownership_only,
                "{label}: re-adopted real turn keeps its own completion lifecycle"
            );
            shared.restart.global_active.store(1, Ordering::Relaxed);

            // The re-adopted turn finishes (terminal delivery commits) — but the
            // mailbox is not released (the #4370 defect).
            let mut committed = reloaded;
            committed.terminal_delivery_committed = true;
            inflight::save_inflight_state(&committed).expect("commit re-adopted turn");

            // A starved synthetic relay turn arrives and reclaims the stale
            // mailbox.
            let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
                &shared,
                &provider,
                channel_id,
                tmux,
                real_id,
                snap.active_request_owner,
                snap.active_turn_kind,
                snap.turn_started_at,
                snap.active_turn_nonce.clone(),
                synth_id,
            )
            .await;
            assert!(
                reclaimed,
                "{label}: committed re-adopted owner must yield the mailbox (#4370)"
            );

            // Transition #2: relay ownership passes real user → synthetic. This
            // is the zero-loss proof — the synthetic turn now owns the relay.
            let synth_claimed = mailbox_try_start_turn_kinded(
                &shared,
                channel_id,
                Arc::new(CancelToken::new()),
                synthetic_owner(),
                synth_id,
                ActiveTurnKind::Background,
            )
            .await;
            assert!(
                synth_claimed,
                "{label}: synthetic relay turn must acquire the freed mailbox"
            );
            let after = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
            assert_eq!(
                after.active_request_owner,
                Some(synthetic_owner()),
                "{label}: relay ownership transitioned real→synthetic"
            );
            assert_eq!(after.active_user_message_id, Some(synth_id));
        }
    }

    // ===================================================================
    // #4370 F6 — Path B: the ROW-ABSENT stuck-mailbox shape (the #4370
    // reproduction). The on-disk row was cleared (e.g. the watcher's
    // identity-guarded clear succeeded) but the mailbox slot stayed stuck owned
    // by the re-adopted real user, so `load_inflight_state` returns `None`. There
    // is no row / on-disk marker to inspect, so eligibility is decided ENTIRELY by
    // the in-memory `readopted_mailbox_ledger`.
    // ===================================================================

    /// Path B positive (#4370 R3-7 — drives the PRODUCTION record path): re-adopt a
    /// real-user turn through `reregister_active_turn_from_inflight`, reproduce the
    /// watcher terminal-commit transition (stamp the ledger `finished`, then clear
    /// the durable row), and prove the resulting ABSENT-row + `finished` + aged
    /// mailbox IS reclaimed and its ledger entry evicted. Seeding the ledger via
    /// the real re-adopt (not a bare `record_readopted_mailbox_owner`) means a
    /// broken production record path would leave the ledger empty and fail this
    /// reclaim — the exact regression a direct-seed test would miss.
    #[tokio::test(flavor = "current_thread")]
    async fn path_b_absent_row_in_ledger_aged_reclaims_and_evicts() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_080);
        let tmux = "AgentDesk-claude-4370-pathb";
        let real_id = MessageId::new(4_370_180);
        let synth_id = MessageId::new(4_370_280);

        // Re-adopt the live real-user turn through the production entrypoint. This
        // records the ledger (finished == false) AND starts the mailbox turn.
        let pre = real_user_inflight_state(channel_id, real_id, tmux, false);
        inflight::save_inflight_state(&pre).expect("seed pre-restart inflight");
        let readopted =
            crate::services::discord::recovery::reregister_active_turn_from_inflight(&shared, &pre)
                .await;
        assert!(readopted, "restart must re-adopt the live turn");
        shared.restart.global_active.store(1, Ordering::Relaxed);
        // Capture the mailbox cancel token the re-adopt bound, to prove the reclaim
        // cancels it.
        let real_token = crate::services::discord::mailbox_snapshot(&shared, channel_id)
            .await
            .cancel_token
            .expect("re-adopted mailbox holds a cancel token");

        // The re-adopted turn's terminal delivery commits (watcher path). This
        // legacy/restart fixture uses the non-episode finish variant; automatic
        // exact-episode recovery uses the stable-pin `_for_episode` sibling in
        // the terminal-commit epilogue. Then the durable row is cleared → the
        // ABSENT-row shape.
        shared.mark_readopted_mailbox_owner_finished(
            &provider,
            channel_id.get(),
            real_owner().get(),
            real_id.get(),
        );
        assert!(
            inflight::clear_inflight_state(&provider, channel_id.get()),
            "the terminal-commit clear removes the durable row"
        );
        assert!(
            inflight::load_inflight_state(&provider, channel_id.get()).is_none(),
            "Path B precondition: the durable row is ABSENT after the commit clear"
        );

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            old_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            reclaimed,
            "an aged, ledger-FINISHED re-adopted owner with an ABSENT row must be reclaimable (#4370 Path B)"
        );
        assert!(real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            shared.restart.global_active.load(Ordering::Relaxed),
            0,
            "the finalize must decrement global_active"
        );
        assert!(
            !shared.is_readopted_mailbox_owner(
                &provider,
                channel_id.get(),
                real_owner().get(),
                real_id.get()
            ),
            "the ledger entry must be evicted after a successful reclaim"
        );
    }

    /// Path B negative (narrow scoping): row ABSENT and the owner is NOT in the
    /// ledger → never reclaimed. This is the conservative bail — without a ledger
    /// record we cannot prove this mailbox is a re-adopted turn, so we must not
    /// steal what might be a genuinely live turn.
    #[tokio::test(flavor = "current_thread")]
    async fn path_b_absent_row_not_in_ledger_is_never_reclaimed() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_081);
        let tmux = "AgentDesk-claude-4370-pathb-noledger";
        let real_id = MessageId::new(4_370_181);
        let synth_id = MessageId::new(4_370_281);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        // No ledger record, no on-disk row.
        assert!(inflight::load_inflight_state(&provider, channel_id.get()).is_none());

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            old_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "an ABSENT row for a real owner NOT in the ledger must never be reclaimed (#4370 Path B conservative bail)"
        );
        assert!(!real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    /// Path B negative (age gate): row ABSENT, owner IS in the ledger, but the
    /// owner is younger than the 120s positive-staleness gate → not reclaimed.
    /// The `OwnerInflightAbsent` reason keeps its `requires_positive_owner_age`
    /// gate for the re-adopted class, exactly as for the #4018 synthetic class.
    #[tokio::test(flavor = "current_thread")]
    async fn path_b_absent_row_in_ledger_young_is_not_reclaimed() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_082);
        let tmux = "AgentDesk-claude-4370-pathb-young";
        let real_id = MessageId::new(4_370_182);
        let synth_id = MessageId::new(4_370_282);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        shared.record_readopted_mailbox_owner(
            &provider,
            channel_id.get(),
            real_owner().get(),
            real_id.get(),
        );
        // Mark FINISHED so the ONLY thing blocking reclaim is the age gate — this
        // test isolates the `>= 120s` gate, not R3-1's `finished` liveness gate
        // (covered by `path_b_absent_row_ledger_live_not_finished_is_never_stolen`).
        shared.mark_readopted_mailbox_owner_finished(
            &provider,
            channel_id.get(),
            real_owner().get(),
            real_id.get(),
        );

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            young_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "a ledger-recorded re-adopted owner younger than 120s must not be reclaimed (#4370 Path B age gate)"
        );
        assert!(!real_token.cancelled.load(Ordering::Relaxed));
        // The ledger entry is untouched (no reclaim happened) — a later aged
        // attempt can still fire.
        assert!(shared.is_readopted_mailbox_owner(
            &provider,
            channel_id.get(),
            real_owner().get(),
            real_id.get()
        ));
    }

    /// Path B negative (LIVE-TURN THEFT GUARD): row ABSENT, a ledger entry exists
    /// for a PRIOR re-adopted turn, but a DIFFERENT `user_msg_id` now owns the
    /// mailbox (a live successor turn). The exact-id match fails, so the stale
    /// ledger entry can NEVER authorize stealing the live successor.
    #[tokio::test(flavor = "current_thread")]
    async fn path_b_absent_row_ledger_different_user_msg_id_is_never_stolen() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_083);
        let tmux = "AgentDesk-claude-4370-pathb-theft";
        let stale_ledger_id = MessageId::new(4_370_183);
        let live_successor_id = MessageId::new(4_370_193);
        let synth_id = MessageId::new(4_370_283);
        // The mailbox is owned by the LIVE successor turn (a different id).
        let live_token = seed_real_owner_mailbox(&shared, channel_id, live_successor_id).await;
        // The ledger still records the PRIOR re-adopted turn's id, marked FINISHED
        // (its terminal delivery had committed) — so the ONLY guard rejecting the
        // reclaim here is the exact-id mismatch against the live successor, not the
        // `finished` gate.
        shared.record_readopted_mailbox_owner(
            &provider,
            channel_id.get(),
            real_owner().get(),
            stale_ledger_id.get(),
        );
        shared.mark_readopted_mailbox_owner_finished(
            &provider,
            channel_id.get(),
            real_owner().get(),
            stale_ledger_id.get(),
        );

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            live_successor_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            old_owner_started_at(),
            live_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "a stale ledger entry (different user_msg_id) must never steal a live successor turn (#4370 Path B theft guard)"
        );
        assert!(!live_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }

    /// Path B negative (#4370 R3-1 LIVENESS GUARD): row ABSENT, the owner IS in the
    /// ledger with the EXACT `(owner, active_user_message_id)`, and aged past the
    /// 120s gate — but the entry is NOT `finished` (a genuinely LIVE re-adopted turn
    /// whose durable row merely happens to be absent, e.g. a not-yet-committed turn
    /// whose row was transiently cleared). Absence + exact-id + age ALONE must NOT
    /// reclaim it; only a `finished` entry (a committed terminal delivery) is
    /// reclaimable. Before R3-1 this shape had no liveness signal at all and could
    /// have been stolen — this is the test that pins the enforced invariant.
    #[tokio::test(flavor = "current_thread")]
    async fn path_b_absent_row_ledger_live_not_finished_is_never_stolen() {
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_084);
        let tmux = "AgentDesk-claude-4370-pathb-live";
        let real_id = MessageId::new(4_370_184);
        let synth_id = MessageId::new(4_370_284);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        // Ledger records the re-adopted owner but the entry is LIVE: a fresh
        // `record_readopted_mailbox_owner` seeds `finished == false`, and no
        // terminal commit has stamped it finished.
        shared.record_readopted_mailbox_owner(
            &provider,
            channel_id.get(),
            real_owner().get(),
            real_id.get(),
        );
        assert!(
            inflight::load_inflight_state(&provider, channel_id.get()).is_none(),
            "Path B precondition: the durable row is ABSENT"
        );

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            // AGED — isolates the `finished` gate from the age gate: age is
            // satisfied, so a reclaim would fire if `finished` were not required.
            old_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            !reclaimed,
            "an ABSENT-row re-adopted owner that has NOT committed terminal delivery (finished == false) must NEVER be reclaimed, even aged with an exact-id ledger match (#4370 R3-1 liveness guard)"
        );
        assert!(!real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);

        let synth_claimed = mailbox_try_start_turn_kinded(
            &shared,
            channel_id,
            Arc::new(CancelToken::new()),
            synthetic_owner(),
            synth_id,
            ActiveTurnKind::Background,
        )
        .await;
        assert!(
            !synth_claimed,
            "the live (not-finished) re-adopted turn must keep the mailbox slot"
        );
    }

    /// #4370 F3(b): a `Finalized` reclaim of a re-adopted REAL-user owner does NOT
    /// suppress that turn's completion footer / ✅ reaction / analytics.
    ///
    /// Evidence chain (see the #4370 review): a real turn's `⏳→✅` reaction,
    /// completion footer, and transcript/analytics are all emitted INLINE by the
    /// tmux watcher in the SAME pass that sets `terminal_delivery_committed = true`
    /// — BEFORE any finalizer handoff (the #4106 pre-panel early-release finalizes
    /// the turn *before* the footer edit, proving the finalizer never owns the
    /// footer/✅). The Finalized reclaim below only runs because
    /// `terminal_delivery_committed == true`, i.e. AFTER that UI already rendered.
    ///
    /// The reclaim submits `TerminalEvent::Cancel` through
    /// `FinalizeContext::watcher()`. The only reaction hook,
    /// `finalized_reaction_lifecycle`, strips `⏳` / withholds `✅` ONLY for a
    /// *backstop-cleanup* context (`clear_inflight && kickoff_queue`). `watcher()`
    /// is NOT that shape, so the reclaim schedules NO reaction change and cannot
    /// suppress the already-rendered footer/✅. This test pins both the invariant
    /// (context shape) and the behaviour (reclaim finalizes cleanly).
    #[tokio::test(flavor = "current_thread")]
    async fn finalized_reclaim_of_readopted_real_owner_does_not_suppress_completion_footer() {
        // #4370 R3-3: assert the REAL context the reclaim submits — read from the
        // SAME `reclaim_finalize_context()` source `finalize_stale_mailbox_owner_if_current`
        // passes, NOT a re-fabricated `FinalizeContext::watcher()`. If the production
        // call site is re-pointed at a backstop-cleanup context, THIS breaks (the
        // old test asserted a standalone `watcher()` and would have stayed green).
        let ctx = stale_reclaim::reclaim_finalize_context();
        // The exact `backstop_cleanup` predicate from `finalized_reaction_lifecycle`
        // (turn_finalizer/cleanup.rs:54-57) — the only shape that schedules a
        // reaction change on a Cancel.
        let backstop_cleanup = ctx.clear_inflight
            && ctx.kickoff_queue
            && !ctx.allow_completion_cleanup
            && !ctx.drain_voice;
        assert!(
            !backstop_cleanup,
            "the reclaim finalize context must not be a backstop-cleanup shape — that is the only shape that strips ⏳ / withholds ✅ on a Cancel (#4370 F3b)"
        );

        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_370_090);
        let tmux = "AgentDesk-claude-4370-footer";
        // REAL Discord snowflake ids (>= 1e14): required so the reclaim's Cancel
        // actually reaches the reaction gate in `finalized_reaction_lifecycle`
        // (it early-returns on non-real ids), making the "no reaction scheduled"
        // assertion below a genuine test of the context gate, not of the id filter.
        let real_id = MessageId::new(1_437_009_000_000_190);
        let synth_id = MessageId::new(1_437_009_000_000_290);
        let real_token = seed_real_owner_mailbox(&shared, channel_id, real_id).await;
        // Committed (footer/✅ already rendered pre-restart) + marked re-adopted.
        let mut state = real_user_inflight_state(channel_id, real_id, tmux, true);
        state.readopted_from_inflight = true;
        inflight::save_inflight_state(&state).expect("save committed re-adopted inflight");

        // Observe the ACTUAL finalizer-scheduled reactions. In test builds
        // `schedule_reaction_cleanup` records synchronously into a global the
        // recorder captures; this test's canonical root guard also holds the
        // crate-wide shared test-environment lock, so the recorder cannot race.
        // An empty record set proves the reclaim scheduled NO reaction change
        // and therefore cannot suppress the already-pending completion footer / ✅.
        crate::services::discord::turn_finalizer::cleanup::begin_reaction_cleanup_recording();

        let reclaimed = release_reclaimable_stale_synthetic_mailbox_owner_if_current(
            &shared,
            &provider,
            channel_id,
            tmux,
            real_id,
            Some(real_owner()),
            ActiveTurnKind::UserOrAgent,
            young_owner_started_at(),
            real_token.turn_nonce().map(str::to_owned),
            synth_id,
        )
        .await;
        assert!(
            reclaimed,
            "a committed re-adopted owner must yield the stuck mailbox (Finalized, no age gate) (#4370 F3b)"
        );
        // The reclaim scheduled NO reaction change — the behavioural proof that it
        // cannot suppress the re-adopted turn's already-rendered/pending chrome.
        let reaction_records =
            crate::services::discord::turn_finalizer::cleanup::take_reaction_cleanup_records();
        assert!(
            reaction_records.is_empty(),
            "the stale-owner reclaim must schedule NO reaction change (would suppress the completion footer / ✅); got {reaction_records:?} (#4370 F3b)"
        );
        // The finalize released the mailbox cleanly (token cancelled, global_active
        // decremented) WITHOUT re-touching the already-rendered completion UI.
        assert!(real_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    }
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
    captured_source: Option<(String, u64)>,
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
        captured_source,
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
            if record.captured_source.is_some()
                && crate::services::tui_prompt_dedupe::external_input_relay_lease(
                    provider.as_str(),
                    &record.tmux_session_name,
                    record.channel_id,
                )
                .is_some_and(|lease| lease.turn_id != record.lease_turn_id)
            {
                return false;
            }

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

            let claim = claim::claim_tui_direct_synthetic_turn_inner::<true>(
                shared,
                &provider,
                channel_id,
                &record.tmux_session_name,
                &record.prompt_text,
                anchor_message_id,
                &lease,
                record.captured_source.as_ref(),
            )
            .await
            .0;

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
                    source_event_id: None,
                    observed_at: chrono::Utc::now(),
                    external_input_lease_generation:
                        crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
                    ssh_direct_observation_generation:
                        crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
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
        .tmux_session_live_for_relay(tmux_session_name)
        .is_some_and(|live| live);
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
///   live producer registered (`relay_producer_registry::get_live_producer` → `None`,
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

fn tui_direct_watcher_synthetic_inflight_shape_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && state.effective_relay_owner_kind() == RelayOwnerKind::Watcher
    })
}

/// A passive synthetic claim has resolved away from the BridgeAdapter once
/// either the tmux watcher or the session-bound relay owns terminal delivery.
/// The Codex/Claude idle observers record their provisional BridgeAdapter lease
/// before the synthetic-start observer resolves this owner; they must wait for
/// BOTH non-bridge outcomes or they can spawn a second Discord surface beside a
/// live session-bound relay (#4455).
pub(super) fn tui_direct_synthetic_non_bridge_owner_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    session_bound_relay_has_live_producer: bool,
) -> bool {
    state.is_some_and(|state| {
        state.turn_source == TurnSource::ExternalInput
            && state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && match state.effective_relay_owner_kind() {
                RelayOwnerKind::Watcher => true,
                RelayOwnerKind::SessionBoundRelay => session_bound_relay_has_live_producer,
                _ => false,
            }
    })
}

pub(in crate::services::discord) fn tui_direct_watcher_synthetic_inflight_matches(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> bool {
    state.is_some_and(|state| {
        tui_direct_watcher_synthetic_inflight_shape_matches(Some(state), tmux_session_name)
            && state.turn_start_offset.unwrap_or(state.last_offset) < current_offset
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
pub(super) async fn wait_for_tui_direct_synthetic_non_bridge_claim(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> bool {
    let deadline = tokio::time::Instant::now() + TUI_DIRECT_SYNTHETIC_CLAIM_WAIT;
    loop {
        let session_bound_relay_has_live_producer =
            crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
                .get_live_producer(tmux_session_name)
                .is_some();
        if tui_direct_synthetic_non_bridge_owner_matches(
            super::super::inflight::load_inflight_state(provider, channel_id.get()).as_ref(),
            tmux_session_name,
            session_bound_relay_has_live_producer,
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
    expected_session_key: Option<&str>,
    reason: &'static str,
) {
    let Some(state) = super::super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if !tui_direct_synthetic_inflight_matches(Some(&state), tmux_session_name) {
        return;
    }
    if let Some(expected_session_key) = expected_session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && state.session_key.as_deref() != Some(expected_session_key)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            expected_session_key,
            actual_session_key = state.session_key.as_deref().unwrap_or(""),
            reason,
            "skipping TUI-direct synthetic finalizer cleanup; inflight belongs to a newer session key"
        );
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
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    if finalizer_turn_id == 0 {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            reason,
            "skipping TUI-direct synthetic finalizer cleanup; finalizer turn id is zero"
        );
        return;
    }

    let identity = super::super::inflight::InflightTurnIdentity::from_state(&state);
    match super::super::inflight::clear_inflight_state_if_matches_identity(
        provider,
        channel_id.get(),
        &identity,
    ) {
        super::super::inflight::GuardedClearOutcome::Cleared => {}
        outcome => {
            tracing::info!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                reason,
                ?outcome,
                "skipping TUI-direct synthetic finalizer cleanup; inflight identity changed before clear"
            );
            return;
        }
    }

    let _ = shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            super::super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::super::turn_finalizer::TerminalEvent::Cancel,
            super::super::turn_finalizer::FinalizeContext::monitor(),
            Some(super::super::turn_finalizer::SyntheticClaimSnapshot::from_row(&state)),
            shared.clone(),
        )
        .await;
}

// #3982 orphan-at-birth reclaim helpers + their unit tests live in the sibling
// `synthetic_orphan_reclaim` module (keeps this file focused and under the
// giant-file threshold); `defer_synthetic_turn_start` / `restore_pending_starts`
// inject `synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn()`.
