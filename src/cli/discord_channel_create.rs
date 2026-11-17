//! `discord channel-create --kind forum`: the category's Decision forum via the shared resolver.

use poise::serenity_prelude as serenity;
use serde_json::{Value, json};

use crate::server::routes::AppState;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum ChannelKind {
    #[default]
    Text,
    Forum,
}

pub(crate) async fn create_forum(
    state: &AppState,
    http: &serenity::Http,
    guild: serenity::GuildId,
    name: &str,
    category_id: Option<&str>,
    topic: Option<&str>,
) -> Result<Value, String> {
    let raw = category_id.ok_or("--kind forum needs --category-id")?;
    let category = raw
        .parse()
        .map(serenity::ChannelId::new)
        .map_err(|err| format!("invalid category id {raw:?}: {err}"))?;
    let pool =
        (state.pg_pool.as_ref()).ok_or("--kind forum needs PostgreSQL for its create journal")?;
    let config = &state.config.discord.decisions;
    let forum = crate::services::discord::decision_forums::resolve_or_create(
        pool, http, config, guild, category, name, topic,
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(json!({
        "id": forum.id.to_string(),
        "name": forum.name,
        "kind": "forum",
        "category_id": category.get().to_string(),
        "created": forum.created,
    }))
}
