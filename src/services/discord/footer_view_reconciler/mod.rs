//! Completion-footer reconciler (#4049 S4-b1).
//!
//! The bridge and watcher notify this module about completion-footer state
//! changes; this module owns the registry transitions and the corresponding
//! Discord edits. Live in-progress footer edits remain on their existing paths
//! until S4-b2.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::{ProviderKind, SharedData, single_message_panel as smp};
use crate::services::agent_protocol::StatusEvent;

mod registry;

pub(in crate::services::discord) use registry::CompletionFooterOwner;

#[cfg(test)]
pub(in crate::services::discord) use registry::{
    completion_footer_edit_for_registered_target_at,
    completion_footer_edit_for_registered_target_at_for_owner,
    completion_footer_edit_still_registered, completion_footer_forget_registered_target,
    completion_footer_record_edit_result, completion_footer_record_edit_result_for_edit,
    completion_footer_registered_failure_count,
    completion_footer_supersede_registered_target_for_owner, register_completion_footer_target,
    register_completion_footer_target_for_owner,
};

#[derive(Clone, Copy)]
pub(in crate::services::discord) enum FooterViewWriter<'a> {
    Bridge {
        shared: &'a SharedData,
    },
    Watcher {
        shared: &'a Arc<SharedData>,
        http: &'a Arc<serenity::Http>,
    },
    #[cfg(test)]
    Test {
        shared: &'a SharedData,
        sink: &'a FooterViewTestSink,
    },
}

impl<'a> FooterViewWriter<'a> {
    pub(in crate::services::discord) fn bridge(shared: &'a SharedData) -> Self {
        Self::Bridge { shared }
    }

    pub(in crate::services::discord) fn watcher(
        shared: &'a Arc<SharedData>,
        http: &'a Arc<serenity::Http>,
    ) -> Self {
        Self::Watcher { shared, http }
    }

    #[cfg(test)]
    fn test(shared: &'a SharedData, sink: &'a FooterViewTestSink) -> Self {
        Self::Test { shared, sink }
    }

    fn shared(self) -> &'a SharedData {
        match self {
            Self::Bridge { shared } => shared,
            Self::Watcher { shared, .. } => shared.as_ref(),
            #[cfg(test)]
            Self::Test { shared, .. } => shared,
        }
    }

    async fn wait_for_discord_lane(self, channel_id: ChannelId) {
        if let Self::Watcher { shared, .. } = self {
            super::rate_limit_wait(shared, channel_id).await;
        }
    }

    async fn edit_channel_message(
        self,
        channel_id: ChannelId,
        msg_id: MessageId,
        text: &str,
        wait_before_edit: bool,
    ) -> Result<(), String> {
        if wait_before_edit {
            self.wait_for_discord_lane(channel_id).await;
        }
        match self {
            Self::Bridge { shared } => {
                let Some(http) = shared.serenity_http_or_token_fallback() else {
                    return Err("no Discord HTTP available for completion footer edit".to_string());
                };
                super::http::edit_channel_message(&http, channel_id, msg_id, text)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
            Self::Watcher { http, .. } => {
                super::http::edit_channel_message(http, channel_id, msg_id, text)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
            #[cfg(test)]
            Self::Test { sink, .. } => {
                sink.push(channel_id, msg_id, text);
                Ok(())
            }
        }
    }

    async fn warn_turn_end_wip(
        self,
        channel_id: ChannelId,
        provider: &ProviderKind,
        expected_user_msg_id: Option<u64>,
        source: &'static str,
    ) {
        let inflight = super::turn_end_wip_warning::load_matching_inflight_state(
            provider,
            channel_id,
            expected_user_msg_id,
        );
        match self {
            Self::Bridge { shared } => {
                let _ = super::turn_end_wip_warning::warn_turn_end_wip_with_shared_http(
                    shared,
                    channel_id,
                    inflight.as_ref(),
                    source,
                )
                .await;
            }
            Self::Watcher { http, .. } => {
                let _ = super::turn_end_wip_warning::warn_turn_end_wip_with_http(
                    http,
                    channel_id,
                    inflight.as_ref(),
                    source,
                )
                .await;
            }
            #[cfg(test)]
            Self::Test { .. } => {}
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct FooterViewRecordedEdit {
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) message_id: MessageId,
    pub(in crate::services::discord) text: String,
}

#[cfg(test)]
#[derive(Default)]
pub(in crate::services::discord) struct FooterViewTestSink {
    edits: std::sync::Mutex<Vec<FooterViewRecordedEdit>>,
}

#[cfg(test)]
impl FooterViewTestSink {
    fn push(&self, channel_id: ChannelId, message_id: MessageId, text: &str) {
        self.edits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(FooterViewRecordedEdit {
                channel_id,
                message_id,
                text: text.to_string(),
            });
    }

    fn edits(&self) -> Vec<FooterViewRecordedEdit> {
        self.edits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[derive(Debug, Clone)]
struct CompletionFooterTerminalEdit {
    message_id: MessageId,
    owner: CompletionFooterOwner,
    text: String,
    remove_after_edit: bool,
    completion_block: Option<String>,
    delivered_terminal_ids: Vec<super::placeholder_live_events::TerminalSlotId>,
}

struct CompletedFooterPlan {
    supersede_edit: Option<registry::CompletionFooterEdit>,
    terminal_edit: Option<CompletionFooterTerminalEdit>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_turn_completed_footer(
    shared: &SharedData,
    channel_id: ChannelId,
    terminal_msg_id: Option<MessageId>,
    owner: CompletionFooterOwner,
    provider: &ProviderKind,
    terminal_text: &str,
    indicator: &str,
    background: bool,
    background_agent_pending: bool,
) -> CompletedFooterPlan {
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::TurnCompleted {
            background,
            background_agent_pending,
        },
    );
    let rendered = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, provider, indicator);
    let Some(msg_id) = terminal_msg_id else {
        return CompletedFooterPlan {
            supersede_edit: None,
            terminal_edit: None,
        };
    };
    let supersede_edit = registry::register_completion_footer_target_for_owner(
        channel_id,
        msg_id,
        owner,
        provider,
        chrono::Utc::now().timestamp(),
        terminal_text,
        rendered.block.as_deref(),
        rendered.has_unfinished_entries,
    );
    let terminal_edit = smp::finalize_streaming_footer_with_completion(
        terminal_text,
        provider,
        rendered.block.as_deref(),
    )
    .map(|text| CompletionFooterTerminalEdit {
        message_id: msg_id,
        owner,
        text,
        remove_after_edit: !rendered.has_unfinished_entries,
        completion_block: rendered.block,
        delivered_terminal_ids: rendered.delivered_terminal_ids,
    });
    CompletedFooterPlan {
        supersede_edit,
        terminal_edit,
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn note_turn_completed_footer(
    writer: FooterViewWriter<'_>,
    channel_id: ChannelId,
    terminal_msg_id: Option<MessageId>,
    owner: CompletionFooterOwner,
    provider: &ProviderKind,
    terminal_text: &str,
    indicator: &str,
    background: bool,
    background_agent_pending: bool,
    source: &'static str,
) -> bool {
    let plan = prepare_turn_completed_footer(
        writer.shared(),
        channel_id,
        terminal_msg_id,
        owner,
        provider,
        terminal_text,
        indicator,
        background,
        background_agent_pending,
    );
    if let Some(edit) = plan.supersede_edit
        && let Err(error) = writer
            .edit_channel_message(channel_id, edit.message_id, &edit.text, true)
            .await
    {
        tracing::warn!(
            "[footer_view_reconciler] failed to supersede completion footer message {} in channel {} from {}: {}",
            edit.message_id,
            channel_id,
            source,
            error
        );
    }
    let Some(terminal_edit) = plan.terminal_edit else {
        return true;
    };
    writer
        .warn_turn_end_wip(channel_id, provider, Some(owner.user_msg_id), source)
        .await;
    let edited = match writer
        .edit_channel_message(
            channel_id,
            terminal_edit.message_id,
            &terminal_edit.text,
            true,
        )
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[footer_view_reconciler] failed to edit completion footer message {} in channel {} from {}: {}",
                terminal_edit.message_id,
                channel_id,
                source,
                error
            );
            false
        }
    };
    let recorded = registry::completion_footer_record_committed_text_result_for_owner(
        channel_id,
        terminal_edit.message_id,
        terminal_edit.owner,
        terminal_edit.remove_after_edit,
        edited,
        &terminal_edit.text,
        terminal_edit.completion_block.as_deref(),
    );
    if edited && recorded {
        writer
            .shared()
            .ui
            .placeholder_live_events
            .evict_delivered_terminal_footer_tasks(
                channel_id,
                &terminal_edit.delivered_terminal_ids,
            );
    }
    edited
}

pub(in crate::services::discord) async fn note_footer_superseded(
    writer: FooterViewWriter<'_>,
    channel_id: ChannelId,
    owner: CompletionFooterOwner,
    source: &'static str,
) -> bool {
    let Some(edit) =
        registry::completion_footer_supersede_registered_target_for_owner(channel_id, Some(owner))
    else {
        return false;
    };
    match writer
        .edit_channel_message(channel_id, edit.message_id, &edit.text, true)
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[footer_view_reconciler] failed to supersede completion footer message {} in channel {} from {}: {}",
                edit.message_id,
                channel_id,
                source,
                error
            );
            false
        }
    }
}

pub(in crate::services::discord) async fn note_background_refresh_due(
    writer: FooterViewWriter<'_>,
    channel_id: ChannelId,
    owner: Option<CompletionFooterOwner>,
    indicator: &str,
    source: &'static str,
) -> bool {
    let edit = if let Some(owner) = owner {
        registry::completion_footer_edit_for_registered_target_for_owner(
            writer.shared(),
            channel_id,
            owner,
            indicator,
        )
    } else {
        registry::completion_footer_edit_for_registered_target(
            writer.shared(),
            channel_id,
            indicator,
        )
    };
    let Some(edit) = edit else {
        return false;
    };
    writer.wait_for_discord_lane(channel_id).await;
    if !registry::completion_footer_edit_still_registered(channel_id, &edit) {
        return false;
    }
    let edited = match writer
        .edit_channel_message(channel_id, edit.message_id, &edit.text, false)
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[footer_view_reconciler] failed to refresh completion footer message {} in channel {} from {}: {}",
                edit.message_id,
                channel_id,
                source,
                error
            );
            false
        }
    };
    registry::completion_footer_record_edit_result_for_edit(
        writer.shared(),
        channel_id,
        &edit,
        edited,
    );
    edited
}

pub(in crate::services::discord) async fn note_footer_suppressed_for_tui_mirror(
    writer: FooterViewWriter<'_>,
    channel_id: ChannelId,
    terminal_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    terminal_text: &str,
    source: &'static str,
) -> bool {
    let Some(msg_id) = terminal_msg_id else {
        return true;
    };
    registry::completion_footer_forget_registered_target_if_message(channel_id, msg_id);
    let Some(finalized) =
        smp::finalize_streaming_footer_with_completion(terminal_text, provider, None)
    else {
        return true;
    };
    match writer
        .edit_channel_message(channel_id, msg_id, &finalized, true)
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                "[footer_view_reconciler] failed to strip TUI mirror completion footer on message {} in channel {} from {}: {}",
                msg_id,
                channel_id,
                source,
                error
            );
            false
        }
    }
}

pub(in crate::services::discord) fn note_footer_suppressed_for_message_takeover(
    channel_id: ChannelId,
    message_id: MessageId,
) -> bool {
    registry::completion_footer_forget_registered_target_if_message(channel_id, message_id)
}

pub(in crate::services::discord) fn completion_footer_has_registered_target(
    channel_id: ChannelId,
) -> bool {
    registry::completion_footer_has_registered_target(channel_id)
}

#[cfg(test)]
pub(in crate::services::discord) fn register_completion_footer_target_for_test(
    channel_id: ChannelId,
    message_id: MessageId,
    provider: &ProviderKind,
) {
    let _ = registry::register_completion_footer_target(
        channel_id,
        message_id,
        provider,
        chrono::Utc::now().timestamp(),
        "test footer",
        Some("test completion block"),
        true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_unfinished_subagent(channel_id: ChannelId) -> Arc<SharedData> {
        let shared = super::super::make_shared_data_for_tests();
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::SubagentStart {
                subagent_type: Some("bgworker".to_string()),
                desc: Some("Review".to_string()),
                agent_id: None,
                tool_use_id: Some(format!("toolu_agent_{}", channel_id.get())),
                background: true,
            },
        );
        shared
    }

    #[tokio::test]
    async fn bridge_watcher_completion_notifications_render_identical_edits() {
        let provider = ProviderKind::Claude;
        let bridge_channel = ChannelId::new(4_049_401);
        let watcher_channel = ChannelId::new(4_049_402);
        let bridge_shared = push_unfinished_subagent(bridge_channel);
        let watcher_shared = push_unfinished_subagent(watcher_channel);
        let bridge_sink = FooterViewTestSink::default();
        let watcher_sink = FooterViewTestSink::default();

        assert!(
            note_turn_completed_footer(
                FooterViewWriter::test(bridge_shared.as_ref(), &bridge_sink),
                bridge_channel,
                Some(MessageId::new(4_049_501)),
                CompletionFooterOwner::new(4_049_601, 1_800_000_000),
                &provider,
                "Final answer",
                "⠸",
                false,
                false,
                "bridge_test",
            )
            .await
        );
        assert!(
            note_turn_completed_footer(
                FooterViewWriter::test(watcher_shared.as_ref(), &watcher_sink),
                watcher_channel,
                Some(MessageId::new(4_049_502)),
                CompletionFooterOwner::new(4_049_602, 1_800_000_000),
                &provider,
                "Final answer",
                "⠸",
                false,
                false,
                "watcher_test",
            )
            .await
        );

        let bridge_edits = bridge_sink.edits();
        let watcher_edits = watcher_sink.edits();
        assert_eq!(bridge_edits.len(), 1);
        assert_eq!(watcher_edits.len(), 1);
        assert_eq!(bridge_edits[0].text, watcher_edits[0].text);
        completion_footer_forget_registered_target(bridge_channel);
        completion_footer_forget_registered_target(watcher_channel);
    }

    #[tokio::test]
    async fn footer_supersede_is_idempotent() {
        let channel_id = ChannelId::new(4_049_411);
        let shared = push_unfinished_subagent(channel_id);
        let sink = FooterViewTestSink::default();
        let provider = ProviderKind::Codex;
        let owner = CompletionFooterOwner::new(4_049_611, 1_800_000_000);
        let _ = register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(4_049_511),
            owner,
            &provider,
            owner.started_at_unix,
            "Old answer",
            None,
            true,
        );

        assert!(
            note_footer_superseded(
                FooterViewWriter::test(shared.as_ref(), &sink),
                channel_id,
                owner,
                "supersede_test",
            )
            .await
        );
        assert!(
            !note_footer_superseded(
                FooterViewWriter::test(shared.as_ref(), &sink),
                channel_id,
                owner,
                "supersede_test",
            )
            .await
        );
        assert_eq!(sink.edits().len(), 1);
    }

    #[tokio::test]
    async fn unfinished_background_refresh_schedules_registered_footer_edit() {
        let channel_id = ChannelId::new(4_049_421);
        let shared = push_unfinished_subagent(channel_id);
        let sink = FooterViewTestSink::default();
        let provider = ProviderKind::Claude;
        let owner = CompletionFooterOwner::new(4_049_621, 1_800_000_000);
        let _ = register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(4_049_521),
            owner,
            &provider,
            owner.started_at_unix,
            "Final answer",
            None,
            true,
        );

        assert!(
            note_background_refresh_due(
                FooterViewWriter::test(shared.as_ref(), &sink),
                channel_id,
                Some(owner),
                "⠼",
                "refresh_test",
            )
            .await
        );
        let edits = sink.edits();
        assert_eq!(edits.len(), 1);
        assert!(edits[0].text.contains("Review ⠼"));
        assert!(completion_footer_has_registered_target(channel_id));
        completion_footer_forget_registered_target(channel_id);
    }

    #[tokio::test]
    async fn tui_suppress_for_clean_mirror_forgets_target_without_write() {
        let channel_id = ChannelId::new(4_049_431);
        let shared = push_unfinished_subagent(channel_id);
        let sink = FooterViewTestSink::default();
        let provider = ProviderKind::Claude;
        let owner = CompletionFooterOwner::new(4_049_631, 1_800_000_000);
        let msg_id = MessageId::new(4_049_531);
        let _ = register_completion_footer_target_for_owner(
            channel_id,
            msg_id,
            owner,
            &provider,
            owner.started_at_unix,
            "Final answer",
            None,
            true,
        );

        assert!(
            note_footer_suppressed_for_tui_mirror(
                FooterViewWriter::test(shared.as_ref(), &sink),
                channel_id,
                Some(msg_id),
                &provider,
                "Final answer",
                "tui_suppress_test",
            )
            .await
        );
        assert!(sink.edits().is_empty());
        assert!(!completion_footer_has_registered_target(channel_id));
    }

    #[test]
    fn generation_guard_rejects_stale_two_message_status_completion() {
        assert!(super::super::turn_bridge::two_message_status_edit_generation_is_stale(1, true, 2));
        assert!(
            !super::super::turn_bridge::two_message_status_edit_generation_is_stale(1, false, 2)
        );
        assert!(
            !super::super::turn_bridge::two_message_status_edit_generation_is_stale(2, true, 2)
        );
    }
}
