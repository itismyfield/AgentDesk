//! Bounded create-message transport for re-posts: shares the gateway client's route buckets, sends
//! through a client that neither follows redirects nor retries, and repeats only a confirmed 429.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use reqwest::StatusCode;
use serenity::http::{LightMethod, Ratelimit, RatelimitInfo, Request, Route};
use serenity::{ChannelId, MessageId, UserId};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::services::tui_o::repost::send::{
    AttemptGuard, BoundedTransport, CreatedMessage, RepostEnvelope, WireOutcome,
};
use probe::ProbeRead;
use probe::matcher::ObservedMessage;

// The read-only probe, declared beside its reads so it stays dormant with them.
#[path = "../../tui_o/repost/probe.rs"]
pub(crate) mod probe;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unsupported {
    /// Without the gateway's limiter there are no route buckets to share.
    NoRatelimiter,
    /// Legacy ignores the proxy while a limiter is set, so the two would reach different hosts.
    Proxy,
    Client(String),
}

pub(crate) struct RepostHttp {
    http: Arc<serenity::Http>,
    client: reqwest::Client,
    base: Option<String>,
}

impl RepostHttp {
    pub(crate) fn new(http: Arc<serenity::Http>) -> Result<Self, Unsupported> {
        if http.proxy.is_some() {
            return Err(Unsupported::Proxy);
        }
        Self::build(http, None)
    }

    /// Sends to `base` in place of `https://discord.com`.
    #[cfg(test)]
    pub(crate) fn at(http: Arc<serenity::Http>, base: String) -> Result<Self, Unsupported> {
        Self::build(http, Some(base))
    }

    fn build(http: Arc<serenity::Http>, base: Option<String>) -> Result<Self, Unsupported> {
        if http.ratelimiter.is_none() {
            return Err(Unsupported::NoRatelimiter);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .use_rustls_tls()
            .build()
            .map_err(|error| Unsupported::Client(error.to_string()))?;
        Ok(Self { http, client, base })
    }
}

fn no_callback(_: RatelimitInfo) {}

fn seconds(response: &reqwest::Response, name: &str) -> Option<Result<Duration, ()>> {
    let value = response.headers().get(name)?.to_str().ok()?;
    let seconds: f64 = value.trim().parse().ok()?;
    Some(Duration::try_from_secs_f64(seconds).map_err(drop))
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    seconds(response, "retry-after")?.ok()
}

/// Serenity's bucket hook panics on a negative or overflowing seconds header, so such a response
/// never reaches it.
fn hookable(response: &reqwest::Response) -> bool {
    ["retry-after", "x-ratelimit-reset-after"]
        .iter()
        .all(|name| !matches!(seconds(response, name), Some(Err(()))))
}

#[derive(serde::Deserialize)]
struct Posted {
    id: MessageId,
    author: Author,
    #[serde(default)]
    content: String,
    #[serde(default)]
    embeds: Vec<Embed>,
}

#[derive(serde::Deserialize)]
struct Author {
    id: UserId,
}

#[derive(serde::Deserialize)]
struct Embed {
    footer: Option<Footer>,
}

#[derive(serde::Deserialize)]
struct Footer {
    text: String,
}

async fn settle(response: reqwest::Response) -> WireOutcome {
    let status = response.status();
    if status.is_success() {
        return match response.json::<Posted>().await {
            Ok(posted) => WireOutcome::Created(CreatedMessage {
                id: posted.id.get(),
                author_id: posted.author.id.get(),
                content: posted.content,
                footers: posted
                    .embeds
                    .into_iter()
                    .filter_map(|embed| Some(embed.footer?.text))
                    .collect(),
            }),
            Err(error) => WireOutcome::Uncertain(format!("unreadable success body: {error}")),
        };
    }
    match status.as_u16() {
        status @ (400 | 403 | 404) => WireOutcome::Refused(status),
        status => WireOutcome::Uncertain(format!("HTTP {status}")),
    }
}

/// After a confirmed 429, waits as Legacy would. `false` means no usable retry time: stop.
async fn wait_out(
    response: &reqwest::Response,
    bucket: &Mutex<Ratelimit>,
    request: &Request<'_>,
) -> bool {
    if response.headers().contains_key("x-ratelimit-global") {
        let Some(wait) = retry_after(response) else {
            return false;
        };
        tokio::time::sleep(wait).await;
        return true;
    }
    if !hookable(response) {
        return false;
    }
    // The bucket hook records the route limit and sleeps out `retry-after` before saying redo.
    let mut hook = bucket.lock().await;
    let redo = hook.post_hook(response, request, &no_callback, false).await;
    matches!(redo, Ok(true))
}

impl BoundedTransport for RepostHttp {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static {
        let (http, client, base) = (
            Arc::clone(&self.http),
            self.client.clone(),
            self.base.clone(),
        );
        let channel_id = ChannelId::new(envelope.channel);
        // Legacy's send_message fills in the client's default mentions the same way.
        let mut message = envelope.message();
        if let Some(mentions) = &self.http.default_allowed_mentions {
            message = message.allowed_mentions(mentions.clone());
        }
        let body = serde_json::to_vec(&message);
        async move {
            let body = match body {
                Ok(body) => body,
                Err(error) => return WireOutcome::Unsent(format!("unencodable message: {error}")),
            };
            let Some(limiter) = http.ratelimiter.as_ref() else {
                return WireOutcome::Unsent("no shared rate limiter".into());
            };
            let route = Route::ChannelMessages { channel_id };
            let request = Request::new(route, LightMethod::Post).body(Some(body));
            // Serenity's global 429 lock is private, so only route buckets are shared with Legacy.
            let bucket = Arc::clone(
                limiter
                    .routes()
                    .write()
                    .await
                    .entry(route.ratelimiting_bucket())
                    .or_default(),
            );
            loop {
                bucket.lock().await.pre_hook(&request, &no_callback).await;
                let built = match request
                    .clone()
                    .build(&client, http.token(), base.as_deref())
                {
                    Ok(builder) => builder.build().map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let built = match built {
                    Ok(built) => built,
                    Err(error) => {
                        return WireOutcome::Unsent(format!("unbuildable request: {error}"));
                    }
                };
                if let Err(reason) = guard.begin() {
                    return WireOutcome::Unsent(reason);
                }
                let response = match client.execute(built).await {
                    Ok(response) => response,
                    Err(error) => return WireOutcome::Uncertain(error.to_string()),
                };
                if response.status() != StatusCode::TOO_MANY_REQUESTS {
                    // Bookkeeping only: a malformed rate header never sends the request again.
                    if hookable(&response) {
                        let mut hook = bucket.lock().await;
                        let _ = hook
                            .post_hook(&response, &request, &no_callback, false)
                            .await;
                    }
                    return settle(response).await;
                }
                guard.throttled();
                if !wait_out(&response, &bucket, &request).await {
                    return WireOutcome::Throttled;
                }
            }
        }
    }
}

/// A message as history or a single read returns it.
#[derive(serde::Deserialize)]
struct Seen {
    id: MessageId,
    channel_id: ChannelId,
    author: Author,
    #[serde(default)]
    content: String,
    #[serde(default)]
    embeds: Vec<Embed>,
    #[serde(default)]
    nonce: Option<serde_json::Value>,
}

impl Seen {
    fn observed(self) -> ObservedMessage {
        ObservedMessage {
            id: self.id.get(),
            channel_id: self.channel_id.get(),
            author_id: self.author.id.get(),
            content: self.content,
            footers: self
                .embeds
                .into_iter()
                .filter_map(|embed| Some(embed.footer?.text))
                .collect(),
            // Discord sends a nonce as a string or a number; an absent one stays absent.
            nonce: self.nonce.and_then(|nonce| match nonce {
                serde_json::Value::String(text) => Some(text),
                serde_json::Value::Number(number) => Some(number.to_string()),
                _ => None,
            }),
        }
    }
}

impl RepostHttp {
    /// One GET through the shared route bucket. `Ok(None)` is a 404; a 429 is an error, unretried.
    async fn get(
        &self,
        route: Route<'_>,
        params: Vec<(&'static str, String)>,
    ) -> Result<Option<reqwest::Response>, String> {
        let limiter = self
            .http
            .ratelimiter
            .as_ref()
            .ok_or("no shared rate limiter")?;
        let params = (!params.is_empty()).then_some(params);
        let request = Request::new(route, LightMethod::Get).params(params);
        let bucket = Arc::clone(
            limiter
                .routes()
                .write()
                .await
                .entry(route.ratelimiting_bucket())
                .or_default(),
        );
        bucket.lock().await.pre_hook(&request, &no_callback).await;
        let built = request
            .clone()
            .build(&self.client, self.http.token(), self.base.as_deref())
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .execute(built)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        // The hook would sleep out a 429; a read just fails instead.
        if status != StatusCode::TOO_MANY_REQUESTS && hookable(&response) {
            let mut hook = bucket.lock().await;
            let _ = hook
                .post_hook(&response, &request, &no_callback, false)
                .await;
        }
        match status {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(response)),
            status => Err(format!("HTTP {status}")),
        }
    }
}

fn ids(channel: u64, id: Option<u64>) -> Result<(ChannelId, Option<MessageId>), String> {
    let nonzero = channel != 0 && id != Some(0);
    nonzero
        .then(|| (ChannelId::new(channel), id.map(MessageId::new)))
        .ok_or_else(|| "a zero id".to_owned())
}

impl ProbeRead for RepostHttp {
    fn credentials(&self) -> String {
        hex::encode(Sha256::digest(self.http.token().as_bytes()))[..16].to_owned()
    }

    async fn message(&self, channel: u64, id: u64) -> Result<Option<ObservedMessage>, String> {
        let (channel_id, message_id) = ids(channel, Some(id))?;
        let message_id = message_id.ok_or("no message id")?;
        let route = Route::ChannelMessage {
            channel_id,
            message_id,
        };
        let Some(response) = self.get(route, Vec::new()).await? else {
            return Ok(None);
        };
        let seen: Seen = response.json().await.map_err(|error| error.to_string())?;
        Ok(Some(seen.observed()))
    }

    async fn history(
        &self,
        channel: u64,
        before: Option<u64>,
        limit: u8,
    ) -> Result<Vec<ObservedMessage>, String> {
        let (channel_id, before) = ids(channel, before)?;
        let mut params = vec![("limit", limit.to_string())];
        params.extend(before.map(|before| ("before", before.get().to_string())));
        let route = Route::ChannelMessages { channel_id };
        let response = self.get(route, params).await?.ok_or("HTTP 404 Not Found")?;
        let page: Vec<Seen> = response.json().await.map_err(|error| error.to_string())?;
        Ok(page.into_iter().map(Seen::observed).collect())
    }
}

#[cfg(test)]
#[path = "o_writer_repost_io_tests.rs"]
mod tests;
