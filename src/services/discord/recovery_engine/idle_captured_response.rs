use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;

#[derive(PartialEq, Eq)]
struct SourceAtEof {
    file: SourceFileIdentity,
    modified: std::time::SystemTime,
    end: u64,
}

impl SourceAtEof {
    fn capture(row: &inflight::InflightTurnState, output: &Path) -> Option<Self> {
        if row.output_path.as_deref().map(Path::new) != Some(output) {
            return None;
        }
        let file = std::fs::File::open(output).ok()?;
        let metadata = file.metadata().ok()?;
        if metadata.len() != row.last_offset {
            return None;
        }
        Some(Self {
            file: SourceFileIdentity::from_open_file(&file),
            modified: metadata.modified().ok()?,
            end: metadata.len(),
        })
    }
}

pub(in crate::services::discord) async fn recover_idle_partial_response(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    row: &inflight::InflightTurnState,
    output: &Path,
) -> bool {
    let Some(provider) = row.provider_kind() else {
        return false;
    };
    let retry_delay =
        super::super::health::relay_recovery_retry_delay_secs(row.recovery_relay_attempts);
    if retry_delay > 0
        && inflight::parse_updated_at_unix(&row.updated_at).is_none_or(|updated| {
            chrono::Utc::now()
                .timestamp()
                .saturating_sub(updated)
                .max(0)
                < retry_delay
        })
    {
        return false;
    }
    let Some(tmux) = row.tmux_session_name.as_deref() else {
        return false;
    };
    if !recovery_ready_without_output_has_captured_response(row)
        || row.restart_mode.is_some()
        || row.rebind_origin
        || row.terminal_delivery_completed()
        || row.current_msg_id == 0
        || shared.relay_emission_in_flight(ChannelId::new(row.channel_id))
        || !crate::services::provider::tmux_session_fallback_ready_for_input(
            tmux,
            &provider,
            row.runtime_kind,
        )
        .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
    {
        return false;
    }
    let gateway = DiscordGateway::new(http.clone(), shared.clone(), provider, None);
    recover_idle_partial_response_from_ready_source(http, shared, row, output, &gateway).await
}

/// The caller has already observed a ready pane. Keep the source, dormant
/// claim and delivery settlement identical for production and gateway fixtures.
pub(in crate::services::discord) async fn recover_idle_partial_response_from_ready_source(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    row: &inflight::InflightTurnState,
    output: &Path,
    gateway: &dyn super::super::gateway::TurnGateway,
) -> bool {
    let Some(provider) = row.provider_kind() else {
        return false;
    };
    let Some(source) = SourceAtEof::capture(row, output) else {
        return false;
    };
    let Some(start) = row.turn_start_offset.filter(|start| *start < source.end) else {
        return false;
    };
    if extract_response_from_output(&output.to_string_lossy(), start) != row.full_response {
        return false;
    }
    let Some(claim) =
        super::super::tui_prompt_relay::capture_dormant_partial(shared, row, output).await
    else {
        return false;
    };
    if SourceAtEof::capture(&claim.row, output).as_ref() != Some(&source)
        || shared.relay_emission_in_flight(ChannelId::new(row.channel_id))
    {
        return false;
    }
    let state = &claim.row;
    // Typed terminals use the existing pinned range lease inside their
    // publisher. Taking a second markerless lease here would block that lease.
    let _lease = if state.requires_pinned_terminal_recovery() {
        None
    } else {
        let Some(context) = RecoveryDeliveryContext::from_state(
            shared,
            &provider,
            state,
            None,
            shared.restart.current_generation,
        ) else {
            return false;
        };
        let Some(response) = state
            .full_response
            .get(state.response_sent_offset..)
            .filter(|body| !body.trim().is_empty())
        else {
            return false;
        };
        let Some(lease) = context.try_acquire_fresh_send_lease(shared, response) else {
            return false;
        };
        Some(lease)
    };
    settle_ready_without_output_for_actor(shared, &provider, state, Some(&claim.actor), |text| {
        let provider = &provider;
        let source = &source;
        async move {
            let mut outcome = relay_captured_recovery_terminal_notice_with_gateway(
                http, shared, provider, state, &text, gateway,
            )
            .await;
            let confirmed_typed = state.requires_pinned_terminal_recovery()
                && matches!(outcome.outcome, RecoveryRelayOutcome::Delivered);
            if !confirmed_typed && SourceAtEof::capture(state, output).as_ref() != Some(source) {
                outcome.outcome = RecoveryRelayOutcome::TransientFailure;
            }
            outcome
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_partial_source_stamp_refuses_append_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("source.jsonl");
        std::fs::write(&output, b"old").unwrap();
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            5071808,
            None,
            1,
            2,
            3,
            String::new(),
            None,
            None,
            Some(output.to_string_lossy().into_owned()),
            None,
            0,
        );
        row.last_offset = 3;
        let initial = SourceAtEof::capture(&row, &output).unwrap();
        assert!(SourceAtEof::capture(&row, &output).as_ref() == Some(&initial));
        std::fs::write(&output, b"longer").unwrap();
        assert!(SourceAtEof::capture(&row, &output).is_none());
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"new").unwrap();
        std::fs::rename(replacement, &output).unwrap();
        assert!(SourceAtEof::capture(&row, &output).as_ref() != Some(&initial));
    }
}

pub(super) async fn finish_recovered_turn_mailbox_for_captured_state(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    snapshot: super::turn_finalizer::SyntheticClaimSnapshot,
) -> Option<super::turn_finalizer::FinalizeOutcome> {
    // A row recovered without a mailbox actor cannot authorize releasing one
    // admitted later while its Discord request was in flight.
    if snapshot.recovery_actor.is_none() {
        return None;
    }
    if let Some(channel_id) = inflight::opt_channel_id(state.channel_id) {
        Some(
            finish_recovered_turn_mailbox_with_snapshot(
                shared,
                provider,
                channel_id,
                state.effective_finalizer_turn_id(),
                Some(snapshot),
            )
            .await,
        )
    } else {
        None
    }
}

pub(super) async fn finish_recovered_turn_mailbox_with_snapshot(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_msg_id: u64,
    snapshot: Option<super::turn_finalizer::SyntheticClaimSnapshot>,
) -> super::turn_finalizer::FinalizeOutcome {
    shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            super::turn_finalizer::TurnKey::new(
                channel_id,
                user_msg_id,
                shared.restart.current_generation,
            ),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            super::turn_finalizer::FinalizeContext::monitor(),
            snapshot,
            shared.clone(),
        )
        .await
}

pub(super) async fn settle_ready_without_output<F, Fut>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    relay: F,
) -> bool
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future,
    Fut::Output: Into<CapturedRecoveryDelivery>,
{
    settle_ready_without_output_for_actor(shared, provider, state, None, relay).await
}

pub(super) async fn settle_ready_without_output_for_actor<F, Fut>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
    relay: F,
) -> bool
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future,
    Fut::Output: Into<CapturedRecoveryDelivery>,
{
    if state.restart_mode.is_some() || state.rebind_origin {
        return false;
    }
    // A pre-existing actor must be the caller's captured incarnation. IDs and
    // even equal nonces cannot identify two actors representing one execution.
    let owner = super::mailbox_snapshot(shared, ChannelId::new(state.channel_id)).await;
    let admissible_actor = match (actor, owner.cancel_token.as_ref()) {
        (Some(expected), Some(current)) => {
            Arc::ptr_eq(expected, current)
                && !crate::services::provider::cancel_requested(Some(expected))
        }
        (None, None) => true,
        _ => false,
    };
    let current = inflight::load_inflight_state(provider, state.channel_id);
    if !admissible_actor
        || current.as_ref().is_none_or(|current| {
            !inflight::InflightEpisodePin::from_state(state).matches_state(current)
                || current.save_generation != state.save_generation
        })
    {
        return false;
    }
    let mut snapshot = super::turn_finalizer::SyntheticClaimSnapshot::from_row(state);
    snapshot.recovery_actor = actor.map(Arc::downgrade);
    if recovery_ready_without_output_already_delivered(state) {
        retire_captured_ready_response(shared, provider, state, snapshot, |snapshot| {
            finish_recovered_turn_mailbox_for_captured_state(shared, provider, state, snapshot)
        })
        .await;
        return true;
    }
    if terminal_text_idempotency::captured_terminal_receipt_exists(provider, state) {
        settle_captured_ready_delivery(
            shared,
            provider,
            state,
            actor,
            snapshot,
            RecoveryRelayOutcome::Delivered.into(),
        )
        .await;
        return true;
    }
    if recovery_ready_without_output_has_captured_response(state) {
        // response_sent_offset covers frozen Discord prefixes, unlike last_offset
        // and last_watcher_relayed_offset, which use source JSONL coordinates.
        let response = &state.full_response[state.response_sent_offset..];
        let final_text = super::formatting::format_for_discord_with_provider(response, provider);
        let delivery = relay(final_text).await.into();
        // A captured partial answer remains a delivery obligation even after a
        // permanent channel error. Reuse the same nonce/save-generation CAS as
        // the idle drain; legacy force-clear/budget identity is too broad here.
        settle_captured_ready_delivery(shared, provider, state, actor, snapshot, delivery).await;
        return true;
    }
    tracing::warn!(
        channel_id = state.channel_id,
        "recovery: ready source at EOF has no terminal receipt or recoverable response suffix; preserving for watcher reattach"
    );
    false
}

async fn settle_captured_ready_delivery(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
    snapshot: super::turn_finalizer::SyntheticClaimSnapshot,
    delivery: CapturedRecoveryDelivery,
) {
    let Some(committed) = shared
        .mailbox(ChannelId::new(state.channel_id))
        .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
            shared: shared.clone(),
            state: state.clone(),
            actor: actor.cloned(),
            delivery,
        })
        .await
    else {
        return;
    };
    let delivered = committed.state;
    let mut committed_snapshot =
        super::turn_finalizer::SyntheticClaimSnapshot::from_row(&delivered);
    committed_snapshot.recovery_actor = snapshot.recovery_actor;
    retire_captured_ready_response(
        shared,
        provider,
        &delivered,
        committed_snapshot,
        |snapshot| {
            finish_recovered_turn_mailbox_for_captured_state(shared, provider, &delivered, snapshot)
        },
    )
    .await;
}

pub(super) async fn retire_captured_ready_response<F, Fut>(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    committed: &inflight::InflightTurnState,
    snapshot: super::turn_finalizer::SyntheticClaimSnapshot,
    finalize: F,
) where
    F: FnOnce(super::turn_finalizer::SyntheticClaimSnapshot) -> Fut,
    Fut: std::future::Future<Output = Option<super::turn_finalizer::FinalizeOutcome>>,
{
    use super::turn_finalizer::FinalizeOutcome;
    let expected_actor = snapshot
        .recovery_actor
        .as_ref()
        .and_then(std::sync::Weak::upgrade);
    let outcome = finalize(snapshot).await;
    match outcome {
        Some(FinalizeOutcome::Deferred) => return,
        Some(FinalizeOutcome::Finalized {
            removed_token: Some(removed),
            ..
        }) => {
            if !expected_actor
                .as_ref()
                .is_some_and(|expected| Arc::ptr_eq(expected, &removed))
            {
                return;
            }
        }
        _ => {
            // AlreadyFinalized can run a guarded mailbox cleanup. Its ledger
            // answer alone does not prove that the original actor was released.
            if super::mailbox_snapshot(shared, ChannelId::new(committed.channel_id))
                .await
                .cancel_token
                .is_some()
            {
                return;
            }
        }
    }
    // Finalization awaited other actors. Compare the committed anchor, nonce,
    // generation and identity again under the canonical row lock before removal.
    inflight::clear_inflight_state_for_snapshot(provider, committed);
}
