mod crud;
mod discord_delivery;
mod outbox;
#[cfg(test)]
mod tests;
mod thread_reuse;

// ── Re-exports: CRUD routes ──────────────────────────────────
pub use crud::{
    UpdateDispatchBody, create_dispatch, get_dispatch, list_dispatches, update_dispatch,
};

// ── Re-exports: Discord delivery ─────────────────────────────
pub(crate) use discord_delivery::send_dispatch_to_discord;

// ── Re-exports: Outbox ───────────────────────────────────────
pub use outbox::resolve_channel_alias_pub;
pub(crate) use outbox::use_counter_model_channel;
#[cfg(test)]
pub(crate) use outbox::{OutboxNotifier, process_outbox_batch};
pub(crate) use outbox::{dispatch_outbox_loop, queue_dispatch_followup};

// ── Re-exports: Thread reuse ─────────────────────────────────
pub(super) use thread_reuse::clear_all_threads;
pub(crate) use thread_reuse::validate_channel_thread_maps_on_startup;
pub use thread_reuse::{
    LinkDispatchThreadBody, get_card_thread, get_pending_dispatch_for_thread, link_dispatch_thread,
};

// ── Shared utilities (used by both discord_delivery and thread_reuse) ──

/// Resolve a channel name alias (e.g. "adk-cc") to a numeric channel ID.
/// Prefer agentdesk.yaml, then fall back to role_map.json.
pub(crate) fn resolve_channel_alias(alias: &str) -> Option<u64> {
    if let Some(channel_id) =
        crate::services::discord::agentdesk_config::resolve_channel_alias(alias)
    {
        return Some(channel_id);
    }

    let root = crate::cli::agentdesk_runtime_root()?;
    let path = root.join("config/role_map.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Strategy 1: Direct lookup in byChannelName → channelId field
    let by_name = json.get("byChannelName")?.as_object()?;
    if let Some(entry) = by_name.get(alias) {
        // If byChannelName entry has a channelId field, use it directly (most reliable)
        if let Some(id) = entry.get("channelId").and_then(|v| v.as_str()) {
            return id.parse().ok();
        }
        if let Some(id) = entry.get("channelId").and_then(|v| v.as_u64()) {
            return Some(id);
        }
    }

    // Strategy 2: Search byChannelId for entries whose channel name matches the alias
    // Each byChannelId entry may have been registered with a channel name
    let by_id = json.get("byChannelId")?.as_object()?;
    for (ch_id, ch_entry) in by_id {
        // Check if this entry's associated channel name matches our alias
        if let Some(ch_name) = ch_entry.get("channelName").and_then(|v| v.as_str()) {
            if ch_name == alias {
                return ch_id.parse().ok();
            }
        }
    }

    // Strategy 3: Fallback — roleId matching (original approach)
    if let Some(entry) = by_name.get(alias) {
        let role_id = entry.get("roleId").and_then(|v| v.as_str())?;
        let provider = entry.get("provider").and_then(|v| v.as_str());
        for (ch_id, ch_entry) in by_id {
            let entry_role = ch_entry.get("roleId").and_then(|v| v.as_str());
            let entry_provider = ch_entry.get("provider").and_then(|v| v.as_str());
            if entry_role == Some(role_id) {
                // If both have provider, must match. If either is missing, accept the match.
                if let (Some(p1), Some(p2)) = (provider, entry_provider) {
                    if p1 == p2 {
                        return ch_id.parse().ok();
                    }
                } else {
                    return ch_id.parse().ok();
                }
            }
        }
    }

    None
}

/// Parse a channel identifier (numeric ID or alias like "adk-cc") to u64.
pub(crate) fn parse_channel_id(channel: &str) -> Option<u64> {
    channel
        .parse::<u64>()
        .ok()
        .or_else(|| resolve_channel_alias(channel))
}
