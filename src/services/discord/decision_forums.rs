//! Resolves a category's Decision forum under one PG lock per (guild, category); a create
//! whose result is unknown is never re-sent, and the forum is adopted once it is visible.

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::config::decisions::DecisionsConfig;
use crate::db::decision_forums::{self as store, ForumState};
use crate::db::postgres::PooledAdvisoryLock;

const LOCK_NAMESPACE: &str = "decision_forum";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedForum {
    pub id: u64,
    pub name: String,
    pub created: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ForumError {
    #[error("forum_override_invalid: configured forum {0} is not a forum in this category")]
    OverrideInvalid(String),
    #[error("forum_name_mismatch: this category's Decision forum is named {0:?}")]
    NameMismatch(String),
    #[error("forum_ambiguous: {0} forums named {1:?} in this category")]
    Ambiguous(usize, String),
    #[error(
        "forum_inventory_unavailable: the announce bot lacks Administrator, so an empty channel list does not prove the forum is absent"
    )]
    InventoryUnavailable,
    #[error("forum_create_rejected: {0}")]
    CreateRejected(String),
    #[error(
        "forum_create_unknown: an earlier create may still apply; it is not sent again until the forum shows up"
    )]
    CreateUnknown,
    #[error("discord: {0}")]
    Discord(#[from] serenity::Error),
    #[error("storage: {0}")]
    Storage(#[from] sqlx::Error),
}

pub(crate) async fn resolve_or_create(
    pool: &PgPool,
    http: &serenity::Http,
    config: &DecisionsConfig,
    guild: serenity::GuildId,
    category: serenity::ChannelId,
    name: &str,
    topic: Option<&str>,
) -> Result<ResolvedForum, ForumError> {
    if let Some(raw) = config.forums_by_category.get(&category.to_string()) {
        let invalid = || ForumError::OverrideInvalid(raw.clone());
        let id = raw.parse().map_err(|_| invalid())?;
        let forum = forum_in(http, guild, category, id)
            .await?
            .ok_or_else(invalid)?;
        return Ok(resolved(&forum, false));
    }
    let key = format!("{guild}:{category}");
    let lock = PooledAdvisoryLock::acquire(pool, LOCK_NAMESPACE, &key).await?;
    let result = resolve_locked(pool, http, guild, category, name, topic).await;
    if let Err(error) = lock.unlock().await {
        tracing::warn!(%error, "decision forum lock release failed; the session end frees it");
    }
    result
}

async fn resolve_locked(
    pool: &PgPool,
    http: &serenity::Http,
    guild: serenity::GuildId,
    category: serenity::ChannelId,
    name: &str,
    topic: Option<&str>,
) -> Result<ResolvedForum, ForumError> {
    let row = store::load(pool, guild.get(), category.get()).await?;
    let mut unknown = false;
    if let Some(row) = row {
        let same_name = row.forum_name == name;
        match row.state {
            ForumState::Ready(id) => {
                // A cache hit is re-checked: a deleted, moved or unreadable forum is not reused.
                let cached = match id.parse() {
                    Ok(id) => forum_in(http, guild, category, id).await?,
                    Err(_) => None,
                };
                match cached {
                    Some(forum) if same_name => return Ok(resolved(&forum, false)),
                    Some(_) => return Err(ForumError::NameMismatch(row.forum_name)),
                    None => {}
                }
            }
            ForumState::CreateMayHaveSent if !same_name => {
                return Err(ForumError::NameMismatch(row.forum_name));
            }
            ForumState::CreateMayHaveSent => unknown = true,
            ForumState::Failed(_) => {}
        }
    }

    let found: Vec<_> = http
        .get_channels(guild)
        .await?
        .into_iter()
        .filter(|channel| {
            channel.kind == serenity::ChannelType::Forum
                && channel.parent_id == Some(category)
                && channel.name == name
        })
        .collect();
    match found.as_slice() {
        [forum] => {
            let state = ForumState::Ready(forum.id.to_string());
            store::store(pool, guild.get(), category.get(), name, &state).await?;
            return Ok(resolved(forum, false));
        }
        [] => {}
        many => return Err(ForumError::Ambiguous(many.len(), name.to_string())),
    }
    if unknown {
        return Err(ForumError::CreateUnknown);
    }
    if !sees_every_channel(http, guild).await? {
        return Err(ForumError::InventoryUnavailable);
    }

    let journal = ForumState::CreateMayHaveSent;
    store::store(pool, guild.get(), category.get(), name, &journal).await?;
    let mut builder = serenity::builder::CreateChannel::new(name)
        .kind(serenity::ChannelType::Forum)
        .category(category);
    if let Some(topic) = topic {
        builder = builder.topic(topic);
    }
    match guild.create_channel(http, builder).await {
        Ok(forum) => {
            let state = ForumState::Ready(forum.id.to_string());
            store::store(pool, guild.get(), category.get(), name, &state).await?;
            Ok(resolved(&forum, true))
        }
        Err(error) if matches!(status(&error), Some(400 | 401 | 403)) => {
            let reason = error.to_string();
            let state = ForumState::Failed(reason.clone());
            store::store(pool, guild.get(), category.get(), name, &state).await?;
            Err(ForumError::CreateRejected(reason))
        }
        Err(error) => {
            tracing::warn!(%error, "decision forum create result unknown; it will not be re-sent");
            Err(ForumError::CreateUnknown)
        }
    }
}

/// The channel as a forum of this guild and category; `None` when it is gone or elsewhere.
async fn forum_in(
    http: &serenity::Http,
    guild: serenity::GuildId,
    category: serenity::ChannelId,
    id: serenity::ChannelId,
) -> Result<Option<serenity::GuildChannel>, ForumError> {
    match http.get_channel(id).await {
        Ok(serenity::Channel::Guild(channel))
            if channel.guild_id == guild
                && channel.parent_id == Some(category)
                && channel.kind == serenity::ChannelType::Forum =>
        {
            Ok(Some(channel))
        }
        Ok(_) => Ok(None),
        Err(error) if status(&error) == Some(404) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Discord leaves channels the bot cannot view out of the list; only Administrator sees all.
async fn sees_every_channel(
    http: &serenity::Http,
    guild: serenity::GuildId,
) -> Result<bool, serenity::Error> {
    let me = http.get_current_user().await?.id;
    let member = http.get_member(guild, me).await?;
    let everyone = serenity::RoleId::new(guild.get());
    Ok(http.get_guild_roles(guild).await?.iter().any(|role| {
        (role.id == everyone || member.roles.contains(&role.id)) && role.permissions.administrator()
    }))
}

fn status(error: &serenity::Error) -> Option<u16> {
    match error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) => {
            Some(response.status_code.as_u16())
        }
        _ => None,
    }
}

fn resolved(forum: &serenity::GuildChannel, created: bool) -> ResolvedForum {
    ResolvedForum {
        id: forum.id.get(),
        name: forum.name.clone(),
        created,
    }
}

#[cfg(test)]
#[path = "decision_forums_tests.rs"]
mod tests;
