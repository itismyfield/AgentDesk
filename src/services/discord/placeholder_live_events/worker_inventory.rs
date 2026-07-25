use std::sync::Mutex;

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;

use super::status_panel::StatusPanelState;

pub(super) fn live_background_worker_inventory(
    states: &DashMap<ChannelId, Mutex<StatusPanelState>>,
    channel_id: ChannelId,
) -> Vec<String> {
    let Some(entry) = states.get(&channel_id) else {
        return Vec::new();
    };
    let state = entry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut workers = state
        .subagents
        .iter()
        .filter(|slot| slot.is_unfinished_background())
        .map(|slot| {
            slot.agent_id
                .as_deref()
                .or(slot.tool_use_id.as_deref())
                .unwrap_or("unnamed_worker")
                .to_string()
        })
        .collect::<Vec<_>>();
    if state.background_agent_pending && workers.is_empty() {
        workers.push("background_agent_pending".to_string());
    }
    workers
}
