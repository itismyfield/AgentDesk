//! Recent delivered-content fingerprints and deduplication.

use super::*;

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn delivered_content_hash(channel_id: u64, body: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&channel_id.to_le_bytes());
    hasher.update(body.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn delivered_content_fingerprint(
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
    delivered_at_epoch_ms: u64,
) -> Option<DeliveredContentFingerprint> {
    (!body.trim().is_empty()).then(|| DeliveredContentFingerprint {
        channel_id,
        content_hash: delivered_content_hash(channel_id, body),
        content_len: body.len() as u64,
        generation_mtime_ns,
        delivered_at_epoch_ms,
    })
}

fn recent_content_fingerprint_matches(
    record: &DeliveryRecord,
    fingerprint: &DeliveredContentFingerprint,
    now_ms: u64,
) -> bool {
    record.recent_delivered_contents.iter().any(|recent| {
        recent.channel_id == fingerprint.channel_id
            && recent.content_len == fingerprint.content_len
            && recent.content_hash == fingerprint.content_hash
            && recent.generation_mtime_ns == fingerprint.generation_mtime_ns
            && now_ms.saturating_sub(recent.delivered_at_epoch_ms)
                <= RECENT_DELIVERED_CONTENT_WINDOW_MS
    })
}

fn prune_recent_content_fingerprints(entries: &mut Vec<DeliveredContentFingerprint>, now_ms: u64) {
    entries.retain(|entry| {
        now_ms.saturating_sub(entry.delivered_at_epoch_ms) <= RECENT_DELIVERED_CONTENT_WINDOW_MS
    });
    if entries.len() > RECENT_DELIVERED_CONTENT_LIMIT {
        entries.drain(0..entries.len() - RECENT_DELIVERED_CONTENT_LIMIT);
    }
}

pub(super) fn record_delivered_content_fingerprint_at(
    path: &Path,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
    delivered_at_epoch_ms: u64,
) -> Result<(), String> {
    let Some(fingerprint) =
        delivered_content_fingerprint(channel_id, body, generation_mtime_ns, delivered_at_epoch_ms)
    else {
        return Ok(());
    };
    mutate_record_at(path, |record| {
        prune_recent_content_fingerprints(
            &mut record.recent_delivered_contents,
            delivered_at_epoch_ms,
        );
        record.recent_delivered_contents.retain(|recent| {
            !(recent.channel_id == fingerprint.channel_id
                && recent.content_len == fingerprint.content_len
                && recent.content_hash == fingerprint.content_hash
                && recent.generation_mtime_ns == fingerprint.generation_mtime_ns)
        });
        record.recent_delivered_contents.push(fingerprint);
        prune_recent_content_fingerprints(
            &mut record.recent_delivered_contents,
            delivered_at_epoch_ms,
        );
    })
}

pub(super) fn recent_delivered_content_matches_at(
    path: &Path,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
    now_ms: u64,
) -> bool {
    let Some(fingerprint) =
        delivered_content_fingerprint(channel_id, body, generation_mtime_ns, now_ms)
    else {
        return false;
    };
    read_record_at(path)
        .as_ref()
        .is_some_and(|record| recent_content_fingerprint_matches(record, &fingerprint, now_ms))
}

// Record a content fingerprint under the supplied channel and generation.
pub(super) fn record_delivered_content_fingerprint_for_generation(
    provider: &ProviderKind,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
) {
    let path = match record_path_or_err(provider, channel_id) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(target: "agentdesk::services::discord::outbound::delivery_record",
                provider = provider.as_str(),
                channel_id,
                error = %error,
                "delivery content fingerprint path unavailable"
            );
            return;
        }
    };
    if let Err(error) = record_delivered_content_fingerprint_at(
        &path,
        channel_id,
        body,
        generation_mtime_ns,
        now_epoch_ms(),
    ) {
        tracing::warn!(target: "agentdesk::services::discord::outbound::delivery_record",
            provider = provider.as_str(),
            channel_id,
            error = %error,
            "delivery content fingerprint write failed"
        );
    }
}

pub(in crate::services::discord::outbound) fn record_fresh_send_content_fingerprint(
    provider: &ProviderKind,
    channel_id: u64,
    body: &str,
    generation_mtime_ns: i64,
) -> Result<(), String> {
    let path = fresh_send_record_path(provider, channel_id)
        .ok_or_else(|| "fresh_send_record: runtime root unavailable".to_string())?;
    record_delivered_content_fingerprint_at(
        &path,
        channel_id,
        body,
        generation_mtime_ns,
        now_epoch_ms(),
    )
}

pub(in crate::services::discord::outbound) fn recent_fresh_send_content_matches(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    body: &str,
) -> bool {
    let Some(path) = fresh_send_record_path(provider, channel.get()) else {
        return false;
    };
    recent_delivered_content_matches_at(
        &path,
        channel.get(),
        body,
        current_generation_mtime_ns(tmux_session_name),
        now_epoch_ms(),
    )
}

pub(in crate::services::discord) fn record_delivered_content_fingerprint(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    body: &str,
) {
    record_delivered_content_fingerprint_for_generation(
        provider,
        channel.get(),
        body,
        current_generation_mtime_ns(tmux_session_name),
    );
}

pub(in crate::services::discord) fn recent_delivered_content_matches(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    body: &str,
) -> bool {
    let generation = current_generation_mtime_ns(tmux_session_name);
    if generation != 0 {
        return super::recent_delivered_content_matches_for_generation(
            provider, channel, generation, body,
        );
    }
    // Preserve the live path's existing lookup when its generation is unknown.
    let Some(path) = delivery_record_path(provider, channel.get()) else {
        return false;
    };
    recent_delivered_content_matches_at(&path, channel.get(), body, generation, now_epoch_ms())
}

/// Match a retained content witness only for a known source generation.
pub(in crate::services::discord) fn recent_delivered_content_matches_for_generation(
    provider: &ProviderKind,
    channel: ChannelId,
    generation_mtime_ns: i64,
    body: &str,
) -> bool {
    if generation_mtime_ns == 0 {
        return false;
    }
    let Some(path) = delivery_record_path(provider, channel.get()) else {
        return false;
    };
    recent_delivered_content_matches_at(
        &path,
        channel.get(),
        body,
        generation_mtime_ns,
        now_epoch_ms(),
    )
}
