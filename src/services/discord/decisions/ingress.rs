//! Messages under a Decision forum (ready, configured or registered before its first post) never
//! reach agent intake; a parent the last read missed is looked up in PG, so none is missed.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::config::decisions::DecisionsConfig;

/// How long one full read of the forum set is reused before it is read again.
const REFRESH: Duration = Duration::from_secs(30);

/// The forum set this process last read and when, and the forums registered or found since; a
/// later read replaces the first but never the second.
#[derive(Default)]
pub(crate) struct Forums(Mutex<Option<(Instant, HashSet<u64>)>>, Mutex<BTreeSet<u64>>);

static FORUMS: Forums = Forums(Mutex::new(None), Mutex::new(BTreeSet::new()));

/// Guards `forum` in this process from now on, before anything is posted to it.
pub(in crate::services::discord) fn register(forum: u64) {
    FORUMS.register(forum);
}

/// Whether PG records `forum` as a ready Decision forum.
async fn ready_in_pg(pool: &PgPool, forum: u64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM decision_forums WHERE forum_id = $1 AND state = 'ready')",
    )
    .bind(forum.to_string())
    .fetch_one(pool)
    .await
}

/// Every Decision forum a guard may know: the ready rows and the configured overrides. A forum
/// whose create is still unknown has no id yet, so nothing is matched for it.
pub(crate) async fn load_forums(
    pool: Option<&PgPool>,
    config: &DecisionsConfig,
) -> Result<HashSet<u64>, sqlx::Error> {
    let parse = |id: &String| id.parse::<u64>().ok();
    let mut forums: HashSet<u64> = (config.forums_by_category.values())
        .filter_map(parse)
        .collect();
    if let Some(pool) = pool {
        let ready: Vec<String> = sqlx::query_scalar(
            "SELECT forum_id FROM decision_forums WHERE state = 'ready' AND forum_id IS NOT NULL",
        )
        .fetch_all(pool)
        .await?;
        forums.extend(ready.iter().filter_map(parse));
    }
    Ok(forums)
}

impl Forums {
    pub(crate) fn register(&self, forum: u64) {
        (self.1.lock().unwrap_or_else(PoisonError::into_inner)).insert(forum);
    }

    /// Whether `parent` is a Decision forum: configured or registered always, else the last read,
    /// which on a miss asks PG for that forum and on failure keeps the last known set.
    pub(crate) async fn contains(
        &self,
        pool: Option<&PgPool>,
        config: &DecisionsConfig,
        parent: u64,
    ) -> bool {
        let configured = |id: &String| id.parse::<u64>().ok() == Some(parent);
        let registered =
            || (self.1.lock().unwrap_or_else(PoisonError::into_inner)).contains(&parent);
        if config.forums_by_category.values().any(configured) || registered() {
            return true;
        }
        let known = |fresh_only: bool| {
            let read = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            (read.as_ref())
                .filter(|(at, _)| !fresh_only || at.elapsed() < REFRESH)
                .map(|(_, forums)| forums.contains(&parent))
        };
        match (known(true), pool) {
            (Some(true), _) => return true,
            (Some(false), Some(pool)) => {
                return match ready_in_pg(pool, parent).await {
                    Ok(ready) => {
                        if ready {
                            self.register(parent);
                        }
                        ready
                    }
                    Err(error) => {
                        tracing::warn!(%error, "decision forum lookup failed; the last read stands");
                        false
                    }
                };
            }
            (Some(false), None) => return false,
            (None, _) => {}
        }
        match load_forums(pool, config).await {
            Ok(forums) => {
                let answer = forums.contains(&parent);
                *self.0.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some((Instant::now(), forums));
                answer
            }
            Err(error) => {
                tracing::warn!(%error, "decision forum set unread; the last known set stays");
                known(false).unwrap_or(false)
            }
        }
    }
}

/// Whether the gateway cache knows `parent` as a channel that is no forum; a Decision forum is a
/// forum, so a thread under any other kind needs no lookup.
fn surely_not_forum(
    ctx: &serenity::Context,
    message: &serenity::Message,
    parent: serenity::ChannelId,
) -> bool {
    let Some(guild) = message.guild_id.and_then(|guild| ctx.cache.guild(guild)) else {
        return false;
    };
    (guild.channels.get(&parent))
        .is_some_and(|channel| channel.kind != serenity::ChannelType::Forum)
}

/// The thread's parent as the gateway cache holds it, for when the caller's own read failed.
fn cached_parent(
    ctx: &serenity::Context,
    message: &serenity::Message,
) -> Option<serenity::ChannelId> {
    let guild = ctx.cache.guild(message.guild_id?)?;
    let thread = (guild.threads.iter()).find(|thread| thread.id == message.channel_id)?;
    thread.parent_id
}

/// Whether the message sits under a Decision forum, so agent intake must not take it. `parent`
/// is the thread parent the caller already resolved. Nothing answers such a message yet.
pub(in crate::services::discord) async fn consumes(
    ctx: &serenity::Context,
    pool: Option<&PgPool>,
    message: &serenity::Message,
    parent: Option<serenity::ChannelId>,
) -> bool {
    let Some(parent) = parent.or_else(|| cached_parent(ctx, message)) else {
        return false;
    };
    if surely_not_forum(ctx, message, parent) {
        return false;
    }
    let config = crate::config::snapshot()
        .map(|config| config.discord.decisions.clone())
        .unwrap_or_default();
    if !FORUMS.contains(pool, &config, parent.get()).await {
        return false;
    }
    tracing::info!(
        channel_id = message.channel_id.get(),
        message_id = message.id.get(),
        forum_id = parent.get(),
        "message under a Decision forum kept out of agent intake"
    );
    true
}

#[cfg(test)]
#[path = "ingress_tests.rs"]
mod tests;
