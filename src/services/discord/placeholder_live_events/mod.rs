use std::collections::VecDeque;
use std::sync::Mutex;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use crate::services::agent_protocol::StatusEvent;
use crate::services::provider::ProviderKind;

mod common;
mod context_panel;
mod recent_events;
mod session_panel;
mod status_events;
mod status_panel;
mod task_panel;
mod workflow_panel;

#[cfg(test)]
mod tests;

use common::CHANNEL_EVENT_CAPACITY;
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
    status_events_from_task_notification, status_events_from_tool_result,
    status_events_from_tool_use,
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
        turn_id: &str,
        kind: &str,
        details: &Value,
    ) -> bool {
        let snapshot = SessionPanelSnapshot::from_lifecycle_event(Some(turn_id), kind, details);
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
        // new session instance.
        //
        // There are two kinds of boundary:
        //   (a) provider-session delta — the new snapshot carries a non-empty
        //       provider_session_id that differs from the stored one (resume /
        //       reconnect to a different provider session).
        //   (b) fresh-session boundary — a genuinely fresh session legitimately
        //       arrives with provider_session_id == None (the common /clear,
        //       idle-timeout, turn-cap, goal-fresh, no_cached_provider_session
        //       paths all normalize to None). For these we cannot key off the
        //       provider id, so we key off the per-instance `turn_id`: a NEW
        //       fresh session arrives on a NEW turn, while every status tick of
        //       the SAME fresh session re-loads the SAME turn's lifecycle row.
        //
        // The `turn_id` marker is what prevents the every-tick / mid-session
        // false-reset: because the Fresh kind PERSISTS across ticks of one fresh
        // session, a naive "reset whenever kind == Fresh" would wipe live
        // subagents/tasks on every tick. Gating on a CHANGE of the fresh turn id
        // resets only on the first tick of each new fresh session and preserves
        // mid-session accumulation thereafter.
        //
        // Unrelated field churn (tmux/recovery_count) within the same session —
        // same provider id, or same fresh turn id — must NOT reset, and a
        // missing→missing id with an unchanged fresh turn id must not trigger a
        // spurious reset.
        let old_provider_session_id = guard
            .session
            .as_ref()
            .and_then(|session| session.provider_session_id())
            .map(str::to_owned);
        let old_fresh_turn_id = guard
            .session
            .as_ref()
            .filter(|session| session.is_fresh())
            .and_then(|session| session.turn_id())
            .map(str::to_owned);

        let provider_session_boundary = snapshot
            .as_ref()
            .and_then(|session| session.provider_session_id())
            .is_some_and(|new_id| old_provider_session_id.as_deref() != Some(new_id));

        let fresh_boundary = snapshot
            .as_ref()
            .filter(|session| session.is_fresh() && session.provider_session_id().is_none())
            .and_then(|session| session.turn_id())
            .is_some_and(|new_turn_id| old_fresh_turn_id.as_deref() != Some(new_turn_id));

        if provider_session_boundary || fresh_boundary {
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
}
