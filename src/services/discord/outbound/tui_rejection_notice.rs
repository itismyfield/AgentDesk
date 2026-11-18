//! Body-free notice delivery; a committed uncertain attempt may only reconcile history.
use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use sqlx::PgPool;

use crate::db::tui_output_disposition::{self as db, Identity, Observation, Reason, Receipt};

pub(crate) enum PostResult {
    Created(Observation),
    NoEffect,
    Uncertain,
}
pub(crate) trait NoticePort {
    fn bot_id(&self) -> u64;
    fn post(
        &self,
        channel: u64,
        content: &str,
        nonce: &str,
    ) -> impl Future<Output = PostResult> + Send;
    #[cfg(test)]
    fn lookup(
        &self,
        channel: u64,
        message: u64,
    ) -> impl Future<Output = Result<Option<Observation>, String>> + Send;
}
pub(crate) struct GatewayNoticePort {
    http: Arc<serenity::Http>,
    bot: u64,
}
impl GatewayNoticePort {
    pub(crate) fn new(http: Arc<serenity::Http>, bot: u64) -> Self {
        Self { http, bot }
    }
}
fn observation(message: serenity::Message) -> Observation {
    Observation {
        message: message.id.get(),
        channel: message.channel_id.get(),
        author: message.author.id.get(),
        content: message.content,
        nonce: message.nonce.map(|n| match n {
            serenity::Nonce::String(value) => value,
            serenity::Nonce::Number(value) => value.to_string(),
        }),
    }
}
impl NoticePort for GatewayNoticePort {
    fn bot_id(&self) -> u64 {
        self.bot
    }
    async fn post(&self, channel: u64, content: &str, nonce: &str) -> PostResult {
        // Installation must supply a notice-only authority context; never inherit a known body route.
        let mut tag = super::current_census().unwrap_or_else(|| serde_json::json!({}));
        if tag
            .get("output_class")
            .is_some_and(|class| class != "NonBody")
        {
            return PostResult::NoEffect;
        }
        tag["output_class"] = serde_json::json!("NonBody");
        match super::census_scope(
            Some(tag),
            super::super::http::send_channel_message_with_nonce(
                &self.http,
                serenity::ChannelId::new(channel),
                content,
                nonce,
            ),
        )
        .await
        {
            Ok(message) => PostResult::Created(observation(message)),
            Err(serenity::Error::Http(error))
                if error
                    .status_code()
                    .is_some_and(|s| matches!(s.as_u16(), 400 | 403 | 404)) =>
            {
                PostResult::NoEffect
            }
            Err(_) => PostResult::Uncertain,
        }
    }
    #[cfg(test)]
    async fn lookup(&self, channel: u64, message: u64) -> Result<Option<Observation>, String> {
        self.http
            .get_message(
                serenity::ChannelId::new(channel),
                serenity::MessageId::new(message),
            )
            .await
            .map(|m| Some(observation(m)))
            .map_err(|e| e.to_string())
    }
}

pub(crate) async fn deliver(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
    port: &impl NoticePort,
) -> Result<Option<Receipt>, String> {
    let Some(attempt) = db::reserve(pool, id, reason).await? else {
        return db::confirmed(pool, id, reason).await;
    };
    let post = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        port.post(id.channel, reason.message(), &attempt.nonce()),
    )
    .await;
    match post {
        Ok(PostResult::Created(seen)) => Ok(Some(
            attempt.verify(port.bot_id(), seen)?.commit(pool).await?,
        )),
        Ok(PostResult::NoEffect) => {
            attempt.no_effect(pool).await?;
            Ok(None)
        }
        Ok(PostResult::Uncertain) | Err(_) => Ok(None),
    }
}

/// History reconciliation needs the posted message id; no production source supplies one yet.
#[cfg(test)]
pub(crate) async fn reconcile(
    pool: &PgPool,
    id: &Identity,
    reason: Reason,
    message: u64,
    port: &impl NoticePort,
) -> Result<Option<Receipt>, String> {
    if message == 0 {
        return Err("notice history lookup needs a message id".into());
    }
    let Some(attempt) = db::pending(pool, id, reason).await? else {
        return Ok(None);
    };
    let seen = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        port.lookup(id.channel, message),
    )
    .await
    .map_err(|_| "notice history lookup timed out")??;
    let Some(seen) = seen else { return Ok(None) };
    if seen.message != message {
        return Err("notice history returned another message".into());
    }
    Ok(Some(
        attempt.verify(port.bot_id(), seen)?.commit(pool).await?,
    ))
}

#[cfg(test)]
#[path = "tui_rejection_notice_tests.rs"]
pub(crate) mod tests;
