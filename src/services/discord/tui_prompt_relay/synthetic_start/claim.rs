use super::*;

#[derive(Clone, Copy)]
pub(super) struct SyntheticClaimIdentity<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) tmux_session_name: &'a str,
    pub(super) prompt_text: &'a str,
    pub(super) anchor_message_id: MessageId,
    pub(super) lease: &'a ExternalInputRelayLease,
    pub(super) register_deferred_start: bool,
}

pub(super) struct SyntheticClaimPreparation<'a> {
    pub(super) identity: SyntheticClaimIdentity<'a>,
    pub(super) output_path: Option<PathBuf>,
    pub(super) start_offset: u64,
    pub(super) relay_owner: ExternalInputRelayOwner,
    pub(super) relay_owner_kind: RelayOwnerKind,
}

pub(in crate::services::discord::tui_prompt_relay) async fn claim_tui_direct_synthetic_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    prompt_text: &str,
    anchor_message_id: MessageId,
    lease: &ExternalInputRelayLease,
) -> TuiDirectSyntheticTurnClaim {
    claim_tui_direct_synthetic_turn_inner::<false>(
        shared,
        provider,
        channel_id,
        tmux_session_name,
        prompt_text,
        anchor_message_id,
        lease,
    )
    .await
}

pub(super) async fn claim_tui_direct_synthetic_turn_inner<const DEFERRED: bool>(
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
    let committed_relay_offset =
        super::super::super::tmux::committed_frontier_for_current_generation(
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
    // First birth requires both a non-shutdown queue consumer and an actual
    // uncancelled same-session watcher handle. Registration alone can leave the
    // passive consumer unfed while the active-row gate defers the idle feeder.
    let session_bound_feed_admissible =
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .get_live_producer(tmux_session_name)
            .is_some()
            && shared
                .tmux_watchers
                .has_uncancelled_watcher_handle(tmux_session_name);
    let relay_owner = tui_direct_synthetic_relay_owner(
        tui_direct_watcher_can_own_output(
            &shared.tmux_watchers,
            tmux_session_name,
            output_path.as_deref(),
        ),
        session_bound_discord_delivery_enabled(),
        session_bound_feed_admissible,
    );
    let relay_owner_kind = match relay_owner {
        ExternalInputRelayOwner::TmuxWatcher => RelayOwnerKind::Watcher,
        ExternalInputRelayOwner::SessionBoundRelay => RelayOwnerKind::SessionBoundRelay,
        _ => RelayOwnerKind::None,
    };

    claim_tui_direct_synthetic_turn_prepared(SyntheticClaimPreparation {
        identity: SyntheticClaimIdentity {
            shared,
            provider,
            channel_id,
            tmux_session_name,
            prompt_text,
            anchor_message_id,
            lease,
            register_deferred_start: DEFERRED,
        },
        output_path,
        start_offset,
        relay_owner,
        relay_owner_kind,
    })
    .await
}

impl SyntheticClaimIdentity<'_> {
    pub(super) fn register_episode(&self, active_turn_nonce: Option<&str>) {
        if self.register_deferred_start {
            // #3154: bind the admitted episode before either durable row write can
            // release the watcher gate. Adoption uses the existing actor nonce.
            self.shared.turn_finalizer.register_start(
                super::super::super::turn_finalizer::TurnKey::new(
                    self.channel_id,
                    self.anchor_message_id.get(),
                    self.shared.restart.current_generation,
                )
                .with_episode_nonce(active_turn_nonce),
                self.provider.clone(),
                RelayOwnerKind::Watcher,
                self.shared,
            );
        }
    }
}
