//! Canonical replay holds admit captured result delivery, while retaining the request episode.

use super::*;

pub(super) async fn hydrate_replay_hold(
    pool: Option<&sqlx::PgPool>,
    state: &mut inflight::InflightTurnState,
) {
    use crate::db::replay_disposition as replay;
    let reason = if let Some(receipt_id) = state.replay_receipt_id {
        match pool {
            None => Some(format!("receipt {receipt_id} unreadable without postgres")),
            Some(pool) => match replay::receipt_disposition(pool, receipt_id).await {
                Ok(Some(disposition))
                    if !replay::stored_disposition_blocks_rerun(Some(&disposition)) =>
                {
                    None
                }
                Ok(Some(disposition)) => Some(format!("receipt {receipt_id} {disposition}")),
                Ok(None) => Some(format!("receipt {receipt_id} disposition missing")),
                Err(error) => Some(format!("receipt {receipt_id} unreadable: {error}")),
            },
        }
    } else if let Some(pool) = pool {
        let sources: Vec<String> = state
            .source_message_ids
            .iter()
            .copied()
            .chain(std::iter::once(state.user_msg_id))
            .filter(|id| *id != 0)
            .map(|id| id.to_string())
            .collect();
        match replay::blocked_receipt_for_sources(
            pool,
            &state.provider,
            &state.channel_id.to_string(),
            &sources,
        )
        .await
        {
            Ok(Some((receipt_id, disposition))) => {
                state.replay_receipt_id = Some(receipt_id);
                Some(format!("receipt {receipt_id} {disposition}"))
            }
            Ok(None) => None,
            Err(error) => Some(format!("replay source disposition unreadable: {error}")),
        }
    } else {
        None
    };
    if let Some(reason) = reason
        && !state.replay_hold_reasons.contains(&reason)
    {
        state.replay_hold_reasons.push(reason);
    }
}

pub(super) async fn deliver_held_debt(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut inflight::InflightTurnState,
) {
    let Some(channel_id) = inflight::opt_channel_id(state.channel_id) else {
        return;
    };
    // Re-serializing an unknown runtime would erase its original spelling; keep its raw row.
    if state.runtime_kind_unknown_on_disk
        || (state.runtime_kind.is_none() && state.version > inflight::inflight_state_version())
        || state
            .full_response
            .get(state.response_sent_offset..)
            .is_none()
    {
        return;
    }
    if !matches!(
        inflight::save_inflight_state_if_identity_unchanged(&mut *state, "recovery_replay_hold"),
        inflight::GuardedSaveOutcome::Saved
    ) || state.terminal_delivery_completed()
    {
        return;
    }
    let owner = super::super::mailbox_snapshot(shared, channel_id).await;
    if owner.cancel_token.is_some()
        && (owner.active_user_message_id != inflight::opt_message_id(state.user_msg_id)
            || crate::services::provider::cancel_requested(owner.cancel_token.as_deref()))
    {
        return;
    }
    // A rollover cursor excludes frozen messages. Without frozen prefixes, replace the complete
    // current body so a confirmed prefix at the same anchor is never overwritten by only its tail.
    let response = if state.streaming_rollover_frozen_msg_ids.is_empty() {
        state.full_response.as_str()
    } else if state.response_sent_offset > 0 {
        &state.full_response[state.response_sent_offset..]
    } else {
        return;
    };
    if response.trim().is_empty() {
        return;
    }
    let text = super::super::formatting::format_for_discord_with_provider(response, provider);
    let delivery =
        relay_captured_recovery_terminal_notice(http, shared, provider, state, &text).await;
    let _ = shared
        .mailbox(channel_id)
        .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
            shared: shared.clone(),
            state: state.clone(),
            actor: owner.cancel_token,
            delivery,
        })
        .await;
}
