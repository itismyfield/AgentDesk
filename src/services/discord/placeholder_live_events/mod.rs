use std::collections::VecDeque;
use std::sync::Mutex;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use crate::services::agent_protocol::StatusEvent;
use crate::services::provider::ProviderKind;

mod background_task_events;
mod common;
mod completion_footer;
mod context_panel;
mod recent_events;
mod session_panel;
mod status_events;
mod status_panel;
mod subagent_rollout;
mod subagent_summary;
mod task_panel;
mod workflow_panel;

#[cfg(test)]
mod tests;

use common::CHANNEL_EVENT_CAPACITY;
pub(in crate::services::discord) use completion_footer::TerminalSlotId;
use completion_footer::{CompletionFooterRender, render_completion_footer};
use context_panel::ContextPanelSnapshot;
use recent_events::render_events;
use session_panel::SessionPanelSnapshot;
use status_panel::{StatusPanelState, render_status_panel};
pub(in crate::services::discord) use task_panel::TaskPanelInfo;
use task_panel::{TaskPanelSnapshot, clean_task_panel_value};

#[cfg(test)]
use common::{
    STATUS_PANEL_SUBAGENT_LIMIT, STATUS_PANEL_TODO_LIMIT, STATUS_PANEL_WORKFLOW_AGENT_LIMIT,
    STATUS_PANEL_WORKFLOW_LIMIT, STATUS_PANEL_WORKFLOW_PHASE_LIMIT,
};
#[cfg(test)]
use status_panel::{
    CompletedKind, DerivedStatus, render_recent_section_header, truncate_status_panel_sections,
};

pub(in crate::services::discord) use recent_events::RecentPlaceholderEvent;
pub(in crate::services::discord) use status_events::{
    status_events_from_task_notification_with_tool_use_id, status_events_from_tool_result_with_id,
    status_events_from_tool_use_with_id,
};
// #3034: the bare (no-id) variants are consumed only by the `tests` submodule
// (the prod path uses the `_with_id` variants above); a `#[cfg(test)]` re-export
// keeps them visible to tests without an `unused_imports` warning in the lib
// build.
#[cfg(test)]
pub(in crate::services::discord) use status_events::{
    status_events_from_json_for_footer_mode, status_events_from_task_notification,
    status_events_from_tool_result, status_events_from_tool_use,
    status_events_from_tool_use_with_id_for_footer_mode,
};

pub(in crate::services::discord) use recent_events::events_from_json;
pub(in crate::services::discord) use status_events::status_events_from_json;

#[derive(Debug, Default)]
pub(in crate::services::discord) struct PlaceholderLiveEvents {
    by_channel: dashmap::DashMap<ChannelId, Mutex<VecDeque<RecentPlaceholderEvent>>>,
    status_by_channel: dashmap::DashMap<ChannelId, Mutex<StatusPanelState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct LiveContextPanelSnapshot {
    pub(in crate::services::discord) provider_session_id: Option<String>,
    pub(in crate::services::discord) used_tokens: u64,
    pub(in crate::services::discord) context_window_tokens: u64,
}

impl PlaceholderLiveEvents {
    pub(in crate::services::discord) fn clear_channel(&self, channel_id: ChannelId) {
        self.by_channel.remove(&channel_id);
        self.status_by_channel.remove(&channel_id);
    }

    pub(in crate::services::discord) fn clear_channel_preserving_footer_residuals(
        &self,
        channel_id: ChannelId,
    ) {
        self.by_channel.remove(&channel_id);
        let has_residuals = self
            .status_by_channel
            .get(&channel_id)
            .is_some_and(|entry| {
                entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .reset_turn_content_preserving_unfinished_footer_residuals()
            });
        if !has_residuals {
            self.status_by_channel.remove(&channel_id);
        }
    }

    pub(in crate::services::discord) fn push_event(
        &self,
        channel_id: ChannelId,
        event: RecentPlaceholderEvent,
    ) {
        let entry = self
            .by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(VecDeque::with_capacity(CHANNEL_EVENT_CAPACITY)));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() >= CHANNEL_EVENT_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(event);
    }

    pub(in crate::services::discord) fn push_many<I>(&self, channel_id: ChannelId, events: I)
    where
        I: IntoIterator<Item = RecentPlaceholderEvent>,
    {
        for event in events {
            self.push_event(channel_id, event);
        }
    }

    pub(in crate::services::discord) fn render_block(
        &self,
        channel_id: ChannelId,
    ) -> Option<String> {
        let entry = self.by_channel.get(&channel_id)?;
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        render_events(guard.iter())
    }

    pub(in crate::services::discord) fn context_panel_snapshot(
        &self,
        channel_id: ChannelId,
    ) -> Option<LiveContextPanelSnapshot> {
        let entry = self.status_by_channel.get(&channel_id)?;
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let context = guard.context.as_ref()?;
        Some(LiveContextPanelSnapshot {
            provider_session_id: context.provider_session_id.clone(),
            used_tokens: context
                .input_tokens
                .saturating_add(context.cache_create_tokens)
                .saturating_add(context.cache_read_tokens),
            context_window_tokens: context.context_window_tokens,
        })
    }

    pub(in crate::services::discord) fn push_status_event(
        &self,
        channel_id: ChannelId,
        event: StatusEvent,
    ) {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.apply(event);
    }

    pub(in crate::services::discord) fn push_status_events<I>(
        &self,
        channel_id: ChannelId,
        events: I,
    ) where
        I: IntoIterator<Item = StatusEvent>,
    {
        for event in events {
            self.push_status_event(channel_id, event);
        }
    }

    pub(in crate::services::discord) fn set_session_panel_lifecycle_event(
        &self,
        channel_id: ChannelId,
        session_instance_key: Option<&str>,
        kind: &str,
        details: &Value,
    ) -> bool {
        let snapshot =
            SessionPanelSnapshot::from_lifecycle_event(session_instance_key, kind, details);
        self.set_session_panel_snapshot(channel_id, snapshot)
    }

    pub(in crate::services::discord) fn clear_session_panel(&self, channel_id: ChannelId) -> bool {
        self.set_session_panel_snapshot(channel_id, None)
    }

    fn set_session_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<SessionPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.session == snapshot {
            return false;
        }

        // #3087: detect a TRUE session boundary and reset the accumulated
        // subagents/tasks/todos/workflows exactly once, on the transition INTO a
        // new session INSTANCE.
        //
        // The boundary is keyed on the snapshot's `session_instance_key` — a
        // STABLE per-INSTANCE marker derived from the tmux `.spawn_nonce` spawn
        // file (`"{tmux_session_name}#{nonce}"`, where `nonce` is a per-spawn v4
        // UUID). That marker is written once per spawn and never rewritten by the
        // live wrapper, so it is invariant across every status tick and every
        // TURN of one session, and invariant across the `None`→`Some`
        // provider-session-id assignment that lands mid-turn on
        // `StreamMessage::Init`. A genuinely new session is a new tmux spawn
        // (`/clear`, idle-timeout, turn-cap, cancel→respawn, …) which mints a
        // fresh nonce, so the instance key changes exactly once on the real
        // boundary. (Earlier rounds keyed on the `.generation` mtime, which a
        // missing/duplicate mtime could collapse — the nonce is guaranteed
        // unique per spawn; see `tmux_session_files::session_panel_instance_key`.)
        //
        // This replaces the earlier per-turn `turn_id` keying, which reset on
        // EVERY turn of a no-provider-id session (each turn carries a new
        // turn_id) and was therefore BLOCKED. Resetting on a CHANGE of the
        // stable instance key fixes both false-reset P1s:
        //   * multi-turn same session (turn_id changes each turn, instance key
        //     unchanged) — NO reset, and
        //   * `None`→`Some` provider-id within one session (instance key
        //     unchanged) — NO reset.
        //
        // The provider-session delta is retained as a secondary trigger so a
        // `Some(a)`→`Some(b)` change to a genuinely different provider session
        // still resets even on the rare path where the instance key is
        // unavailable (`None`, e.g. headless / no live tmux marker). Unrelated
        // field churn (tmux/recovery_count) within the same instance must NOT
        // reset.
        let old_instance_key = guard
            .session
            .as_ref()
            .and_then(|session| session.session_instance_key())
            .map(str::to_owned);
        let old_provider_session_id = guard
            .session
            .as_ref()
            .and_then(|session| session.provider_session_id())
            .map(str::to_owned);

        // #3087 (codex Edge 5): gate the instance-key boundary on the OLD key
        // being `Some` too, mirroring the provider-id gate below. The instance
        // key can transition `None`→`Some` purely because the key became
        // AVAILABLE (e.g. `tmux_session_name` resolved, or the `.spawn_nonce`
        // marker became readable mid-turn) — that is NOT a session change and
        // must preserve the same-session accumulation. Only a `Some(a)`→`Some(b)`
        // change to a genuinely different spawn nonce is a real new-session
        // boundary. (A missing nonce yields `None`, so a respawn whose nonce is
        // unavailable never collides with a stored key here — the provider-id
        // delta below remains the secondary boundary, never a suppressed reset.)
        let new_instance_key = snapshot
            .as_ref()
            .and_then(|session| session.session_instance_key());
        let instance_boundary = match (old_instance_key.as_deref(), new_instance_key) {
            (Some(old), Some(new)) => old != new,
            // `None`→`Some` (key newly available) and `Some`→`None`/`None`→`None`
            // (key lost / never present) are availability transitions, not
            // session changes — never reset on the instance key alone.
            _ => false,
        };

        // Secondary trigger: a `Some(a)`→`Some(b)` change to a DIFFERENT
        // provider session, used only when the instance key cannot decide (e.g.
        // headless / no live tmux marker on either side). This is deliberately
        // gated on the OLD id being `Some` too, so a `None`→`Some` assignment
        // within one instance (the mid-turn `StreamMessage::Init`) never resets
        // (#3087 P1-B).
        let provider_session_boundary = old_provider_session_id.is_some()
            && snapshot
                .as_ref()
                .and_then(|session| session.provider_session_id())
                .is_some_and(|new_id| old_provider_session_id.as_deref() != Some(new_id));

        if instance_boundary || provider_session_boundary {
            guard.reset_session_content();
        }

        guard.session = snapshot;
        true
    }

    pub(in crate::services::discord) fn set_task_panel_info(
        &self,
        channel_id: ChannelId,
        info: TaskPanelInfo<'_>,
    ) -> bool {
        let dispatch_id = clean_task_panel_value(info.dispatch_id);
        if dispatch_id.is_empty() {
            return self.set_task_panel_snapshot(channel_id, None);
        }
        let clean_optional = |value: Option<&str>| {
            value
                .map(clean_task_panel_value)
                .filter(|value| !value.is_empty())
        };
        self.set_task_panel_snapshot(
            channel_id,
            Some(TaskPanelSnapshot {
                dispatch_id,
                card_id: clean_optional(info.card_id),
                dispatch_type: clean_optional(info.dispatch_type),
                owner_instance_id: clean_optional(info.owner_instance_id),
                card_title: clean_optional(info.card_title),
                dispatch_title: clean_optional(info.dispatch_title),
                github_issue_number: info.github_issue_number.filter(|n| *n > 0),
            }),
        )
    }

    fn set_task_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<TaskPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.task == snapshot {
            return false;
        }
        guard.task = snapshot;
        true
    }

    pub(in crate::services::discord) fn set_context_panel_usage(
        &self,
        channel_id: ChannelId,
        provider_session_id: Option<&str>,
        input_tokens: u64,
        cache_create_tokens: u64,
        cache_read_tokens: u64,
        context_window_tokens: u64,
        compact_percent: u64,
    ) -> bool {
        if context_window_tokens == 0 {
            return false;
        }
        self.set_context_panel_snapshot(
            channel_id,
            Some(ContextPanelSnapshot {
                provider_session_id: provider_session_id
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                input_tokens,
                cache_create_tokens,
                cache_read_tokens,
                context_window_tokens,
                compact_percent,
            }),
        )
    }

    fn set_context_panel_snapshot(
        &self,
        channel_id: ChannelId,
        snapshot: Option<ContextPanelSnapshot>,
    ) -> bool {
        let entry = self
            .status_by_channel
            .entry(channel_id)
            .or_insert_with(|| Mutex::new(StatusPanelState::default()));
        let mut guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.context == snapshot {
            return false;
        }
        guard.context = snapshot;
        true
    }

    pub(in crate::services::discord) fn render_status_panel(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
    ) -> String {
        self.render_status_panel_with_heartbeat(
            channel_id,
            provider,
            started_at_unix,
            chrono::Utc::now().timestamp(),
        )
    }

    fn render_status_panel_with_heartbeat(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
        heartbeat_at_unix: i64,
    ) -> String {
        let snapshot = self
            .status_by_channel
            .get(&channel_id)
            .map(|entry| {
                entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .unwrap_or_default();
        render_status_panel(
            snapshot,
            self.render_block(channel_id),
            provider,
            started_at_unix,
            heartbeat_at_unix,
        )
    }

    pub(in crate::services::discord) fn render_completion_footer(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        indicator: &str,
    ) -> CompletionFooterRender {
        let snapshot = self
            .status_by_channel
            .get(&channel_id)
            .map(|entry| {
                entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .unwrap_or_default();
        render_completion_footer(snapshot, provider, indicator)
    }
}
