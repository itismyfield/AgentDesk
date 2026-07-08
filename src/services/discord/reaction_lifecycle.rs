use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::SharedData;

const MIN_REAL_DISCORD_MESSAGE_ID: u64 = 100_000_000_000_000;

pub(in crate::services::discord) fn is_real_discord_message_id_value(id: u64) -> bool {
    (MIN_REAL_DISCORD_MESSAGE_ID..super::voice_barge_in::INTERNAL_VOICE_MESSAGE_ID_START)
        .contains(&id)
}

pub(in crate::services::discord) fn is_real_discord_message_id(
    message_id: serenity::MessageId,
) -> bool {
    is_real_discord_message_id_value(message_id.get())
}

pub(in crate::services::discord) fn reaction_target_channel_from_thread_parents<I>(
    channel_id: ChannelId,
    thread_parents: I,
) -> ChannelId
where
    I: IntoIterator<Item = (ChannelId, ChannelId)>,
{
    thread_parents
        .into_iter()
        .find_map(|(parent, thread)| (thread == channel_id).then_some(parent))
        .unwrap_or(channel_id)
}

pub(in crate::services::discord) fn reaction_target_channel_for_shared(
    shared: &SharedData,
    channel_id: ChannelId,
) -> ChannelId {
    reaction_target_channel_from_thread_parents(
        channel_id,
        shared
            .dispatch
            .thread_parents
            .iter()
            .map(|entry| (*entry.key(), *entry.value())),
    )
}

#[cfg(not(test))]
#[derive(Clone, Copy)]
enum ReactionAction {
    Add,
    Remove,
}

fn parent_retry_failure_status(
    _initial_status: Option<u16>,
    final_status: Option<u16>,
) -> Option<u16> {
    final_status
}

#[cfg(test)]
pub(in crate::services::discord) fn test_parent_retry_failure_status(
    initial_status: Option<u16>,
    final_status: Option<u16>,
) -> Option<u16> {
    parent_retry_failure_status(initial_status, final_status)
}

#[cfg(not(test))]
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct ReactionLifecycleError {
    message: String,
    status: Option<u16>,
}

#[cfg(not(test))]
impl ReactionLifecycleError {
    pub(in crate::services::discord) fn status(&self) -> Option<u16> {
        self.status
    }

    fn new(message: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }

    fn from_serenity(error: serenity::Error) -> Self {
        let status = match &error {
            serenity::Error::Http(http_error) => {
                http_error.status_code().map(|status| status.as_u16())
            }
            _ => None,
        };
        Self::new(error.to_string(), status)
    }

    fn with_parent_retry_error(self, parent_channel_id: ChannelId, second: Self) -> Self {
        let status = parent_retry_failure_status(self.status, second.status);
        Self::new(
            format!(
                "{}; parent-channel retry in {} failed: {}",
                self, parent_channel_id, second
            ),
            status,
        )
    }

    fn with_dispatch_parent_retry_error(self, target_channel_id: ChannelId, second: Self) -> Self {
        let status = parent_retry_failure_status(self.status, second.status);
        Self::new(
            format!(
                "{}; dispatch-parent retry in {} failed: {}",
                self, target_channel_id, second
            ),
            status,
        )
    }
}

#[cfg(not(test))]
impl std::fmt::Display for ReactionLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(not(test))]
impl ReactionAction {
    fn label(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

#[cfg(not(test))]
async fn apply_reaction_action_detailed(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
    action: ReactionAction,
) -> Result<(), ReactionLifecycleError> {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    match action {
        ReactionAction::Add => channel_id
            .create_reaction(http, message_id, reaction)
            .await
            .map_err(ReactionLifecycleError::from_serenity),
        ReactionAction::Remove => channel_id
            .delete_reaction(http, message_id, None, reaction)
            .await
            .map_err(ReactionLifecycleError::from_serenity),
    }
}

#[cfg(not(test))]
async fn thread_parent_channel_from_http(
    http: &serenity::Http,
    channel_id: ChannelId,
) -> Option<ChannelId> {
    let channel = channel_id.to_channel(http).await.ok()?.guild()?;
    let is_thread = matches!(
        channel.kind,
        serenity::ChannelType::NewsThread
            | serenity::ChannelType::PublicThread
            | serenity::ChannelType::PrivateThread
    );
    is_thread.then_some(channel.parent_id).flatten()
}

#[cfg(not(test))]
async fn try_reaction_raw_once_on_channel_detailed(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
    action: ReactionAction,
) -> Result<(), ReactionLifecycleError> {
    if !is_real_discord_message_id(message_id) {
        tracing::debug!(
            channel_id = channel_id.get(),
            message = message_id.get(),
            emoji = %emoji,
            action = action.label(),
            "discord reaction skipped for non-Discord/synthetic message id"
        );
        return Ok(());
    }

    apply_reaction_action_detailed(http, channel_id, message_id, emoji, action).await
}

#[cfg(not(test))]
async fn try_reaction_raw_on_channel_detailed(
    http: &serenity::Http,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
    action: ReactionAction,
) -> Result<(), ReactionLifecycleError> {
    match try_reaction_raw_once_on_channel_detailed(http, channel_id, message_id, emoji, action)
        .await
    {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if let Some(parent_channel_id) = thread_parent_channel_from_http(http, channel_id)
                .await
                .filter(|parent| *parent != channel_id)
            {
                tracing::debug!(
                    channel_id = channel_id.get(),
                    parent_channel = parent_channel_id.get(),
                    message = message_id.get(),
                    emoji = %emoji,
                    action = action.label(),
                    error = %first_error,
                    "discord reaction retrying against thread parent channel"
                );
                try_reaction_raw_once_on_channel_detailed(
                    http,
                    parent_channel_id,
                    message_id,
                    emoji,
                    action,
                )
                .await
                .map_err(|second_error| {
                    first_error.with_parent_retry_error(parent_channel_id, second_error)
                })
            } else {
                Err(first_error)
            }
        }
    }
}

#[cfg(not(test))]
async fn try_reaction_raw_with_shared_detailed(
    http: &serenity::Http,
    shared: &SharedData,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
    action: ReactionAction,
) -> Result<(), ReactionLifecycleError> {
    let target_channel_id = reaction_target_channel_for_shared(shared, channel_id);
    if target_channel_id == channel_id {
        return try_reaction_raw_on_channel_detailed(http, channel_id, message_id, emoji, action)
            .await;
    }

    match try_reaction_raw_once_on_channel_detailed(http, channel_id, message_id, emoji, action)
        .await
    {
        Ok(()) => Ok(()),
        Err(first_error) => {
            tracing::debug!(
                channel_id = channel_id.get(),
                target_channel = target_channel_id.get(),
                message = message_id.get(),
                emoji = %emoji,
                action = action.label(),
                error = %first_error,
                "discord reaction retrying against dispatch thread parent channel"
            );
            try_reaction_raw_once_on_channel_detailed(
                http,
                target_channel_id,
                message_id,
                emoji,
                action,
            )
            .await
            .map_err(|second_error| {
                first_error.with_dispatch_parent_retry_error(target_channel_id, second_error)
            })
        }
    }
}

#[cfg(not(test))]
pub(in crate::services::discord) async fn try_add_reaction_raw_with_shared_detailed(
    http: &serenity::Http,
    shared: &SharedData,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
) -> Result<(), ReactionLifecycleError> {
    try_reaction_raw_with_shared_detailed(
        http,
        shared,
        channel_id,
        message_id,
        emoji,
        ReactionAction::Add,
    )
    .await
}

#[cfg(not(test))]
pub(in crate::services::discord) async fn try_remove_reaction_raw_with_shared_detailed(
    http: &serenity::Http,
    shared: &SharedData,
    channel_id: ChannelId,
    message_id: serenity::MessageId,
    emoji: char,
) -> Result<(), ReactionLifecycleError> {
    try_reaction_raw_with_shared_detailed(
        http,
        shared,
        channel_id,
        message_id,
        emoji,
        ReactionAction::Remove,
    )
    .await
}

#[cfg(test)]
mod tests {
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn dispatch_thread_reaction_targets_parent_channel() {
        let parent = ChannelId::new(123_450_000_000_000_001);
        let thread = ChannelId::new(123_450_000_000_000_002);
        let other_parent = ChannelId::new(123_450_000_000_000_003);
        let other_thread = ChannelId::new(123_450_000_000_000_004);

        let target = super::reaction_target_channel_from_thread_parents(
            thread,
            [(other_parent, other_thread), (parent, thread)],
        );

        assert_eq!(target, parent);
        assert_eq!(
            super::reaction_target_channel_from_thread_parents(parent, [(parent, thread)]),
            parent,
            "non-thread parent channels must stay unchanged"
        );
    }

    #[test]
    fn real_discord_message_id_guard_rejects_synthetic_ranges() {
        assert!(!super::is_real_discord_message_id_value(0));
        assert!(!super::is_real_discord_message_id(MessageId::new(99)));
        assert!(!super::is_real_discord_message_id(MessageId::new(
            99_999_999_999_999
        )));
        assert!(!super::is_real_discord_message_id(MessageId::new(
            super::super::voice_barge_in::INTERNAL_VOICE_MESSAGE_ID_START
        )));
        assert!(super::is_real_discord_message_id(MessageId::new(
            100_000_000_000_000
        )));
        assert!(super::is_real_discord_message_id(MessageId::new(
            940_000_000_000_108
        )));
    }
}
