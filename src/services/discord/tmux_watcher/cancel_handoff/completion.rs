//! Same-process original-actor completion. Custody is not a delivery receipt.
use super::*;
use crate::services::discord::{
    inflight, outbound::delivery_record as records, turn_finalizer as finalizer,
};
use crate::services::provider::CancelToken;
use std::sync::Weak;

pub(in crate::services::discord) async fn capture_actors(
    shared: &SharedData,
    channel: ChannelId,
    original: Option<&InflightTurnState>,
) -> (Option<Weak<CancelToken>>, Option<Weak<CancelToken>>) {
    let Some(original) = original else {
        return (None, None);
    };
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return (None, None);
    };
    let snapshot = mailbox.snapshot().await;
    let Some(actor) = snapshot.cancel_token else {
        return (None, None);
    };
    let completion_actor = (original
        .turn_nonce
        .as_deref()
        .is_some_and(|nonce| !nonce.is_empty())
        && actor.turn_nonce() == original.turn_nonce.as_deref()
        && snapshot.active_user_message_id.map(|id| id.get())
            == Some(original.effective_finalizer_turn_id())
        && snapshot.active_request_owner.map(|id| id.get())
            == Some(original.request_owner_user_id))
    .then(|| Arc::downgrade(&actor));
    let owned_id = snapshot.active_user_message_id.map(|id| id.get())
        == Some(original.effective_finalizer_turn_id())
        || (original.user_msg_id == 0 && snapshot.active_user_message_id.is_none());
    let summary_nonce = original
        .turn_nonce
        .as_deref()
        .filter(|nonce| !nonce.is_empty());
    let summary_actor = (actor.turn_nonce().filter(|nonce| !nonce.is_empty()) == summary_nonce
        && snapshot
            .active_turn_nonce
            .as_deref()
            .filter(|nonce| !nonce.is_empty())
            == summary_nonce
        && owned_id
        && snapshot.active_request_owner.map(|id| id.get())
            == Some(original.request_owner_user_id))
    .then(|| Arc::downgrade(&actor));
    (completion_actor, summary_actor)
}

pub(in crate::services::discord::tmux::tmux_watcher) async fn finish_after_receipt(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    provider: &ProviderKind,
    original: Option<&InflightTurnState>,
    actor: Option<&Weak<CancelToken>>,
    authority: WatcherSourceAuthority,
    range: (u64, u64),
) {
    let (Some(original), Some(actor)) = (original, actor) else {
        return;
    };
    let Some(session) = original.tmux_session_name.as_deref() else {
        return;
    };
    let Some(path) = original.output_path.as_deref() else {
        return;
    };
    // The ordinary live-row path retains its existing epilogue. Corruption is
    // not absence, and a successor projection must never be borrowed or cleared.
    if !matches!(
        inflight::load_inflight_state_read_only_result(provider, channel.get()),
        Ok(None)
    ) || original.provider != provider.as_str()
        || original.channel_id != channel.get()
        || original.effective_finalizer_turn_id() == 0
        || original.turn_start_offset != Some(range.0)
        || authority.source_file
            == crate::services::cluster::stream_relay::SourceFileIdentity::Unavailable
        || authority.generation_mtime_ns == 0
        || authority.generation_mtime_ns != read_generation_file_mtime_ns(session)
        || authority.reset_incarnation != shared.relay_frontier_token(channel).reset_incarnation
        || !std::fs::File::open(path).ok().is_some_and(|file| {
            crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file)
                == authority.source_file
        })
        || recent_turn_stop_for_watcher_range(channel, session, range.0).is_some()
    {
        return;
    }
    let source = records::ExactJsonlSourceIdentity {
        provider: provider.as_str().into(),
        tmux_session_name: session.into(),
        turn_nonce: original.turn_nonce.clone().unwrap_or_default(),
        range,
        generation_mtime_ns: authority.generation_mtime_ns,
        offset_authority_channel_id: channel.get(),
        delivery_channel_id: channel.get(),
    };
    // An ACK high-watermark, a newer receipt, or merely retained bytes is not
    // proof. Reuse the existing exact source/destination receipt predicate.
    if !source.is_authoritative()
        || !records::read_record(provider, channel.get()).is_some_and(|record| {
            record.confirmed_deliveries.iter().any(|receipt| {
                receipt.source == source
                    && records::confirmed_delivery_receipt_exists(
                        provider,
                        channel,
                        receipt.message_id,
                        &source,
                    )
            })
        })
    {
        return;
    }
    let mut claim = finalizer::SyntheticClaimSnapshot::from_row(original);
    claim.recovery_actor = Some(actor.clone());
    shared
        .turn_finalizer
        .submit_terminal_with_claim_snapshot(
            finalizer::TurnKey::new(
                channel,
                original.effective_finalizer_turn_id(),
                shared.restart.current_generation,
            )
            .with_episode_nonce(original.turn_nonce.as_deref()),
            provider.clone(),
            finalizer::TerminalEvent::Complete,
            finalizer::FinalizeContext::watcher(),
            Some(claim),
            shared.clone(),
        )
        .await;
}

#[cfg(test)]
#[path = "completion_actor_capture_tests.rs"]
mod actor_capture_tests;
