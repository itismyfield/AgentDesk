//! Bounded create-message transport for re-posts: shares the gateway client's route buckets, sends
//! through a client that neither follows redirects nor retries, and repeats only a confirmed 429.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude as serenity;
use reqwest::StatusCode;
use serenity::http::{LightMethod, Ratelimit, RatelimitInfo, Request, Route};
use serenity::{ChannelId, MessageId, UserId};
use tokio::sync::Mutex;

use crate::services::tui_o::repost::send::{
    AttemptGuard, BoundedTransport, CreatedMessage, RepostEnvelope, WireOutcome,
};

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

#[cfg(test)]
#[path = "o_writer_repost_io_tests.rs"]
mod tests;
