//! The mention policy every relayed Discord message carries; `discord::http` documents why.

use poise::serenity_prelude::CreateAllowedMentions;

/// User pings stay on; `@everyone`, `@here` and role mentions never ping.
pub(crate) fn relay_allowed_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(true)
        .everyone(false)
        .all_roles(false)
}

pub(crate) fn relay_message_payload(content: &str) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "allowed_mentions": relay_allowed_mentions(),
    })
}
