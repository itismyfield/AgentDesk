//! Read-only delivered-frontier probes for diagnostics.

use std::path::Path;

use poise::serenity_prelude::ChannelId;

use super::delivery_record::{
    DeliveredCommit, current_generation_mtime_ns, delivery_record_path,
    durable_frontier_generation_current, read_record_at,
};
use crate::services::provider::ProviderKind;

/// #3610 PR-2: a recovery-time terminal delivery anchor, generation-validated.
///
/// `panel_msg_id` / `panel_channel_id` are the `(message, channel)` the committed
/// terminal answer actually lives in. `range` is the `(start, end)` JSONL slice
/// that commit covered. All three come from the same `delivered_frontier`, so
/// they are mutually consistent by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct CurrentGenerationAnchor {
    pub panel_msg_id: u64,
    pub panel_channel_id: u64,
    pub range: (u64, u64),
}

/// #3610 PR-2 (stale-anchor guard, path-based core): the durable
/// `delivered_frontier` terminal anchor, but only when it belongs to the current
/// wrapper generation and the anchor pair is fully populated/non-zero.
pub(in crate::services::discord) fn current_generation_delivered_anchor_at(
    path: &Path,
    current_gen_mtime: i64,
) -> Option<CurrentGenerationAnchor> {
    let frontier = read_record_at(path)
        .and_then(|r| r.delivered_frontier)
        .filter(|f| {
            durable_frontier_generation_current(f.generation_mtime_ns, current_gen_mtime)
        })?;
    let panel_msg_id = frontier.panel_msg_id.filter(|&id| id != 0)?;
    let panel_channel_id = frontier.panel_channel_id.filter(|&id| id != 0)?;
    Some(CurrentGenerationAnchor {
        panel_msg_id,
        panel_channel_id,
        range: frontier.range,
    })
}

/// #3610 PR-2: env-resolved wrapper over
/// [`current_generation_delivered_anchor_at`].
pub(in crate::services::discord) fn current_generation_delivered_anchor(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
) -> Option<CurrentGenerationAnchor> {
    let path = delivery_record_path(provider, channel.get())?;
    let current_gen = current_generation_mtime_ns(tmux_session_name);
    current_generation_delivered_anchor_at(&path, current_gen)
}

/// Current-generation durable delivered frontier with diagnostic details.
///
/// Idle recap uses this read-only view to report the same trusted frontier the
/// relay dedup machinery trusts, while preserving the committed range and
/// terminal anchor ids for a deterministic operator report. Missing/malformed
/// records and stale prior-generation frontiers return `None`.
pub(in crate::services::discord) fn delivered_frontier_current_generation(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
) -> Option<DeliveredCommit> {
    let path = delivery_record_path(provider, channel.get())?;
    let current_gen = current_generation_mtime_ns(tmux_session_name);
    read_record_at(&path)
        .and_then(|r| r.delivered_frontier)
        .filter(|f| durable_frontier_generation_current(f.generation_mtime_ns, current_gen))
}
