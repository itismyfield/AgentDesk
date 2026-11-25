//! Posts a stored request with its channel's provider bot: forum guarded, journal, then one POST.
//! A lost answer is settled by finding that post, a refusal is kept; neither is posted again.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use uuid::Uuid;

use super::{Ask, card, ingress};
use crate::config::DiscordConfig;
use crate::db::decisions::publication::{self as store, Journal, Publication};
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::decision_forums::{self, ForumError};
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::manual_steer::action_handle::ActionHandle;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The post exists with its buttons disabled.
    Posted(u64),
    /// The POST may have gone out and its post is not found yet; it is not sent again.
    Pending,
    /// Discord refused the post or its forum for good.
    Refused(String),
    /// Nothing was sent: no live bot, no forum yet, or storage away.
    Unready(&'static str),
    /// The request's channel sits in no category, so it has no Decision forum.
    NoCategory,
}

/// A stored request and the channel and provider its ask resolved.
pub(crate) struct Asked<'a> {
    pub request_id: Uuid,
    pub ask: &'a Ask,
    pub logical_channel: u64,
    pub provider: &'a ProviderKind,
}

/// The card's length in UTF-16 units when Discord would refuse it as too long.
pub(crate) fn too_long(ask: &Ask, owner: Option<u64>) -> Option<usize> {
    let options = ask.stored_options().ok()?;
    card::render(ask, &options, owner, Uuid::nil()).err()
}

pub(crate) async fn publish(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    config: &DiscordConfig,
    asked: Asked<'_>,
) -> Outcome {
    let unready = |error: sqlx::Error| {
        tracing::warn!(%error, "decision publication state unread");
        Outcome::Unready("decision_store_unavailable")
    };
    let bot = provider_bot(registry, asked.logical_channel, asked.provider);
    match store::load(pool, asked.request_id).await {
        Ok(Publication::Disabled { thread_id }) => Outcome::Posted(thread_id),
        Ok(Publication::PostFailed(error)) => Outcome::Refused(error),
        Ok(Publication::Posting {
            forum_id,
            bot_user_id,
        }) => match bot.await {
            Some((http, _)) => {
                reconcile(pool, &http, forum_id, bot_user_id, asked.request_id).await
            }
            None => Outcome::Pending,
        },
        Ok(Publication::Stored) => match bot.await {
            Some(bot) => post(pool, registry, config, &asked, bot).await,
            None => Outcome::Unready("decision_bot_unavailable"),
        },
        Err(error) => unready(error),
    }
}

/// The provider bot that owns the channel with its gateway connected now, not merely once (a
/// handed-back gateway keeps its Context): its REST client and user id.
async fn provider_bot(
    registry: Option<&HealthRegistry>,
    channel: u64,
    provider: &ProviderKind,
) -> Option<(Arc<serenity::Http>, u64)> {
    let channel = serenity::ChannelId::new(channel);
    let owner = health::resolve_direct_channel_runtime(registry?, channel, provider).await;
    let (http, shared) = owner.ok()?;
    if !shared.bot_connected.load(Ordering::SeqCst) {
        return None;
    }
    let bot = shared
        .http
        .cached_serenity_ctx
        .get()?
        .cache
        .current_user()
        .id;
    Some((http, bot.get()))
}

async fn post(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    config: &DiscordConfig,
    asked: &Asked<'_>,
    (http, bot): (Arc<serenity::Http>, u64),
) -> Outcome {
    let (guild, category) = match category_of(&http, asked.logical_channel).await {
        Ok(Some(found)) => found,
        Ok(None) => return Outcome::NoCategory,
        Err(error) => {
            tracing::warn!(%error, "decision target category unread");
            return Outcome::Unready("target_category_unavailable");
        }
    };
    let announce = match registry {
        Some(registry) => {
            health::resolve_utility_bot_http(registry, UtilityBotRole::Announce).await
        }
        None => return Outcome::Unready("decision_bot_unavailable"),
    };
    let Ok(announce) = announce else {
        return Outcome::Unready("decision_forum_unavailable");
    };
    let decisions = &config.decisions;
    let name = decisions.forum_name();
    let forum =
        decision_forums::resolve_or_create(pool, &announce, decisions, guild, category, name, None);
    let forum = match forum.await {
        Ok(forum) => forum.id,
        Err(error) => return forum_outcome(error),
    };
    // The forum is guarded before anything can appear in it.
    ingress::register(forum);
    let options = match asked.ask.stored_options() {
        Ok(options) => options,
        Err(rule) => return Outcome::Refused(rule.into()),
    };
    let Ok(card) = card::render(asked.ask, &options, config.owner_id, asked.request_id) else {
        return Outcome::Refused("decision_post_too_long".into());
    };
    let custom_ids: Vec<String> = (options.iter())
        .map(|_| ActionHandle::generate().to_decision_custom_id())
        .collect();
    let journal = Journal {
        provider: asked.provider.as_str(),
        bot_user_id: bot,
        forum_id: forum,
        handles: &custom_ids,
    };
    match store::journal(pool, asked.request_id, &journal).await {
        Ok(true) => {}
        // Another run holds the POST; its post is found, never sent again from here.
        Ok(false) => return Outcome::Pending,
        Err(error) => {
            tracing::warn!(%error, "decision journal not written; nothing was sent");
            return Outcome::Unready("decision_store_unavailable");
        }
    }
    let body = card::post_body(&card, &options, &custom_ids, config.owner_id);
    match send_post(&http, forum, &body).await {
        Ok(thread) => {
            if let Err(error) = store::posted(pool, asked.request_id, thread).await {
                tracing::warn!(%error, thread, "decision post not recorded; it is found again");
            }
            Outcome::Posted(thread)
        }
        Err(error) => match http_status(&error) {
            Some(status @ (400 | 401 | 403 | 404)) => {
                let reason = if status == 403 {
                    "decision_post_forbidden"
                } else {
                    "decision_post_rejected"
                };
                tracing::warn!(%error, reason, "decision post refused; it is not sent again");
                if let Err(error) = store::refused(pool, asked.request_id, reason).await {
                    tracing::warn!(%error, "decision post refusal not recorded");
                }
                Outcome::Refused(reason.into())
            }
            _ => {
                tracing::warn!(%error, "decision post result unknown; it is found, not re-sent");
                Outcome::Pending
            }
        },
    }
}

/// Finds the post a lost answer left behind and records it; without it the request stays pending.
async fn reconcile(
    pool: &PgPool,
    http: &serenity::Http,
    forum: u64,
    bot: u64,
    request_id: Uuid,
) -> Outcome {
    match find_post(http, forum, bot, request_id).await {
        Ok(Some(thread)) => {
            if let Err(error) = store::posted(pool, request_id, thread).await {
                tracing::warn!(%error, thread, "found decision post not recorded");
            }
            Outcome::Posted(thread)
        }
        Ok(None) => Outcome::Pending,
        Err(error) => {
            tracing::warn!(%error, "decision post search failed; it stays pending");
            Outcome::Pending
        }
    }
}

/// The post of `request_id` among the forum's active threads: a thread our bot started whose
/// starter, written by our bot, ends with the request's marker line (a quoted marker is not one).
async fn find_post(
    http: &serenity::Http,
    forum: u64,
    bot: u64,
    request_id: Uuid,
) -> Result<Option<u64>, serenity::Error> {
    if forum == 0 || bot == 0 {
        return Ok(None);
    }
    let (forum, bot) = (serenity::ChannelId::new(forum), serenity::UserId::new(bot));
    let serenity::Channel::Guild(channel) = http.get_channel(forum).await? else {
        return Ok(None);
    };
    let active = http.get_guild_active_threads(channel.guild_id).await?;
    let marker = card::marker(request_id);
    let started = |thread: &&serenity::GuildChannel| {
        thread.parent_id == Some(forum) && thread.owner_id == Some(bot)
    };
    for thread in active.threads.iter().filter(started) {
        let starter_id = serenity::MessageId::new(thread.id.get());
        let Ok(starter) = http.get_message(thread.id, starter_id).await else {
            continue;
        };
        let last = starter.content.lines().last();
        if starter.author.id == bot && last == Some(marker.as_str()) {
            return Ok(Some(thread.id.get()));
        }
    }
    Ok(None)
}

/// The guild and category of the request's channel; `None` when it sits in no category.
async fn category_of(
    http: &serenity::Http,
    channel: u64,
) -> Result<Option<(serenity::GuildId, serenity::ChannelId)>, serenity::Error> {
    match http.get_channel(serenity::ChannelId::new(channel)).await? {
        serenity::Channel::Guild(channel) => {
            Ok((channel.parent_id).map(|category| (channel.guild_id, category)))
        }
        _ => Ok(None),
    }
}

/// One forum post; a forum post's thread and starter message share the returned id.
async fn send_post(
    http: &serenity::Http,
    forum: u64,
    body: &serde_json::Value,
) -> Result<u64, serenity::Error> {
    let route = serenity::http::Route::ChannelForumPosts {
        channel_id: serenity::ChannelId::new(forum),
    };
    let request = serenity::http::Request::new(route, serenity::http::LightMethod::Post)
        .body(Some(serde_json::to_vec(body)?));
    let thread: serenity::GuildChannel = http.request(request).await?.json().await?;
    Ok(thread.id.get())
}

fn http_status(error: &serenity::Error) -> Option<u16> {
    match error {
        serenity::Error::Http(http) => http.status_code().map(|status| status.as_u16()),
        _ => None,
    }
}

/// A forum that does not exist yet, or cannot be seen now, is not ready; any other forum error
/// is Discord's or the configuration's answer and is returned as it is.
fn forum_outcome(error: ForumError) -> Outcome {
    match error {
        ForumError::CreateUnknown
        | ForumError::InventoryUnavailable
        | ForumError::Discord(_)
        | ForumError::Storage(_) => {
            tracing::warn!(%error, "decision forum not ready; nothing was sent");
            Outcome::Unready("decision_forum_unavailable")
        }
        refused => {
            let text = refused.to_string();
            let code = text.split(':').next().unwrap_or("decision_forum_refused");
            Outcome::Refused(code.to_string())
        }
    }
}

#[cfg(all(test, unix))]
#[path = "publication_tests.rs"]
mod tests;
