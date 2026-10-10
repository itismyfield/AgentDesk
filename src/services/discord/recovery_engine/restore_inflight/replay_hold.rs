//! Canonical replay holds admit captured result delivery, while retaining the request episode.

use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReplayLookup {
    Verified,
    Unverified { projected: bool, reason: String },
}

pub(super) async fn hydrate_replay_hold(
    pool: Option<&sqlx::PgPool>,
    state: &mut inflight::InflightTurnState,
) -> bool {
    match hydrate_replay_hold_checked(pool, state).await {
        ReplayLookup::Verified => true,
        ReplayLookup::Unverified { projected, reason } => {
            tracing::debug!(channel_id = state.channel_id, %reason, "replay lookup unverified");
            !projected
        }
    }
}

pub(super) async fn hydrate_replay_hold_checked(
    pool: Option<&sqlx::PgPool>,
    state: &mut inflight::InflightTurnState,
) -> ReplayLookup {
    use crate::db::replay_disposition as replay;
    let Some(pool) = pool else {
        return ReplayLookup::Unverified {
            projected: state.replay_receipt_id.is_some(),
            reason: "replay authority unavailable".into(),
        };
    };
    let receipt = match state.replay_receipt_id {
        Some(receipt_id) => match replay::receipt_disposition(pool, receipt_id).await {
            Ok(Some(disposition)) => Some((receipt_id, disposition)),
            _ => {
                return ReplayLookup::Unverified {
                    projected: true,
                    reason: format!("receipt {receipt_id} unverified"),
                };
            }
        },
        None => {
            let sources: Vec<String> = state
                .source_message_ids
                .iter()
                .chain([&state.user_msg_id])
                .filter(|&&id| id != 0)
                .map(|id| id.to_string())
                .collect();
            let provider = &state.provider;
            let channel = state.channel_id.to_string();
            match replay::blocked_receipt_for_sources(pool, provider, &channel, &sources).await {
                Ok(receipt) => receipt,
                Err(error) => {
                    tracing::warn!(channel_id = state.channel_id, %error, "replay source lookup failed");
                    #[cfg(all(test, unix))]
                    super::replay_hold_tests::apply_read_error_mutant(state, &error);
                    return ReplayLookup::Unverified {
                        projected: false,
                        reason: format!("replay source lookup failed: {error}"),
                    };
                }
            }
        }
    };
    if let Some((receipt_id, disposition)) = receipt
        .filter(|(_, disposition)| replay::stored_disposition_blocks_rerun(Some(disposition)))
    {
        state.replay_receipt_id = Some(receipt_id);
        let reason = format!("receipt {receipt_id} {disposition}");
        if !state.replay_hold_reasons.contains(&reason) {
            state.replay_hold_reasons.push(reason);
        }
    }
    ReplayLookup::Verified
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReplayDebtDelivery {
    Acknowledged,
    Retained(&'static str),
}

pub(super) async fn deliver_held_debt(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut inflight::InflightTurnState,
) {
    if let ReplayDebtDelivery::Retained(reason) =
        deliver_held_debt_checked(http, shared, provider, state).await
    {
        tracing::debug!(
            channel_id = state.channel_id,
            reason,
            "replay debt retained"
        );
    }
}

pub(super) async fn deliver_held_debt_checked(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &mut inflight::InflightTurnState,
) -> ReplayDebtDelivery {
    let Some(channel_id) = inflight::opt_channel_id(state.channel_id) else {
        return ReplayDebtDelivery::Retained("invalid channel");
    };
    // Re-serializing an unknown runtime or newer format would erase data; keep its raw row.
    if state.runtime_kind_unknown_on_disk
        || state.version > inflight::inflight_state_version()
        || !state
            .full_response
            .is_char_boundary(state.response_sent_offset)
    {
        return ReplayDebtDelivery::Retained("unsupported runtime, format or cursor");
    }
    if !matches!(
        inflight::save_inflight_state_if_identity_unchanged(&mut *state, "recovery_replay_hold"),
        inflight::GuardedSaveOutcome::Saved
    ) || state.terminal_delivery_completed()
    {
        return ReplayDebtDelivery::Retained("save unconfirmed or delivery already completed");
    }
    let owner = super::super::mailbox_snapshot(shared, channel_id).await;
    if owner.cancel_token.is_some()
        && (owner.active_user_message_id != inflight::opt_message_id(state.user_msg_id)
            || crate::services::provider::cancel_requested(owner.cancel_token.as_deref()))
    {
        return ReplayDebtDelivery::Retained("another actor owns the delivery");
    }
    // A rollover cursor excludes frozen messages. Without frozen prefixes, replace the complete
    // current body so a confirmed prefix at the same anchor is never overwritten by only its tail.
    let response = if state.streaming_rollover_frozen_msg_ids.is_empty() {
        state.full_response.as_str()
    } else if state.response_sent_offset > 0 {
        &state.full_response[state.response_sent_offset..]
    } else {
        return ReplayDebtDelivery::Retained("frozen debt cursor missing");
    };
    if response.trim().is_empty() {
        return ReplayDebtDelivery::Retained("no captured body");
    }
    let text = super::super::formatting::format_for_discord_with_provider(response, provider);
    let delivery =
        relay_captured_recovery_terminal_notice(http, shared, provider, state, &text).await;
    let committed = shared
        .mailbox(channel_id)
        .commit_captured_ready_delivery(CapturedReadyDeliveryCommit {
            shared: shared.clone(),
            state: state.clone(),
            actor: owner.cancel_token,
            delivery,
        })
        .await;
    if committed.is_some() {
        ReplayDebtDelivery::Acknowledged
    } else {
        ReplayDebtDelivery::Retained("captured delivery ack unconfirmed")
    }
}
