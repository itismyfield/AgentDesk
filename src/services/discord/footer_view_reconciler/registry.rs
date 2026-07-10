use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::discord::{
    ProviderKind, SharedData, placeholder_live_events::TerminalSlotId, single_message_panel as smp,
};

#[derive(Debug, Clone)]
struct RegisteredCompletionFooter {
    message_id: MessageId,
    owner: CompletionFooterOwner,
    provider: ProviderKind,
    base_body: String,
    last_completion_block: Option<String>,
    last_committed_text: Option<String>,
    registered_at_unix: i64,
    consecutive_edit_failures: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterOwner {
    pub(in crate::services::discord) user_msg_id: u64,
    pub(in crate::services::discord) started_at_unix: i64,
}

impl CompletionFooterOwner {
    pub(in crate::services::discord) fn new(user_msg_id: u64, started_at_unix: i64) -> Self {
        Self {
            user_msg_id,
            started_at_unix,
        }
    }

    #[cfg(test)]
    fn unknown() -> Self {
        Self {
            user_msg_id: 0,
            started_at_unix: 0,
        }
    }
}

fn completion_footer_owner_is_newer_than(
    registered: CompletionFooterOwner,
    takeover: CompletionFooterOwner,
) -> bool {
    if registered.started_at_unix != takeover.started_at_unix {
        return registered.started_at_unix > takeover.started_at_unix;
    }
    if registered.user_msg_id == 0 {
        return false;
    }
    takeover.user_msg_id == 0 || registered.user_msg_id > takeover.user_msg_id
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterEdit {
    pub(in crate::services::discord) message_id: MessageId,
    pub(in crate::services::discord) text: String,
    pub(in crate::services::discord) remove_after_edit: bool,
    owner: CompletionFooterOwner,
    pub(in crate::services::discord) completion_block: Option<String>,
    // #3391: identities of terminal task/subagent slots in `text`; evicted by
    // slot identity (not line string) once this edit is delivered.
    pub(in crate::services::discord) delivered_terminal_ids: Vec<TerminalSlotId>,
}

fn completion_footer_registry() -> &'static Mutex<HashMap<u64, RegisteredCompletionFooter>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, RegisteredCompletionFooter>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(in crate::services::discord) fn register_completion_footer_target(
    channel_id: ChannelId,
    message_id: MessageId,
    provider: &ProviderKind,
    registered_at_unix: i64,
    base_body: &str,
    completion_block: Option<&str>,
    has_unfinished_entries: bool,
) -> Option<CompletionFooterEdit> {
    register_completion_footer_target_for_owner(
        channel_id,
        message_id,
        CompletionFooterOwner::unknown(),
        provider,
        registered_at_unix,
        base_body,
        completion_block,
        has_unfinished_entries,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn register_completion_footer_target_for_owner(
    channel_id: ChannelId,
    message_id: MessageId,
    owner: CompletionFooterOwner,
    provider: &ProviderKind,
    registered_at_unix: i64,
    base_body: &str,
    completion_block: Option<&str>,
    has_unfinished_entries: bool,
) -> Option<CompletionFooterEdit> {
    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = guard.remove(&channel_id.get());
    if let Some(target) = previous.as_ref()
        && target.message_id != message_id
        && completion_footer_owner_is_newer_than(target.owner, owner)
    {
        tracing::warn!(
            target: "agentdesk.discord.single_message_panel",
            event = "completion_footer_register_skipped",
            channel_id = channel_id.get(),
            old_message_id = target.message_id.get(),
            attempted_message_id = message_id.get(),
            registered_owner_user_msg_id = target.owner.user_msg_id,
            registered_owner_started_at_unix = target.owner.started_at_unix,
            attempted_owner_user_msg_id = owner.user_msg_id,
            attempted_owner_started_at_unix = owner.started_at_unix,
            provider = ?target.provider,
            reason = if has_unfinished_entries {
                "newer_target_already_registered"
            } else {
                "newer_target_clear_skipped"
            },
            "completion footer target registration skipped because a newer target is registered"
        );
        guard.insert(channel_id.get(), target.clone());
        return None;
    }
    if has_unfinished_entries {
        let base_body = smp::completion_footer_base_body(base_body, provider);
        let owner = previous
            .as_ref()
            .filter(|target| target.message_id == message_id)
            .map(|target| target.owner)
            .unwrap_or(owner);
        let last_committed_text = previous
            .as_ref()
            .filter(|target| target.message_id == message_id)
            .and_then(|target| target.last_committed_text.clone());
        let registered_at_unix = previous
            .as_ref()
            .filter(|target| target.message_id == message_id)
            .map(|target| target.registered_at_unix)
            .unwrap_or(registered_at_unix);
        let consecutive_edit_failures = previous
            .as_ref()
            .filter(|target| target.message_id == message_id)
            .map(|target| target.consecutive_edit_failures)
            .unwrap_or(0);
        if let Some(target) = previous.as_ref() {
            if target.message_id == message_id {
                tracing::debug!(
                    target: "agentdesk.discord.single_message_panel",
                    event = "completion_footer_target_duplicate_discarded",
                    channel_id = channel_id.get(),
                    message_id = message_id.get(),
                    owner_user_msg_id = owner.user_msg_id,
                    owner_started_at_unix = owner.started_at_unix,
                    provider = ?provider,
                    reason = "same_message_re_registered",
                    "completion footer kept existing edit target"
                );
            } else {
                tracing::info!(
                    target: "agentdesk.discord.single_message_panel",
                    event = "completion_footer_target_changed",
                    channel_id = channel_id.get(),
                    old_message_id = target.message_id.get(),
                    new_message_id = message_id.get(),
                    old_owner_user_msg_id = target.owner.user_msg_id,
                    old_owner_started_at_unix = target.owner.started_at_unix,
                    new_owner_user_msg_id = owner.user_msg_id,
                    new_owner_started_at_unix = owner.started_at_unix,
                    provider = ?provider,
                    reason = "new_message_registered",
                    "completion footer edit target changed"
                );
            }
        } else {
            tracing::debug!(
                target: "agentdesk.discord.single_message_panel",
                event = "completion_footer_target_registered",
                channel_id = channel_id.get(),
                message_id = message_id.get(),
                owner_user_msg_id = owner.user_msg_id,
                owner_started_at_unix = owner.started_at_unix,
                provider = ?provider,
                reason = "new_target",
                "completion footer edit target registered"
            );
        }
        guard.insert(
            channel_id.get(),
            RegisteredCompletionFooter {
                message_id,
                owner,
                provider: provider.clone(),
                base_body,
                last_completion_block: completion_block.map(str::to_string),
                last_committed_text,
                registered_at_unix,
                consecutive_edit_failures,
            },
        );
    } else if let Some(target) = previous.as_ref() {
        tracing::debug!(
            target: "agentdesk.discord.single_message_panel",
            event = "completion_footer_target_cleared",
            channel_id = channel_id.get(),
            old_message_id = target.message_id.get(),
            old_owner_user_msg_id = target.owner.user_msg_id,
            old_owner_started_at_unix = target.owner.started_at_unix,
            provider = ?target.provider,
            reason = "no_unfinished_entries",
            "completion footer target cleared"
        );
    }
    previous
        .filter(|target| target.message_id != message_id)
        .map(supersede_edit_from_registered_target)
}

pub(in crate::services::discord) fn completion_footer_supersede_registered_target_for_owner(
    channel_id: ChannelId,
    takeover_owner: Option<CompletionFooterOwner>,
) -> Option<CompletionFooterEdit> {
    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let target = guard.get(&channel_id.get())?.clone();
    if let Some(owner) = takeover_owner
        && completion_footer_owner_is_newer_than(target.owner, owner)
    {
        tracing::warn!(
            target: "agentdesk.discord.single_message_panel",
            event = "completion_footer_supersede_skipped",
            channel_id = channel_id.get(),
            old_message_id = target.message_id.get(),
            old_owner_user_msg_id = target.owner.user_msg_id,
            old_owner_started_at_unix = target.owner.started_at_unix,
            new_owner_user_msg_id = owner.user_msg_id,
            new_owner_started_at_unix = owner.started_at_unix,
            provider = ?target.provider,
            reason = "newer_target_already_registered",
            "completion footer supersede skipped because a newer target is registered"
        );
        return None;
    }
    let target = guard.remove(&channel_id.get())?;
    tracing::info!(
        target: "agentdesk.discord.single_message_panel",
        event = "completion_footer_target_superseded",
        channel_id = channel_id.get(),
        old_message_id = target.message_id.get(),
        old_owner_user_msg_id = target.owner.user_msg_id,
        old_owner_started_at_unix = target.owner.started_at_unix,
        new_owner_user_msg_id = takeover_owner.map(|owner| owner.user_msg_id),
        new_owner_started_at_unix = takeover_owner.map(|owner| owner.started_at_unix),
        provider = ?target.provider,
        reason = "explicit_supersede",
        "completion footer edit target superseded"
    );
    Some(supersede_edit_from_registered_target(target))
}

#[cfg(test)]
pub(in crate::services::discord) fn completion_footer_registered_failure_count(
    channel_id: ChannelId,
) -> Option<u8> {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&channel_id.get())
        .map(|target| target.consecutive_edit_failures)
}

pub(in crate::services::discord) fn completion_footer_has_registered_target(
    channel_id: ChannelId,
) -> bool {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&channel_id.get())
}

pub(in crate::services::discord) fn completion_footer_edit_for_registered_target(
    shared: &SharedData,
    channel_id: ChannelId,
    indicator: &str,
) -> Option<CompletionFooterEdit> {
    completion_footer_edit_for_registered_target_at(
        shared,
        channel_id,
        indicator,
        chrono::Utc::now().timestamp(),
    )
}

pub(in crate::services::discord) fn completion_footer_edit_for_registered_target_for_owner(
    shared: &SharedData,
    channel_id: ChannelId,
    owner: CompletionFooterOwner,
    indicator: &str,
) -> Option<CompletionFooterEdit> {
    completion_footer_edit_for_registered_target_at_for_owner(
        shared,
        channel_id,
        Some(owner),
        indicator,
        chrono::Utc::now().timestamp(),
    )
}

pub(in crate::services::discord) fn completion_footer_edit_for_registered_target_at(
    shared: &SharedData,
    channel_id: ChannelId,
    indicator: &str,
    now_unix: i64,
) -> Option<CompletionFooterEdit> {
    completion_footer_edit_for_registered_target_at_for_owner(
        shared, channel_id, None, indicator, now_unix,
    )
}

pub(in crate::services::discord) fn completion_footer_edit_for_registered_target_at_for_owner(
    shared: &SharedData,
    channel_id: ChannelId,
    owner: Option<CompletionFooterOwner>,
    indicator: &str,
    now_unix: i64,
) -> Option<CompletionFooterEdit> {
    let target = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&channel_id.get())
        .cloned()?;
    if let Some(expected) = owner
        && target.owner != expected
    {
        tracing::warn!(
            target: "agentdesk.discord.single_message_panel",
            event = "completion_footer_edit_skipped",
            channel_id = channel_id.get(),
            message_id = target.message_id.get(),
            registered_owner_user_msg_id = target.owner.user_msg_id,
            registered_owner_started_at_unix = target.owner.started_at_unix,
            expected_owner_user_msg_id = expected.user_msg_id,
            expected_owner_started_at_unix = expected.started_at_unix,
            provider = ?target.provider,
            reason = "owner_mismatch",
            "completion footer edit skipped because caller does not own the target"
        );
        return None;
    }
    let idle_expired =
        completion_footer_idle_animation_expired(target.registered_at_unix, now_unix);
    let render_indicator = if idle_expired {
        smp::COMPLETION_FOOTER_IDLE_EXPIRED_INDICATOR
    } else {
        indicator
    };
    let rendered = shared.ui.placeholder_live_events.render_completion_footer(
        channel_id,
        &target.provider,
        render_indicator,
    );
    let completion_block = rendered.block;
    let text = smp::compose_completion_footer_text(&target.base_body, completion_block.as_deref());
    let remove_after_edit = idle_expired || !rendered.has_unfinished_entries;
    if text.trim().is_empty() {
        if idle_expired {
            completion_footer_forget_registered_target_if_identity(
                channel_id,
                target.message_id,
                target.owner,
            );
        }
        return None;
    }
    if target.last_committed_text.as_deref() == Some(text.as_str()) {
        tracing::debug!(
            target: "agentdesk.discord.single_message_panel",
            event = "completion_footer_edit_skipped",
            channel_id = channel_id.get(),
            message_id = target.message_id.get(),
            owner_user_msg_id = target.owner.user_msg_id,
            owner_started_at_unix = target.owner.started_at_unix,
            provider = ?target.provider,
            reason = "unchanged_text",
            "completion footer edit skipped because rendered text is unchanged"
        );
        if remove_after_edit {
            completion_footer_forget_registered_target_if_identity(
                channel_id,
                target.message_id,
                target.owner,
            );
        }
        return None;
    }
    Some(CompletionFooterEdit {
        message_id: target.message_id,
        text,
        remove_after_edit,
        owner: target.owner,
        completion_block,
        delivered_terminal_ids: rendered.delivered_terminal_ids,
    })
}

pub(in crate::services::discord) fn completion_footer_edit_still_registered(
    channel_id: ChannelId,
    edit: &CompletionFooterEdit,
) -> bool {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&channel_id.get())
        .is_some_and(|target| target.message_id == edit.message_id && target.owner == edit.owner)
}

fn completion_footer_idle_animation_expired(registered_at_unix: i64, now_unix: i64) -> bool {
    now_unix.saturating_sub(registered_at_unix) >= smp::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS
}

#[cfg(test)]
pub(in crate::services::discord) fn completion_footer_record_edit_result(
    channel_id: ChannelId,
    remove_after_edit: bool,
    edited: bool,
) {
    completion_footer_record_edit_result_with_block(
        channel_id,
        remove_after_edit,
        edited,
        None,
        None,
        None,
    );
}

pub(in crate::services::discord) fn completion_footer_record_committed_text_result_for_owner(
    channel_id: ChannelId,
    message_id: MessageId,
    owner: CompletionFooterOwner,
    remove_after_edit: bool,
    edited: bool,
    committed_text: &str,
    completion_block: Option<&str>,
) -> bool {
    completion_footer_record_edit_result_with_block(
        channel_id,
        remove_after_edit,
        edited,
        completion_block,
        Some(committed_text),
        Some((message_id, owner)),
    )
}

pub(in crate::services::discord) fn completion_footer_record_edit_result_for_edit(
    shared: &SharedData,
    channel_id: ChannelId,
    edit: &CompletionFooterEdit,
    edited: bool,
) -> bool {
    let recorded = completion_footer_record_edit_result_with_block(
        channel_id,
        edit.remove_after_edit,
        edited,
        edit.completion_block.as_deref(),
        Some(&edit.text),
        Some((edit.message_id, edit.owner)),
    );
    // #3391: this edit delivered the terminal marks once; evict those slot
    // identities so the next render (and any #3386 migration footer) drops the
    // completed task AND subagent entries.
    if edited && recorded {
        shared
            .ui
            .placeholder_live_events
            .evict_delivered_terminal_footer_tasks(channel_id, &edit.delivered_terminal_ids);
    }
    recorded
}

fn completion_footer_record_edit_result_with_block(
    channel_id: ChannelId,
    remove_after_edit: bool,
    edited: bool,
    completion_block: Option<&str>,
    committed_text: Option<&str>,
    guard_identity: Option<(MessageId, CompletionFooterOwner)>,
) -> bool {
    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(target) = guard.get_mut(&channel_id.get()) else {
        return false;
    };
    if let Some((message_id, owner)) = guard_identity {
        if target.message_id != message_id || target.owner != owner {
            tracing::warn!(
                target: "agentdesk.discord.single_message_panel",
                event = "completion_footer_edit_result_skipped",
                channel_id = channel_id.get(),
                edit_message_id = message_id.get(),
                registered_message_id = target.message_id.get(),
                edit_owner_user_msg_id = owner.user_msg_id,
                edit_owner_started_at_unix = owner.started_at_unix,
                registered_owner_user_msg_id = target.owner.user_msg_id,
                registered_owner_started_at_unix = target.owner.started_at_unix,
                provider = ?target.provider,
                reason = "owner_or_message_mismatch",
                "completion footer edit result ignored because registry target changed"
            );
            return false;
        }
    }

    if remove_after_edit && edited {
        guard.remove(&channel_id.get());
        return true;
    }
    if edited {
        target.consecutive_edit_failures = 0;
        if let Some(block) = completion_block {
            target.last_completion_block = Some(block.to_string());
        }
        if let Some(text) = committed_text {
            target.last_committed_text = Some(text.to_string());
        }
        return true;
    }
    target.consecutive_edit_failures = target.consecutive_edit_failures.saturating_add(1);
    if target.consecutive_edit_failures >= smp::COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES {
        guard.remove(&channel_id.get());
    }
    true
}

fn supersede_edit_from_registered_target(
    target: RegisteredCompletionFooter,
) -> CompletionFooterEdit {
    let completion_block = target
        .last_completion_block
        .as_deref()
        .map(freeze_completion_footer_block);
    let text = smp::compose_completion_footer_text(&target.base_body, completion_block.as_deref());
    CompletionFooterEdit {
        message_id: target.message_id,
        text,
        remove_after_edit: true,
        owner: target.owner,
        completion_block,
        // #3391 migration-race rule: a frozen supersede snapshot never counts
        // as "the once" — it carries only the last delivered block string with
        // no slot identity. Terminal marks in it were either already evicted by
        // a confirmed live delivery, or (failed-edit race) render once more on
        // the new target and evict on that delivery; a ✓ is never lost.
        delivered_terminal_ids: Vec::new(),
    }
}

fn freeze_completion_footer_block(block: &str) -> String {
    smp::SINGLE_MESSAGE_PANEL_SPINNER_FRAMES
        .iter()
        .fold(block.to_string(), |acc, frame| {
            acc.replace(frame, smp::COMPLETION_FOOTER_IDLE_EXPIRED_INDICATOR)
        })
}

#[cfg(test)]
pub(in crate::services::discord) fn completion_footer_forget_registered_target(
    channel_id: ChannelId,
) {
    completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&channel_id.get());
}

pub(in crate::services::discord) fn completion_footer_forget_registered_target_if_message(
    channel_id: ChannelId,
    message_id: MessageId,
) -> bool {
    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard
        .get(&channel_id.get())
        .is_some_and(|target| target.message_id == message_id)
    {
        return false;
    }
    guard.remove(&channel_id.get());
    true
}

fn completion_footer_forget_registered_target_if_identity(
    channel_id: ChannelId,
    message_id: MessageId,
    owner: CompletionFooterOwner,
) -> bool {
    let mut guard = completion_footer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard
        .get(&channel_id.get())
        .is_some_and(|target| target.message_id == message_id && target.owner == owner)
    {
        return false;
    }
    guard.remove(&channel_id.get());
    true
}
