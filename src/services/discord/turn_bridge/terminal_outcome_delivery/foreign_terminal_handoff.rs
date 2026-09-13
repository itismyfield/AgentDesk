//! Durable custody for a detached episode; retries only POST new messages.

use super::*;
use crate::services::discord::{
    inflight::{CodexRange, InflightTurnIdentity, RelayOwnerKind, load_inflight_state_read_only},
    outbound::{self, delivery_record as dr},
};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct RetrySnapshot {
    version: u8,
    provider: String,
    local: InflightTurnState,
    admitted: Option<CodexRange>,
    full_response: String,
    #[serde(default)]
    delivery_body: Option<String>,
    #[serde(default)]
    empty_recovery_notice: Option<String>,
    response_sent_offset: usize,
    channel_id: u64,
    old_anchor: u64,
    watcher_owner_channel_id: u64,
    tmux_last_offset: Option<u64>,
    cancelled: bool,
    cancel_source: String,
    children: Vec<i64>,
    dispatch_id: Option<String>,
    adk_cwd: Option<String>,
    should_complete: bool,
    should_fail: bool,
    resume_failure: bool,
    recovery_retry: bool,
    delivery_receipts: Vec<u64>,
}

pub(super) struct Handoff<'a> {
    pub provider: &'a ProviderKind,
    pub local: &'a InflightTurnState,
    pub admitted: Option<&'a CodexRange>,
    pub content: &'a str,
    pub delivery_body: Option<&'a str>,
    pub empty_recovery_notice: Option<&'a str>,
    pub response_sent_offset: usize,
    pub channel_id: ChannelId,
    pub old_anchor: MessageId,
    pub watcher_owner_channel_id: ChannelId,
    pub tmux_last_offset: Option<u64>,
    pub cancelled: bool,
    pub cancel_source: String,
    pub children: &'a [i64],
    pub dispatch_id: Option<&'a str>,
    pub adk_cwd: Option<&'a str>,
    pub should_complete: bool,
    pub should_fail: bool,
    pub resume_failure: bool,
    pub recovery_retry: bool,
}

pub(super) enum Outcome {
    Deferred { key: String },
    Unresolved { error: String },
}

pub(super) async fn preserve_or_publish(ctx: Handoff<'_>) -> Outcome {
    let snapshot = RetrySnapshot {
        version: 1,
        provider: ctx.provider.as_str().into(),
        local: ctx.local.clone(),
        admitted: ctx.admitted.cloned(),
        full_response: ctx.content.into(),
        delivery_body: ctx.delivery_body.map(str::to_owned),
        empty_recovery_notice: ctx.empty_recovery_notice.map(str::to_owned),
        response_sent_offset: ctx.response_sent_offset,
        channel_id: ctx.channel_id.get(),
        old_anchor: ctx.old_anchor.get(),
        watcher_owner_channel_id: ctx.watcher_owner_channel_id.get(),
        tmux_last_offset: ctx.tmux_last_offset,
        cancelled: ctx.cancelled,
        cancel_source: ctx.cancel_source,
        children: ctx.children.to_vec(),
        dispatch_id: ctx.dispatch_id.map(str::to_owned),
        adk_cwd: ctx.adk_cwd.map(str::to_owned),
        should_complete: ctx.should_complete,
        should_fail: ctx.should_fail,
        resume_failure: ctx.resume_failure,
        recovery_retry: ctx.recovery_retry,
        delivery_receipts: Vec::new(),
    };
    // Full captured source/episode/body survive a process restart. Do not replace
    // A's identity with the current row or today's source generation.
    let payload = serde_json::to_value(&snapshot).expect("retry snapshot serializes");
    let identity = serde_json::json!({
        "provider": snapshot.provider, "owner": snapshot.local.channel_id, "delivery": snapshot.channel_id,
        "user": snapshot.local.user_msg_id, "nonce": snapshot.local.turn_nonce, "started_at": snapshot.local.started_at,
        "tmux": snapshot.local.tmux_session_name, "source": snapshot.admitted.as_ref().map(|range| &range.source),
        "start": snapshot.local.turn_start_offset, "end": snapshot.tmux_last_offset,
        "path": snapshot.local.output_path, "born_generation": snapshot.local.born_generation,
        "sent_offset": snapshot.response_sent_offset, "body": outbound::outbound_fingerprint(&[&snapshot.full_response]),
    });
    let key = format!(
        "rowless-terminal:{}",
        outbound::outbound_fingerprint(&[&identity.to_string()])
    );
    match crate::services::discord::terminal_delivery_custody::persist(&key, &payload).await {
        Ok(()) => Outcome::Deferred { key },
        // A read/CAS error may mean another retained record exists. A blind
        // fallback POST here would race that record's publisher after restart.
        Err(error) => Outcome::Unresolved {
            error: format!("terminal custody did not acknowledge retention: {error}"),
        },
    }
}

pub(in crate::services::discord) async fn resume(
    registry: &crate::services::discord::health::HealthRegistry,
    payload: &mut serde_json::Value,
    checkpoint: &crate::services::discord::terminal_delivery_custody::CustodyCheckpoint,
) -> Result<bool, String> {
    let provider = payload
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .and_then(ProviderKind::from_str)
        .ok_or("unknown custody provider")?;
    let channel_id = payload
        .get("channel_id")
        .and_then(serde_json::Value::as_u64)
        .filter(|id| *id != 0)
        .ok_or("invalid custody channel")?;
    let Some(shared) = registry
        .shared_for_provider_on_channel(&provider, ChannelId::new(channel_id))
        .await
    else {
        return Ok(false);
    };
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return Ok(false);
    };
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        http,
        shared.clone(),
        provider,
        None,
    );
    resume_payload_with_gateway(&shared, &gateway, payload, checkpoint).await
}

pub(super) async fn resume_payload_with_gateway(
    shared: &Arc<SharedData>,
    gateway: &dyn TurnGateway,
    payload: &mut serde_json::Value,
    checkpoint: &crate::services::discord::terminal_delivery_custody::CustodyCheckpoint,
) -> Result<bool, String> {
    let mut snapshot: RetrySnapshot =
        serde_json::from_value(payload.clone()).map_err(|e| e.to_string())?;
    let result = resume_with_gateway(shared, gateway, &mut snapshot, checkpoint).await;
    // The store persists progress even on false/Err before its next retry. A
    // successful POST followed by a failed dispatch must never be POSTed again.
    *payload = serde_json::to_value(snapshot).map_err(|e| e.to_string())?;
    result
}

pub(super) async fn resume_with_gateway(
    shared: &Arc<SharedData>,
    gateway: &dyn TurnGateway,
    snapshot: &mut RetrySnapshot,
    checkpoint: &crate::services::discord::terminal_delivery_custody::CustodyCheckpoint,
) -> Result<bool, String> {
    if snapshot.version != 1
        || snapshot.channel_id == 0
        || snapshot.old_anchor == 0
        || snapshot.watcher_owner_channel_id == 0
    {
        return Err("unsupported or invalid terminal custody snapshot".into());
    }
    let provider = ProviderKind::from_str(&snapshot.provider).ok_or("unknown custody provider")?;
    let channel = ChannelId::new(snapshot.channel_id);
    let owner_channel = ChannelId::new(snapshot.watcher_owner_channel_id);
    if snapshot.delivery_body.is_none()
        && snapshot.full_response.trim().is_empty()
        && let Some(notice) = snapshot.empty_recovery_notice.as_deref()
    {
        let recovered =
            rowless_receipt::recover_empty_body(rowless_receipt::ReceiptDecisionInput {
                provider: &provider,
                channel_id: channel,
                current_msg_id: MessageId::new(snapshot.old_anchor),
                watcher_owner_channel_id: owner_channel,
                entry_was_rowless: true,
                codex_tui_terminal_range: snapshot.admitted.as_ref(),
                tmux_last_offset: snapshot.tmux_last_offset,
                inflight_state: &snapshot.local,
                full_response: &snapshot.full_response,
            })?;
        snapshot.delivery_body = Some(terminal_delivery_response_after_offset(
            &recovered,
            0,
            Some(notice),
        ));
    }
    let content = snapshot.delivery_body.clone().unwrap_or_else(|| {
        terminal_delivery_response_after_offset(
            &snapshot.full_response,
            snapshot.response_sent_offset,
            None,
        )
    });
    let chunks = crate::services::discord::formatting::split_message(&content);
    if chunks.is_empty() || content.trim().is_empty() {
        return Err("custody has no independently publishable terminal body".into());
    }
    if snapshot.delivery_receipts.len() > chunks.len()
        || snapshot.delivery_receipts.iter().any(|id| {
            *id == 0
                || super::super::headless_delivery::is_synthetic_headless_message_id(
                    MessageId::new(*id),
                )
        })
    {
        return Err("invalid terminal custody transport receipts".into());
    }
    let decision = || {
        rowless_receipt::decision(rowless_receipt::ReceiptDecisionInput {
            provider: &provider,
            channel_id: channel,
            current_msg_id: MessageId::new(snapshot.old_anchor),
            watcher_owner_channel_id: owner_channel,
            entry_was_rowless: true,
            codex_tui_terminal_range: snapshot.admitted.as_ref(),
            tmux_last_offset: snapshot.tmux_last_offset,
            inflight_state: &snapshot.local,
            full_response: &snapshot.full_response,
        })
    };
    let mut delivered = snapshot.delivery_receipts.len() == chunks.len()
        || decision() == rowless_receipt::TerminalReceiptDisposition::AlreadyDelivered;
    let mut held_lease = None;
    if !delivered {
        // Reuse the row's existing owner contract. A foreign row is never A's
        // owner; the shared lease still protects a concurrent live publisher.
        if load_inflight_state_read_only(&provider, snapshot.local.channel_id).is_some_and(|row| {
            InflightTurnIdentity::from_state(&snapshot.local).matches_state(&row)
                && row.turn_nonce == snapshot.local.turn_nonce
                && row.effective_relay_owner_kind() != RelayOwnerKind::None
        }) {
            return Ok(false);
        }
        let lease = bridge_delivery_lease_for_inflight(
            shared,
            owner_channel,
            shared.restart.current_generation,
            &snapshot.local,
            snapshot.tmux_last_offset,
        );
        if matches!(lease, BridgeLeaseAcquire::Skip) {
            return Ok(false);
        }
        // Recheck after acquiring; an actor may have completed just before this
        // lease became available. NoRange remains the existing honest exemption.
        delivered = decision() == rowless_receipt::TerminalReceiptDisposition::AlreadyDelivered;
        if !delivered {
            // Reuse the ordinary chunk formatter and gateway. Persist each
            // acknowledged prefix before another await, while the SAME source
            // lease and custody file lock remain held across the whole loop.
            for chunk in chunks.iter().skip(snapshot.delivery_receipts.len()) {
                let id = gateway.send_message(channel, chunk).await?;
                if super::super::headless_delivery::is_synthetic_headless_message_id(id) {
                    return Err("terminal POST returned no real Discord receipt".into());
                }
                snapshot.delivery_receipts.push(id.get());
                checkpoint
                    .persist(&serde_json::to_value(&*snapshot).map_err(|e| e.to_string())?)?;
            }
        }
        // Custody never advances a read cursor, clears the foreign row or
        // adopts its anchor. Drop releases only this held lease.
        held_lease = Some(lease);
    }
    if let Some(admitted) = snapshot.admitted.as_ref()
        && let Some(message_id) = snapshot.delivery_receipts.last()
        && admitted.identity.matches_state(&snapshot.local)
        && admitted.result == snapshot.full_response
        && rowless_receipt::source_matches_episode(
            &admitted.source,
            &provider,
            &snapshot.local,
            owner_channel,
            channel,
        )
    {
        crate::services::tmux_common::with_tmux_source_authority(
            &admitted.source.tmux_session_name,
            |authority| {
                if admitted.source_receipt_is_live(authority) {
                    dr::record_historical_pinned_delivery(&admitted.source, *message_id)
                } else {
                    Ok(())
                }
            },
        )?;
    }
    drop(held_lease);
    if snapshot.cancelled {
        return Ok(!cancel_prompt_replace::settle_cancelled_episode_work(
            shared,
            snapshot.dispatch_id.as_deref(),
            &snapshot.cancel_source,
            &mut snapshot.children,
        )
        .await);
    }
    let dispatch_requires_settlement = delivery_epilogue::settle_terminal_dispatch(
        delivery_epilogue::TerminalDispatchSettlement {
            shared,
            dispatch_id: snapshot.dispatch_id.as_deref(),
            adk_cwd: snapshot.adk_cwd.as_deref(),
            full_response: &snapshot.full_response,
            should_complete: snapshot.should_complete,
            should_fail: snapshot.should_fail,
            committed: true,
            preserve: false,
            resume_failure: snapshot.resume_failure,
            recovery_retry: snapshot.recovery_retry,
        },
    )
    .await;
    if !dispatch_requires_settlement {
        return Ok(true);
    }
    let Some(dispatch_id) = snapshot.dispatch_id.as_deref() else {
        return Ok(true);
    };
    let status = if let Some(pool) = shared.pg_pool.as_ref() {
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id=$1")
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?
    } else {
        super::super::completion_guard::fetch_dispatch_snapshot(shared.api_port, dispatch_id)
            .await
            .map(|s| s.status)
    };
    Ok(
        status
            .is_some_and(|status| matches!(status.as_str(), "completed" | "failed" | "cancelled")),
    )
}
