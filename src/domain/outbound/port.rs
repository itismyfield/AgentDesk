//! Transport port over the Discord HTTP API with plain string ids, its failure classes, and
//! the caller-facing delivery port for Serenity-free intents.

use std::future::Future;

use super::intent::OutboundIntent;
use super::result::DeliveryResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchMessagePostErrorKind {
    MessageTooLong,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchMessagePostError {
    kind: DispatchMessagePostErrorKind,
    detail: String,
    http_status: Option<reqwest::StatusCode>,
    discord_error_code: Option<i64>,
}

impl DispatchMessagePostError {
    pub(crate) fn new(kind: DispatchMessagePostErrorKind, detail: String) -> Self {
        Self {
            kind,
            detail,
            http_status: None,
            discord_error_code: None,
        }
    }

    pub(crate) fn http(
        kind: DispatchMessagePostErrorKind,
        status: reqwest::StatusCode,
        discord_error_code: Option<i64>,
        detail: String,
    ) -> Self {
        Self {
            kind,
            detail,
            http_status: Some(status),
            discord_error_code,
        }
    }

    pub(crate) fn kind(&self) -> DispatchMessagePostErrorKind {
        self.kind
    }

    pub(crate) fn http_status(&self) -> Option<reqwest::StatusCode> {
        self.http_status
    }

    pub(crate) fn discord_error_code(&self) -> Option<i64> {
        self.discord_error_code
    }

    pub(crate) fn is_length_error(&self) -> bool {
        self.kind == DispatchMessagePostErrorKind::MessageTooLong
    }

    /// 5xx/429/408 and failures without an HTTP response keep retry authority; other 4xx
    /// responses and payload/policy failures are terminal.
    pub(crate) fn is_transient(&self) -> bool {
        if self.is_length_error() {
            return false;
        }
        self.http_status.is_none_or(|status| {
            status.is_server_error()
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
        })
    }
}

impl std::fmt::Display for DispatchMessagePostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for DispatchMessagePostError {}

/// Production wraps `reqwest`; tests inject a deterministic client. Each call returns the
/// created or edited message id.
#[allow(async_fn_in_trait)]
pub(crate) trait DiscordOutboundClient: Send + Sync {
    async fn post_message(
        &self,
        target_channel: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError>;

    /// Clients that cannot express references fall back to a plain post.
    async fn post_message_with_reference(
        &self,
        target_channel: &str,
        content: &str,
        _reference_channel: &str,
        _reference_message: &str,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_message(target_channel, content).await
    }

    /// Clients without nonce support keep the plain post; Serenity clients override this.
    async fn post_message_with_nonce(
        &self,
        target_channel: &str,
        content: &str,
        _nonce: &str,
        _enforce_nonce: bool,
    ) -> Result<String, DispatchMessagePostError> {
        self.post_message(target_channel, content).await
    }

    /// Keeps the reference even when the client cannot express a nonce.
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

    async fn resolve_dm_channel(&self, user_id: &str) -> Result<String, DispatchMessagePostError> {
        Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("outbound client does not support DM channel resolution for user {user_id}"),
        ))
    }
}

/// Delivers one intent; the implementation owns the dedup reservation and nonce use.
pub(crate) trait OutboundDelivery: Send + Sync {
    fn deliver(&self, intent: OutboundIntent) -> impl Future<Output = DeliveryResult> + Send;
}
