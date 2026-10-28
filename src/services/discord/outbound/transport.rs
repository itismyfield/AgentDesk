//! Shared Discord outbound transport and in-process dedup primitives.
//!
//! The v3 envelope/policy/result modules own outbound delivery semantics.
//! This module keeps the transport trait, HTTP/Serenity implementations, and
//! temporary in-memory deduper used by production callers and tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::services::discord::SharedData;
use crate::services::dispatches::discord_delivery::{
    DispatchMessagePostError, DispatchMessagePostErrorKind,
};

pub(crate) async fn post_serenity_message_with_nonce(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    content: &str,
    reference: Option<(ChannelId, MessageId)>,
    nonce: &str,
    enforce_nonce: bool,
) -> Result<String, serenity::Error> {
    let mut message = serenity::CreateMessage::new()
        .content(content)
        .nonce(serenity::model::channel::Nonce::String(nonce.to_string()))
        .enforce_nonce(enforce_nonce)
        .allowed_mentions(crate::services::discord::http::relay_allowed_mentions());
    if let Some((reference_channel, reference_message)) = reference {
        message = message.reference_message(
            serenity::MessageReference::from((reference_channel, reference_message))
                .fail_if_not_exists(false),
        );
    }
    crate::services::discord::rate_limit_wait(shared, channel_id).await;
    channel_id
        .send_message(http, message)
        .await
        .map(|message| message.id.get().to_string())
}

/// Short stable fingerprint for outbound idempotency keys. Parts are separated
/// before hashing so concatenated inputs cannot collide by boundary changes.
pub(crate) fn outbound_fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    format!("{digest:x}").chars().take(16).collect()
}

/// Transport abstraction over the Discord HTTP API. Production code uses an
/// implementation that wraps `reqwest`; tests inject a deterministic mock.
#[allow(async_fn_in_trait)]
pub(crate) trait DiscordOutboundClient: Send + Sync {
    /// Post `content` to `target_channel` (a channel id or thread id).
    /// Returns the created message id on success.
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError>;

    /// Post `content` as a reply/reference to another message. Clients that
    /// cannot express references may fall back to a plain post.
    async fn post_message_with_reference(
        &self,
        target_channel: &str,
        content: &str,
        _reference_channel: &str,
        _reference_message: &str,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_message(target_channel, content).await
    }

    /// Post with Discord's create-message nonce contract. Clients that do not
    /// support nonces preserve legacy behavior; production Serenity clients
    /// override this method.
    async fn post_message_with_nonce(
        &self,
        target_channel: &str,
        content: &str,
        _nonce: &str,
        _enforce_nonce: bool,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_message(target_channel, content).await
    }

    /// Reference-bearing nonce variant used when a durable send is also a
    /// reply. The default retains the reference even when the client cannot
    /// express a nonce.
    async fn post_message_with_reference_and_nonce(
        &self,
        target_channel: &str,
        content: &str,
        reference_channel: &str,
        reference_message: &str,
        _nonce: &str,
        _enforce_nonce: bool,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_message_with_reference(
            target_channel,
            content,
            reference_channel,
            reference_message,
        )
        .await
    }

    /// Edit an existing message. Returns the edited message id on success.
    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        _content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!(
                "outbound client does not support edit for channel {target_channel} message {message_id}"
            ),
        ))
    }

    /// Resolve or create the DM channel for a Discord user id.
    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("outbound client does not support DM channel resolution for user {user_id}"),
        ))
    }
}

/// In-memory dedup table keyed on the serialized v3 outbound dedup key.
///
/// Follow-up slices can swap this for a durable table without touching
/// callers.
#[derive(Clone, Default)]
pub(crate) struct OutboundDeduper {
    inner: Arc<Mutex<HashMap<String, OutboundDedupState>>>,
}

impl OutboundDeduper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the previously-delivered payload, if any.
    pub(crate) fn lookup(&self, key: &str) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        match guard.get(key) {
            Some(OutboundDedupState::Delivered(message_id)) => Some(message_id.clone()),
            Some(OutboundDedupState::InFlight { .. }) | None => None,
        }
    }

    pub(crate) fn record(&self, key: &str, message_id: &str) {
        let notify = if let Ok(mut guard) = self.inner.lock() {
            let notify = match guard.get(key) {
                Some(OutboundDedupState::InFlight { notify }) => Some(notify.clone()),
                _ => None,
            };
            guard.insert(
                key.to_string(),
                OutboundDedupState::Delivered(message_id.to_string()),
            );
            notify
        } else {
            None
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    /// Atomically claims `key` for a send attempt.
    pub(crate) fn reserve(&self, key: &str) -> OutboundDedupClaim {
        if let Ok(mut guard) = self.inner.lock() {
            match guard.get(key) {
                Some(OutboundDedupState::Delivered(message_id)) => {
                    return OutboundDedupClaim::Duplicate(message_id.clone());
                }
                Some(OutboundDedupState::InFlight { notify }) => {
                    return OutboundDedupClaim::InFlight(OutboundDedupInFlight {
                        key: key.to_string(),
                        dedup: self.clone(),
                        notify: notify.clone(),
                    });
                }
                None => {
                    let notify = Arc::new(Notify::new());
                    guard.insert(key.to_string(), OutboundDedupState::InFlight { notify });
                    return OutboundDedupClaim::Reserved(OutboundDedupReservation {
                        key: key.to_string(),
                        dedup: self.clone(),
                        completed: false,
                    });
                }
            }
        }
        OutboundDedupClaim::InFlight(OutboundDedupInFlight {
            key: key.to_string(),
            dedup: self.clone(),
            notify: Arc::new(Notify::new()),
        })
    }

    fn release_inflight(&self, key: &str) {
        let notify = if let Ok(mut guard) = self.inner.lock() {
            match guard.get(key) {
                Some(OutboundDedupState::InFlight { notify }) => {
                    let notify = notify.clone();
                    guard.remove(key);
                    Some(notify)
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    fn inflight_matches(&self, key: &str, expected: &Arc<Notify>) -> Option<bool> {
        let guard = self.inner.lock().ok()?;
        Some(matches!(
            guard.get(key),
            Some(OutboundDedupState::InFlight { notify }) if Arc::ptr_eq(notify, expected)
        ))
    }
}

#[derive(Clone)]
enum OutboundDedupState {
    InFlight { notify: Arc<Notify> },
    Delivered(String),
}

pub(crate) enum OutboundDedupClaim {
    Reserved(OutboundDedupReservation),
    Duplicate(String),
    InFlight(OutboundDedupInFlight),
}

pub(crate) enum OutboundDedupWait {
    Delivered(String),
    Released,
    TimedOut,
}

pub(crate) struct OutboundDedupReservation {
    key: String,
    dedup: OutboundDeduper,
    completed: bool,
}

impl OutboundDedupReservation {
    pub(crate) fn record(&mut self, message_id: &str) {
        self.dedup.record(&self.key, message_id);
        self.completed = true;
    }

    pub(crate) fn release(&mut self) {
        if !self.completed {
            self.dedup.release_inflight(&self.key);
            self.completed = true;
        }
    }
}

impl Drop for OutboundDedupReservation {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct OutboundDedupInFlight {
    key: String,
    dedup: OutboundDeduper,
    notify: Arc<Notify>,
}

impl OutboundDedupInFlight {
    pub(crate) async fn wait_for_delivery(self, timeout: Duration) -> OutboundDedupWait {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(message_id) = self.dedup.lookup(&self.key) {
                return OutboundDedupWait::Delivered(message_id);
            }
            match self.dedup.inflight_matches(&self.key, &self.notify) {
                Some(true) => {}
                Some(false) => return OutboundDedupWait::Released,
                None => return OutboundDedupWait::TimedOut,
            }
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return self
                    .dedup
                    .lookup(&self.key)
                    .map(OutboundDedupWait::Delivered)
                    .unwrap_or(OutboundDedupWait::TimedOut);
            };
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self
                    .dedup
                    .lookup(&self.key)
                    .map(OutboundDedupWait::Delivered)
                    .unwrap_or(OutboundDedupWait::TimedOut);
            }
        }
    }
}

/// Reqwest-backed [`DiscordOutboundClient`] used by production call sites.
#[derive(Clone)]
pub(crate) struct HttpOutboundClient {
    client: reqwest::Client,
    token: String,
    discord_api_base: String,
}

impl HttpOutboundClient {
    pub(crate) fn new(
        client: reqwest::Client,
        token: impl Into<String>,
        discord_api_base: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token: token.into(),
            discord_api_base: discord_api_base.into(),
        }
    }
}

impl DiscordOutboundClient for HttpOutboundClient {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        crate::services::dispatches::discord_delivery::post_raw_message_once(
            &self.client,
            &self.token,
            &self.discord_api_base,
            target_channel,
            content,
        )
        .await
    }

    async fn edit_message(
        &self,
        target_channel: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        crate::services::dispatches::discord_delivery::edit_raw_message_once(
            &self.client,
            &self.token,
            &self.discord_api_base,
            target_channel,
            message_id,
            content,
        )
        .await
    }

    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        let url = crate::services::dispatches::discord_delivery::discord_api_url(
            &self.discord_api_base,
            "/users/@me/channels",
        );
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bot {}", self.token))
            .json(&serde_json::json!({ "recipient_id": user_id }))
            .send()
            .await
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("failed to resolve DM channel for user {user_id}: {error}"),
                )
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to resolve DM channel for user {user_id}: HTTP {status} {body}"),
            ));
        }

        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("failed to parse DM channel response for user {user_id}: {error}"),
                )
            })?;
        body.get("id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    format!("DM channel response for user {user_id} omitted channel id"),
                )
            })
    }
}
