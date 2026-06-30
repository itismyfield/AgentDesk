use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use crate::services::agent_protocol::StatusEvent;
use crate::services::provider::ProviderKind;

mod background_task_events;
mod common;
mod completion_footer;
mod context_panel;
mod freshness;
mod recent_events;
mod session_panel;
mod slot_rehydration;
mod status_events;
mod status_panel;
mod subagent_rollout;
mod subagent_summary;
mod task_panel;
mod turn_anchor;
mod workflow_panel;

#[cfg(test)]
mod tests;

use common::CHANNEL_EVENT_CAPACITY;
pub(in crate::services::discord) use completion_footer::TerminalSlotId;
use completion_footer::{CompletionFooterRender, render_completion_footer};
use context_panel::ContextPanelSnapshot;
use recent_events::render_compact_events;
#[cfg(test)]
use recent_events::render_events;
use session_panel::SessionPanelSnapshot;
use status_panel::{CompletedKind, DerivedStatus, StatusPanelState, render_status_panel};
pub(in crate::services::discord) use task_panel::TaskPanelInfo;
use task_panel::{TaskPanelSnapshot, clean_task_panel_value};

#[cfg(test)]
use common::{
    STATUS_PANEL_SUBAGENT_LIMIT, STATUS_PANEL_TODO_LIMIT, STATUS_PANEL_WORKFLOW_AGENT_LIMIT,
    STATUS_PANEL_WORKFLOW_LIMIT, STATUS_PANEL_WORKFLOW_PHASE_LIMIT,
};
#[cfg(test)]
use status_panel::{render_recent_section_header, truncate_status_panel_sections};

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
    status_events_from_task_notification_xml_for_footer_mode, status_events_from_tool_result,
    status_events_from_tool_use, status_events_from_tool_use_with_id_for_footer_mode,
};

pub(in crate::services::discord) use recent_events::events_from_json;
pub(in crate::services::discord) use status_events::status_events_from_json;

#[derive(Debug, Default)]
pub(in crate::services::discord) struct PlaceholderLiveEvents {
    by_channel: dashmap::DashMap<ChannelId, Mutex<VecDeque<RecentPlaceholderEvent>>>,
    status_by_channel: dashmap::DashMap<ChannelId, Mutex<StatusPanelState>>,
    // #3477 item 3: monotonic timestamp of the most recent live (Recent/terminal)
    // event push per channel. Compared against the turn's `completed_at` so a
    // late output batch that lands AFTER `TurnCompleted` still keeps the 🖥️ Recent
    // block visible (the completion suppression only hides STALE pre-completion
    // content, never a fresh late batch).
    last_recent_event_at: dashmap::DashMap<ChannelId, Instant>,
    // #3812: wall-clock unix stamp of the most recent live-content arrival per
    // channel, set once when the content lands (never recomputed at render). The
    // status panel's live/stale confidence line anchors its `<t:UNIX:R>` relative
    // age here, so the rendered text stays byte-identical across heartbeat ticks
    // (no needless re-edit) while Discord shows the localized live age client-side.
    last_recent_event_unix: dashmap::DashMap<ChannelId, i64>,
    // #3404 (codex r1): last logged live-panel compaction counts per channel —
    // the INFO line fires on count CHANGE only, not every render tick, so the
    // log stays usable as a compaction event counter for the relay scan.
    compaction_log_counts: dashmap::DashMap<ChannelId, (usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct LiveContextPanelSnapshot {
    pub(in crate::services::discord) provider_session_id: Option<String>,
    pub(in crate::services::discord) used_tokens: u64,
    pub(in crate::services::discord) context_window_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalUiObligationPanelStatus {
    Pending,
    Completed,
    Deadline,
}

impl PlaceholderLiveEvents {
    pub(in crate::services::discord) fn clear_channel(&self, channel_id: ChannelId) {
        self.by_channel.remove(&channel_id);
        self.status_by_channel.remove(&channel_id);
        self.compaction_log_counts.remove(&channel_id);
        self.last_recent_event_at.remove(&channel_id);
        self.last_recent_event_unix.remove(&channel_id); // #3812: drop the freshness anchor too.
    }

    pub(in crate::services::discord) fn clear_channel_preserving_footer_residuals(
        &self,
        channel_id: ChannelId,
    ) {
        self.by_channel.remove(&channel_id);
        // #3477 item 3: the recent-event ring is gone, so its freshness stamp is
        // stale — drop it with the ring so the next turn starts un-fresh.
        self.last_recent_event_at.remove(&channel_id);
        self.last_recent_event_unix.remove(&channel_id); // #3812: reset the freshness anchor.
        // #3404 (codex r2): a turn reset starts a new compaction episode — re-arm
        // the count-change log gate even when footer residuals survive.
        self.compaction_log_counts.remove(&channel_id);
        let keep_entry = self
            .status_by_channel
            .get(&channel_id)
            .is_some_and(|entry| {
                let mut guard = entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let has_residuals =
                    guard.reset_turn_content_preserving_unfinished_footer_residuals();
                // #3811: the reset preserves an intake-set 요청 anchor; keep the
                // entry alive when it carries one even with no footer residuals,
                // otherwise the whole entry (and the anchor) would be dropped here
                // before the turn renders its request link.
                has_residuals || guard.request_user_msg_id.is_some()
            });
        if !keep_entry {
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
        drop(guard);
        // #3477 item 3: stamp the live-content arrival so a render after
        // `TurnCompleted` can tell a fresh late batch (keep 🖥️ Recent) from a
        // stale pre-completion block (suppress on a genuinely idle completed turn).
        self.last_recent_event_at.insert(channel_id, Instant::now());
        // #3812: stamp the wall-clock arrival so the confidence line's `<t:UNIX:R>`
        // age anchors to a stable point set here, not recomputed per render tick.
        self.last_recent_event_unix
            .insert(channel_id, chrono::Utc::now().timestamp());
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
        self.render_compact_block(channel_id)
    }

    fn render_compact_block(&self, channel_id: ChannelId) -> Option<String> {
        let entry = self.by_channel.get(&channel_id)?;
        let guard = entry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        render_compact_events(guard.iter())
    }

    #[cfg(test)]
    fn render_raw_block_for_tests(&self, channel_id: ChannelId) -> Option<String> {
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

    /// #3886: `true` iff this channel's live panel still holds a non-terminal
    /// (`진행 중`) state. The TimedOut-completion-gate reconcile gates on this to
    /// finalize a stuck panel AT MOST ONCE (preserves #3477/#3812 byte-stability).
    pub(in crate::services::discord) fn status_panel_is_unfinished(
        &self,
        channel_id: ChannelId,
    ) -> bool {
        self.status_by_channel
            .get(&channel_id)
            .is_some_and(|entry| {
                let guard = entry.lock().unwrap_or_else(|p| p.into_inner());
                !matches!(guard.status, DerivedStatus::Completed { .. })
            })
    }

    /// #3393: bridge an observed `<task-notification>` XML user-record into the
    /// live-panel terminal StatusEvents for `channel_id`.
    pub(in crate::services::discord) fn bridge_task_notification_xml(
        &self,
        channel_id: ChannelId,
        raw: &str,
    ) {
        let events = status_events::status_events_from_task_notification_xml(raw);
        if events.is_empty() {
            return;
        }
        let parsed = super::tui_task_card::parse_task_notification(raw);
        tracing::info!(
            channel_id = channel_id.get(),
            kind = parsed.kind(),
            tool_use_id = parsed.tool_use_id.as_deref().unwrap_or(""),
            status = parsed.status.as_deref().unwrap_or(""),
            "#3393: bridged user-record <task-notification> XML to live panel terminal StatusEvents"
        );
        self.push_status_events(channel_id, events);
    }

    pub(in crate::services::discord) fn task_notification_completion_visible_in_footer_for_mode(
        &self,
        channel_id: ChannelId,
        raw: &str,
        footer_mode_enabled: bool,
    ) -> bool {
        if !footer_mode_enabled {
            return false;
        }
        let events =
            status_events::status_events_from_task_notification_xml_for_footer_mode(raw, true);
        if events.is_empty() {
            return false;
        }
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
        task_notification_success_completion_visible_in_snapshot(&snapshot, &events)
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

    pub(in crate::services::discord) fn render_terminal_ui_obligation_panel(
        &self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        started_at_unix: i64,
        status: TerminalUiObligationPanelStatus,
    ) -> String {
        let mut snapshot = self
            .status_by_channel
            .get(&channel_id)
            .map(|entry| {
                entry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .unwrap_or_default();
        snapshot.status = match status {
            TerminalUiObligationPanelStatus::Pending => DerivedStatus::TerminalDeliveryPending,
            TerminalUiObligationPanelStatus::Completed => DerivedStatus::Completed {
                kind: CompletedKind::Foreground,
            },
            TerminalUiObligationPanelStatus::Deadline => DerivedStatus::TerminalDeliveryUnconfirmed,
        };
        let completed = matches!(status, TerminalUiObligationPanelStatus::Completed);
        let request_anchor_line = self.request_anchor_line(channel_id, &snapshot);
        let confidence_line = self.panel_confidence_line(channel_id, &snapshot, started_at_unix);
        render_status_panel(
            snapshot,
            self.render_block(channel_id),
            provider,
            started_at_unix,
            chrono::Utc::now().timestamp(),
            !completed,
            request_anchor_line,
            confidence_line,
        )
    }

    // True when the live-panel compaction counts for this channel differ from
    // the last logged pair. Zero counts re-arm the gate (the next compaction
    // episode logs again) and never log themselves.
    fn compaction_counts_changed(
        &self,
        channel_id: ChannelId,
        evicted_tasks: usize,
        evicted_subagents: usize,
    ) -> bool {
        if evicted_tasks == 0 && evicted_subagents == 0 {
            self.compaction_log_counts.remove(&channel_id);
            return false;
        }
        let counts = (evicted_tasks, evicted_subagents);
        self.compaction_log_counts.insert(channel_id, counts) != Some(counts)
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
        // #3404: observability — log when completed Tasks/Subagents are compacted
        // out of the live panel (the live-side analogue of the completion-footer
        // eviction log), gated on count CHANGE so a long over-cap turn does not
        // repeat the same line every render tick (codex r1).
        let (evicted_tasks, evicted_subagents) =
            completion_footer::live_panel_compaction_counts(&snapshot, provider);
        if self.compaction_counts_changed(channel_id, evicted_tasks, evicted_subagents) {
            tracing::info!(
                target: "agentdesk::discord::live_panel",
                channel_id = channel_id.get(),
                evicted_tasks,
                evicted_subagents,
                "#3404: compacted delivered terminal slots from the live status panel"
            );
        }
        // #3477 item 3: a live batch counts as "fresh" when it arrived strictly
        // after the turn's completion instant. `None` completed_at means the turn
        // never completed (still running), so any present live content is fresh.
        let live_content_fresh = match snapshot.completed_at {
            Some(completed_at) => self
                .last_recent_event_at
                .get(&channel_id)
                .is_some_and(|stamp| *stamp.value() > completed_at),
            None => true,
        };
        let request_anchor_line = self.request_anchor_line(channel_id, &snapshot);
        let confidence_line = self.panel_confidence_line(channel_id, &snapshot, started_at_unix);
        render_status_panel(
            snapshot,
            self.render_block(channel_id),
            provider,
            started_at_unix,
            heartbeat_at_unix,
            live_content_fresh,
            request_anchor_line,
            confidence_line,
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
        let request_anchor_line = self.request_anchor_line(channel_id, &snapshot);
        render_completion_footer(snapshot, provider, indicator, request_anchor_line)
    }
}

fn task_notification_success_completion_visible_in_snapshot(
    snapshot: &StatusPanelState,
    events: &[StatusEvent],
) -> bool {
    events.iter().any(|event| match event {
        StatusEvent::BackgroundTaskEnd {
            tool_use_id,
            success: true,
        } => snapshot.tasks.iter().any(|slot| {
            slot.background && slot.tool_use_id.as_deref() == Some(tool_use_id.as_str())
        }),
        StatusEvent::SubagentEnd {
            success: true,
            tool_use_id: Some(tool_use_id),
            ack_only: false,
            ..
        } => snapshot
            .subagents
            .iter()
            .any(|slot| slot.tool_use_id.as_deref() == Some(tool_use_id.as_str())),
        // Completion footers render Tasks/Subagents only. Suppressing Workflow
        // completion cards here would drop the only completion signal.
        StatusEvent::WorkflowEnd { .. } => false,
        _ => false,
    })
}
