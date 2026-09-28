//! Gateway and relay side of the output shadow: copies bot messages and live bindings into it.
//! Kept outside `shadow` because it holds serenity and relay-state handles the census forbids there.

use chrono::Utc;
use poise::serenity_prelude as serenity;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::shadow::LegacyTapEvent;
use crate::services::tui_o::shadow::binding_reader::ShadowTarget;
use crate::services::tui_o::shadow::tap::{self, TuiOConfig};

/// Called by the gateway event handler before dispatch; a no-op while the shadow is off.
pub fn observe(ctx: &serenity::Context, event: &serenity::FullEvent) {
    let Some(tap) = tap::installed() else { return };
    let channel_id = match event {
        serenity::FullEvent::Message { new_message } => new_message.channel_id,
        serenity::FullEvent::MessageUpdate { event, .. } => event.channel_id,
        serenity::FullEvent::MessageDelete { channel_id, .. } => *channel_id,
        _ => return,
    };
    if tap.watches(channel_id.get()) {
        if let Some(copy) = tap_event(ctx.cache.current_user().id.get(), event) {
            tap.offer(copy);
        }
    }
}

/// Own-bot messages only; authorless updates and deletes pass because the diff ignores unseen ids.
fn tap_event(bot_id: u64, event: &serenity::FullEvent) -> Option<LegacyTapEvent> {
    match event {
        serenity::FullEvent::Message { new_message: m } if m.author.id.get() == bot_id => {
            Some(LegacyTapEvent::Created {
                channel_id: m.channel_id.get(),
                msg_id: m.id.get(),
                at: *m.timestamp,
                content: m.content.clone(),
            })
        }
        serenity::FullEvent::MessageUpdate { event: e, .. }
            if e.author.as_ref().is_none_or(|a| a.id.get() == bot_id) =>
        {
            Some(LegacyTapEvent::Updated {
                channel_id: e.channel_id.get(),
                msg_id: e.id.get(),
                at: e.edited_timestamp.map_or_else(Utc::now, |t| *t),
                content: e.content.clone(),
            })
        }
        serenity::FullEvent::MessageDelete {
            channel_id,
            deleted_message_id,
            ..
        } => Some(LegacyTapEvent::Deleted {
            channel_id: channel_id.get(),
            msg_id: deleted_message_id.get(),
            at: Utc::now(),
        }),
        _ => None,
    }
}

/// Starts the shadow once per process when `tui_o.shadow.enabled`; it never blocks intake.
pub fn spawn_if_enabled(config: Option<&TuiOConfig>) {
    let Some(config) = config.map(|c| &c.shadow).filter(|c| c.enabled) else {
        return;
    };
    let Some(runtime_root) = crate::config::runtime_root() else {
        return tracing::warn!("o-shadow: runtime root unresolved; not started");
    };
    tap::start(config, &runtime_root, discover_targets);
}

/// Allowlisted channels with a live TUI binding, read without purging or waiting on relay state.
pub(crate) fn discover_targets(allowlist: &[u64]) -> Vec<ShadowTarget> {
    if allowlist.is_empty() {
        return Vec::new();
    }
    use crate::services::tui_prompt_dedupe::peek_tui_session_channels;
    let kinds = [RuntimeHandoffKind::ClaudeTui, RuntimeHandoffKind::CodexTui];
    let sessions = peek_tui_session_channels(&kinds).unwrap_or_default();
    let allowed = sessions
        .into_iter()
        .filter(|(_, channel_id)| allowlist.contains(channel_id));
    let target = |(tmux_session, channel_id)| ShadowTarget {
        channel_id,
        tmux_session,
    };
    allowed.map(target).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_copies_own_bot_messages_and_authorless_edits_only() {
        let mut own = serenity::Message::default();
        own.author.id = serenity::UserId::new(1);
        own.channel_id = serenity::ChannelId::new(7);
        own.id = serenity::MessageId::new(9);
        let mut other = own.clone();
        other.author.id = serenity::UserId::new(2);
        let message = |new_message| serenity::FullEvent::Message { new_message };
        assert!(matches!(
            tap_event(1, &message(own)),
            Some(LegacyTapEvent::Created { msg_id: 9, .. })
        ));
        assert!(tap_event(1, &message(other)).is_none());
        let update = |author: serde_json::Value| serenity::FullEvent::MessageUpdate {
            old_if_available: None,
            new: None,
            event: serde_json::from_value(serde_json::json!({
                "id": "9", "channel_id": "7", "content": "x", "author": author,
            }))
            .unwrap(),
        };
        let user = |id: &str| serde_json::json!({"id": id, "username": "u", "discriminator": "0"});
        assert!(tap_event(1, &update(serde_json::Value::Null)).is_some());
        assert!(tap_event(1, &update(user("2"))).is_none());
    }
}
