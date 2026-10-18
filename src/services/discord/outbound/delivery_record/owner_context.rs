//! Pinned delivery receipts and watcher owner context.

use super::*;

pub(super) fn pinned_source_receipt(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(ProviderKind, ConfirmedDeliveryReceipt), String> {
    let provider = ProviderKind::from_str(&source.provider)
        .filter(|_| source.is_authoritative() && message_id != 0)
        .ok_or_else(|| "delivery_record: incomplete pinned source receipt".to_string())?;
    Ok((
        provider,
        ConfirmedDeliveryReceipt {
            source: source.clone(),
            delivery_channel_id: source.delivery_channel_id,
            message_id,
        },
    ))
}
/// Append only the exact historical receipt; never alter the current frontier.
pub(in crate::services::discord) fn record_historical_pinned_delivery(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> Result<(), String> {
    let (provider, receipt) = pinned_source_receipt(source, message_id)?;
    let path = record_path_or_err(&provider, source.offset_authority_channel_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let _lock = lock_record_path(&path)?;
    let mut record = read_record_at(&path).unwrap_or_default();
    append_confirmed_receipt(&mut record, receipt);
    write_record_at(&path, &record)
}
pub(in crate::services::discord) fn historical_pinned_delivery_exists(
    source: &ExactJsonlSourceIdentity,
    message_id: u64,
) -> bool {
    let Ok((provider, receipt)) = pinned_source_receipt(source, message_id) else {
        return false;
    };
    read_record(&provider, source.offset_authority_channel_id)
        .is_some_and(|record| record.confirmed_deliveries.contains(&receipt))
}
pub(super) fn commit_ordered_jsonl_range_at(
    path: &Path,
    tmux_session_name: &str,
    range: (u64, u64),
    generation_mtime_ns: i64,
) -> Result<bool, String> {
    commit_ordered_jsonl_range_at_with_before_lock(
        path,
        tmux_session_name,
        range,
        generation_mtime_ns,
        || {},
    )
}

pub(super) fn commit_ordered_jsonl_range_at_with_before_lock(
    path: &Path,
    tmux_session_name: &str,
    range: (u64, u64),
    generation_mtime_ns: i64,
    before_lock: impl FnOnce(),
) -> Result<bool, String> {
    if generation_mtime_ns == 0 || range.1 <= range.0 {
        return Ok(false);
    }
    write_delivered_frontier_guarded_at_with_before_lock(
        path,
        tmux_session_name,
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        },
        before_lock,
    )?;
    Ok(true)
}

/// Durably commit a confirmed idle/catch-up JSONL range. Unlike the rollout
/// shadow writer, this is delivery authority and is therefore flag-independent.
pub(in crate::services::discord) fn commit_ordered_jsonl_range(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    range: (u64, u64),
    generation_mtime_ns: i64,
) -> Result<bool, String> {
    commit_ordered_jsonl_range_at(
        &record_path_or_err(provider, channel.get())?,
        tmux_session_name,
        range,
        generation_mtime_ns,
    )
}

pub(super) fn write_watcher_owner_context_at(
    path: &Path,
    watcher_owner_channel_id: u64,
    tmux_session_name: &str,
    generation_mtime_ns: i64,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_record_path(path)?;
    let context = WatcherOwnerContext {
        watcher_owner_channel_id,
        tmux_session_name: tmux_session_name.to_string(),
        generation_mtime_ns,
    };
    let data = serde_json::to_string_pretty(&context).map_err(|e| e.to_string())?;
    runtime_store::atomic_write(path, &data)
}

/// Mixed-binary-safe owner-channel index writer for #3751.
///
/// `InflightTurnState` carries the same field for fast in-row reads, but an old
/// binary can erase additive inflight fields by reserializing the row. This
/// sidecar writer records the owner under the delivery channel in the separate
/// `discord_delivery_owner_context` subtree that old binaries never scan and
/// known delivery-record mutators never rewrite.
pub(in crate::services::discord) fn record_watcher_owner_channel_context(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    watcher_owner_channel: ChannelId,
    tmux_session_name: &str,
) -> Result<(), String> {
    if tmux_session_name.is_empty() {
        return Ok(());
    }
    write_watcher_owner_context_at(
        &owner_context_path_or_err(provider, delivery_channel.get())?,
        watcher_owner_channel.get(),
        tmux_session_name,
        current_generation_mtime_ns(tmux_session_name),
    )
}

fn read_watcher_owner_context_at(path: &Path) -> Option<WatcherOwnerContext> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub(super) fn watcher_owner_channel_from_context_at(
    path: &Path,
    tmux_session_name: &str,
    current_generation_mtime_ns: i64,
) -> Option<u64> {
    let context = read_watcher_owner_context_at(path)?;
    if context.watcher_owner_channel_id == 0 || context.tmux_session_name != tmux_session_name {
        return None;
    }
    if context.generation_mtime_ns == 0
        || context.generation_mtime_ns != current_generation_mtime_ns
    {
        return None;
    }
    Some(context.watcher_owner_channel_id)
}

pub(in crate::services::discord) fn watcher_owner_channel_for_delivery_channel(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    tmux_session_name: &str,
) -> Option<ChannelId> {
    if tmux_session_name.is_empty() {
        return None;
    }
    let path = delivery_owner_context_path(provider, delivery_channel.get())?;
    watcher_owner_channel_from_context_at(
        &path,
        tmux_session_name,
        current_generation_mtime_ns(tmux_session_name),
    )
    .map(ChannelId::new)
}

fn resolved_receipt_owner_channel(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    source: &ExactJsonlSourceIdentity,
) -> Option<ChannelId> {
    if !source.is_authoritative()
        || current_generation_mtime_ns(&source.tmux_session_name) != source.generation_mtime_ns
    {
        return None;
    }

    let captured_owner =
        crate::services::discord::inflight::opt_channel_id(source.offset_authority_channel_id)?;
    match watcher_owner_channel_for_delivery_channel(
        provider,
        delivery_channel,
        &source.tmux_session_name,
    ) {
        Some(resolved) if resolved == captured_owner => Some(resolved),
        // Same-channel delivery is the deterministic default and predates the
        // split-channel owner-context sidecar. A split receipt, however, is
        // never trusted without the exact current context mapping.
        None if captured_owner == delivery_channel => Some(delivery_channel),
        _ => None,
    }
}

/// Re-read the durable receipt at restored-seed consumption time and require an
/// exact match on both JSONL source identity and Discord destination identity.
/// Missing/legacy/partially populated records fail closed (seed is preserved).
pub(in crate::services::discord) fn confirmed_delivery_receipt_exists(
    provider: &ProviderKind,
    delivery_channel: ChannelId,
    message_id: u64,
    source: &ExactJsonlSourceIdentity,
) -> bool {
    if message_id == 0
        || provider.as_str() != source.provider
        || delivery_channel.get() != source.delivery_channel_id
    {
        return false;
    }
    if super::historical_pinned_delivery_exists(source, message_id) {
        return true;
    }
    let Some(owner_channel) = resolved_receipt_owner_channel(provider, delivery_channel, source)
    else {
        return false;
    };
    read_record(provider, owner_channel.get()).is_some_and(|record| {
        record.confirmed_deliveries.iter().any(|receipt| {
            receipt.is_authoritative()
                && receipt.source == *source
                && receipt.delivery_channel_id == delivery_channel.get()
                && receipt.message_id == message_id
        })
    })
}
