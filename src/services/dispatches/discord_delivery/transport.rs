use sqlx::PgPool;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

// Dispatch delivery transport types.
//
// These are non-presentation domain transport types (delivery results, post
// errors, follow-up classification) produced and consumed entirely within the
// dispatch delivery service. They were previously defined in
// `crate::server::dto::dispatches` (#3037 bucket 4); they now live beside the
// transport logic that owns them. The route layer keeps access through the
// `services::dispatches::discord_delivery` re-export facade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewFollowupKind {
    Pass,
    Unknown,
}

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

    /// Preserve retry authority across the canonical outbound-v3 boundary.
    /// Discord 5xx/429/408 responses and failures without an authoritative
    /// HTTP response are transient; deterministic payload/policy failures and
    /// all other 4xx responses are terminal.
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct DispatchNotifyDeliveryResult {
    pub(crate) status: String,
    pub(crate) dispatch_id: String,
    pub(crate) action: String,
    pub(crate) correlation_id: Option<String>,
    pub(crate) semantic_event_id: Option<String>,
    pub(crate) target_channel_id: Option<String>,
    pub(crate) message_id: Option<String>,
    pub(crate) fallback_kind: Option<String>,
    pub(crate) detail: Option<String>,
}

impl DispatchNotifyDeliveryResult {
    pub(crate) fn success(
        dispatch_id: impl Into<String>,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status: "success".to_string(),
            dispatch_id: dispatch_id.into(),
            action: action.into(),
            correlation_id: None,
            semantic_event_id: None,
            target_channel_id: None,
            message_id: None,
            fallback_kind: None,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn duplicate(dispatch_id: impl Into<String>, detail: impl Into<String>) -> Self {
        let dispatch_id = dispatch_id.into();
        Self {
            status: "duplicate".to_string(),
            action: "notify".to_string(),
            correlation_id: Some(format!("dispatch:{dispatch_id}")),
            semantic_event_id: Some(format!("dispatch:{dispatch_id}:notify")),
            dispatch_id,
            target_channel_id: None,
            message_id: None,
            fallback_kind: None,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn permanent_failure(
        dispatch_id: impl Into<String>,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status: "permanent_failure".to_string(),
            dispatch_id: dispatch_id.into(),
            action: action.into(),
            correlation_id: None,
            semantic_event_id: None,
            target_channel_id: None,
            message_id: None,
            fallback_kind: None,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn with_thread_creation_fallback(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.status = "fallback".to_string();
        self.fallback_kind = Some(match self.fallback_kind.take() {
            Some(existing) => format!("ThreadCreationParentChannel+{existing}"),
            None => "ThreadCreationParentChannel".to_string(),
        });
        self.detail = Some(match self.detail.take() {
            Some(existing) if !existing.trim().is_empty() => format!("{detail}; {existing}"),
            _ => detail,
        });
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct DispatchMessagePostOutcome {
    pub(crate) message_id: String,
    pub(crate) delivery: DispatchNotifyDeliveryResult,
}

fn dispatch_delivery_correlation_id(dispatch_id: &str) -> String {
    format!("dispatch:{dispatch_id}")
}

fn dispatch_delivery_semantic_event_id(dispatch_id: &str) -> String {
    format!("dispatch:{dispatch_id}:notify")
}

pub(crate) fn discord_api_base_url() -> String {
    std::env::var("AGENTDESK_DISCORD_API_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DISCORD_API_BASE.to_string())
}

pub(crate) fn discord_api_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// #2842 (relay-stability P2): bounded retry budget for transient HTTP 429s on
/// the raw dispatch transport. Discord rate-limits are a normal operating
/// condition; previously a single 429 fell through to `Other` and became a
/// terminal PermanentFailure with no backoff. The budget is capped so a long
/// (or hostile) Retry-After cannot stall a dispatch worker indefinitely.
const DISCORD_RATE_LIMIT_MAX_RETRIES: u32 = 3;
const DISCORD_RATE_LIMIT_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);
const DISCORD_RATE_LIMIT_DEFAULT_BACKOFF: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Parse Discord's `Retry-After` header (seconds, possibly fractional) into a
/// backoff duration. Returns `None` when the header is absent or unparseable.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| {
            // Clamp BEFORE constructing the Duration: `Duration::from_secs_f64`
            // panics on values that overflow Duration's range, so a hostile or
            // malformed Retry-After (e.g. "1e30") would crash the worker path
            // instead of being capped. The caller caps the backoff to
            // DISCORD_RATE_LIMIT_MAX_BACKOFF anyway, so clamping to that ceiling
            // here is both safe and behavior-preserving.
            let capped = seconds.min(DISCORD_RATE_LIMIT_MAX_BACKOFF.as_secs_f64());
            std::time::Duration::from_secs_f64(capped)
        })
}

/// Send the request produced by `build`, retrying on HTTP 429 up to
/// `DISCORD_RATE_LIMIT_MAX_RETRIES` times while honoring a capped Retry-After.
/// Returns the first non-429 response, or the last 429 response once the retry
/// budget is exhausted (callers then classify it as any other failure).
/// `build` is invoked fresh per attempt because a reqwest `RequestBuilder` is
/// consumed on `send()`.
async fn send_with_rate_limit_retry<F>(build: F) -> reqwest::Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let mut attempt: u32 = 0;
    loop {
        let response = build().send().await?;
        if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS
            || attempt >= DISCORD_RATE_LIMIT_MAX_RETRIES
        {
            return Ok(response);
        }
        let backoff = parse_retry_after(response.headers())
            .unwrap_or(DISCORD_RATE_LIMIT_DEFAULT_BACKOFF)
            .min(DISCORD_RATE_LIMIT_MAX_BACKOFF);
        attempt += 1;
        tracing::warn!(
            target: "discord::dispatch_transport",
            attempt,
            backoff_ms = backoff.as_millis() as u64,
            "dispatch transport received HTTP 429; backing off then retrying"
        );
        tokio::time::sleep(backoff).await;
    }
}

pub(crate) fn is_discord_length_error(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    body.contains("BASE_TYPE_MAX_LENGTH")
        || lowered.contains("2000 or fewer in length")
        || lowered.contains("100 or fewer in length")
        || lowered.contains("string value is too long")
        || (body.contains("50035") && lowered.contains("length"))
}

fn discord_error_code_from_body(body: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("code").and_then(|code| code.as_i64()))
}

/// Pure POST helper with no pre-truncation. The shared outbound layer owns
/// length policy and fallback decisions.
pub(crate) async fn post_raw_message_once(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message: &str,
) -> Result<String, DispatchMessagePostError> {
    let message_url = discord_api_url(base_url, &format!("/channels/{channel_id}/messages"));
    let response = send_with_rate_limit_retry(|| {
        client
            .post(&message_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
    })
    .await
    .map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("failed to post dispatch message to {channel_id}: {error}"),
        )
    })?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let kind = if is_discord_length_error(status, &body) {
            DispatchMessagePostErrorKind::MessageTooLong
        } else {
            DispatchMessagePostErrorKind::Other
        };
        return Err(DispatchMessagePostError::http(
            kind,
            status,
            discord_error_code_from_body(&body),
            format!("failed to post dispatch message to {channel_id}: {status} {body}"),
        ));
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to parse dispatch message response for {channel_id}: {error}"),
            )
        })?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("dispatch message response for {channel_id} omitted message id"),
            )
        })
}

/// Pure PATCH helper used by the unified outbound API for edit operations.
pub(crate) async fn edit_raw_message_once(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message_id: &str,
    content: &str,
) -> Result<String, DispatchMessagePostError> {
    let url = discord_api_url(
        base_url,
        &format!("/channels/{channel_id}/messages/{message_id}"),
    );
    let response = send_with_rate_limit_retry(|| {
        client
            .patch(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({ "content": content }))
    })
    .await
    .map_err(|error| {
        DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("failed to edit dispatch message {message_id}: {error}"),
        )
    })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let kind = if is_discord_length_error(status, &body) {
            DispatchMessagePostErrorKind::MessageTooLong
        } else {
            DispatchMessagePostErrorKind::Other
        };
        return Err(DispatchMessagePostError::http(
            kind,
            status,
            discord_error_code_from_body(&body),
            format!("Discord edit failed for message {message_id}: {status} {body}"),
        ));
    }

    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("failed to parse dispatch edit response for {message_id}: {error}"),
            )
        })?;
    body.get("id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .ok_or_else(|| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("dispatch edit response for {message_id} omitted message id"),
            )
        })
}

pub(crate) async fn post_dispatch_message_to_channel_with_delivery(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message: &str,
    minimal_message: &str,
    dispatch_id: Option<&str>,
) -> Result<DispatchMessagePostOutcome, DispatchMessagePostError> {
    use crate::services::discord::outbound::delivery::{deliver_outbound, first_raw_message_id};
    use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
    use crate::services::discord::outbound::policy::DiscordOutboundPolicy;
    use crate::services::discord::outbound::result::{DeliveryResult, FallbackUsed};
    use crate::services::discord::outbound::{HttpOutboundClient, shared_outbound_deduper};
    use poise::serenity_prelude::ChannelId;

    let outbound_client =
        HttpOutboundClient::new(client.clone(), token.to_string(), base_url.to_string());
    let correlation_id = dispatch_id.map(dispatch_delivery_correlation_id);
    let semantic_event_id = dispatch_id.map(dispatch_delivery_semantic_event_id);
    let mut policy = DiscordOutboundPolicy::dispatch_outbox();
    let (delivery_correlation_id, delivery_semantic_event_id) =
        match (correlation_id.as_ref(), semantic_event_id.as_ref()) {
            (Some(correlation_id), Some(semantic_event_id)) => {
                (correlation_id.clone(), semantic_event_id.clone())
            }
            _ => {
                policy = policy.without_idempotency();
                (
                    "dispatch:adhoc".to_string(),
                    "dispatch:adhoc:notify".to_string(),
                )
            }
        };
    let target_channel_id = channel_id
        .parse::<u64>()
        .map(ChannelId::new)
        .map_err(|error| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                format!("invalid dispatch outbound channel id {channel_id}: {error}"),
            )
        })?;
    let outbound_msg = DiscordOutboundMessage::new(
        delivery_correlation_id,
        delivery_semantic_event_id,
        message,
        OutboundTarget::Channel(target_channel_id),
        policy,
    )
    .with_summary(minimal_message.to_string());

    match deliver_outbound(
        &outbound_client,
        shared_outbound_deduper(),
        outbound_msg,
        None,
    )
    .await
    {
        DeliveryResult::Sent { messages, .. } => {
            let message_id = first_raw_message_id(&messages).unwrap_or_default();
            Ok(DispatchMessagePostOutcome {
                message_id: message_id.clone(),
                delivery: DispatchNotifyDeliveryResult {
                    status: "success".to_string(),
                    dispatch_id: dispatch_id.unwrap_or("").to_string(),
                    action: "notify".to_string(),
                    correlation_id,
                    semantic_event_id,
                    target_channel_id: Some(channel_id.to_string()),
                    message_id: Some(message_id),
                    fallback_kind: None,
                    detail: None,
                },
            })
        }
        DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => {
            let message_id = first_raw_message_id(&messages).unwrap_or_default();
            if matches!(fallback_used, FallbackUsed::MinimalFallback) {
                tracing::warn!(
                    "[dispatch] Message too long for channel {channel_id}; retried with minimal fallback"
                );
            }
            Ok(DispatchMessagePostOutcome {
                message_id: message_id.clone(),
                delivery: DispatchNotifyDeliveryResult {
                    status: "fallback".to_string(),
                    dispatch_id: dispatch_id.unwrap_or("").to_string(),
                    action: "notify".to_string(),
                    correlation_id,
                    semantic_event_id,
                    target_channel_id: Some(channel_id.to_string()),
                    message_id: Some(message_id),
                    fallback_kind: Some(match fallback_used {
                        FallbackUsed::MinimalFallback => "MinimalFallback".to_string(),
                        FallbackUsed::LengthCompacted => "Truncated".to_string(),
                        other => format!("{other:?}"),
                    }),
                    detail: Some("shared outbound API used degraded delivery".to_string()),
                },
            })
        }
        DeliveryResult::Duplicate { .. } => Ok(DispatchMessagePostOutcome {
            message_id: String::new(),
            delivery: DispatchNotifyDeliveryResult {
                status: "duplicate".to_string(),
                dispatch_id: dispatch_id.unwrap_or("").to_string(),
                action: "notify".to_string(),
                correlation_id,
                semantic_event_id,
                target_channel_id: Some(channel_id.to_string()),
                message_id: None,
                fallback_kind: None,
                detail: Some("shared outbound API deduplicated delivery".to_string()),
            },
        }),
        DeliveryResult::Skip { .. } => Err(DispatchMessagePostError::new(
            DispatchMessagePostErrorKind::Other,
            format!("unexpected skip for channel {channel_id}"),
        )),
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => {
            let kind = if reason.to_ascii_lowercase().contains("base_type_max_length")
                || reason.contains("length")
            {
                DispatchMessagePostErrorKind::MessageTooLong
            } else {
                DispatchMessagePostErrorKind::Other
            };
            Err(DispatchMessagePostError::new(kind, reason))
        }
    }
}

pub(crate) async fn add_thread_member_to_dispatch_thread(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    thread_id: &str,
    user_id: u64,
) -> Result<(), String> {
    let thread_info_url = discord_api_url(base_url, &format!("/channels/{thread_id}"));
    let response = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect thread {thread_id}: {err}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "failed to inspect thread {thread_id}: {status} {body}"
        ));
    }

    let thread_info = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse thread {thread_id}: {err}"))?;
    let is_archived = thread_info
        .get("thread_metadata")
        .and_then(|metadata| metadata.get("archived"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    if is_archived {
        let response = client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": false}))
            .send()
            .await
            .map_err(|err| format!("failed to unarchive thread {thread_id}: {err}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "failed to unarchive thread {thread_id}: {status} {body}"
            ));
        }
    }

    let member_url = discord_api_url(
        base_url,
        &format!("/channels/{thread_id}/thread-members/{user_id}"),
    );
    let response = client
        .put(&member_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to add user {user_id} to thread {thread_id}: {err}"))?;

    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "failed to add user {user_id} to thread {thread_id}: {status} {body}"
        ))
    }
}

pub(crate) async fn maybe_add_owner_to_dispatch_thread(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    thread_id: &str,
    dispatch_id: &str,
    owner_user_id: Option<u64>,
) {
    let Some(owner_user_id) = owner_user_id else {
        return;
    };

    if let Err(err) =
        add_thread_member_to_dispatch_thread(client, token, base_url, thread_id, owner_user_id)
            .await
    {
        tracing::warn!(
            "[dispatch] Failed to add owner {} to thread {} for dispatch {}: {}",
            owner_user_id,
            thread_id,
            dispatch_id,
            err
        );
    }
}

const SLOT_THREAD_RESET_MESSAGE_LIMIT: u64 = 500;
const SLOT_THREAD_RESET_MAX_AGE_DAYS: i64 = 7;

fn discord_thread_created_at(
    thread_id: &str,
    thread_info: &serde_json::Value,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(timestamp) = thread_info
        .get("thread_metadata")
        .and_then(|metadata| metadata.get("create_timestamp"))
        .and_then(|value| value.as_str())
    {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) {
            return Some(parsed.with_timezone(&chrono::Utc));
        }
    }

    let raw_id = thread_id.parse::<u64>().ok()?;
    let timestamp_ms = (raw_id >> 22) + 1_420_070_400_000;
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms as i64)
}

pub(crate) async fn reset_stale_slot_thread_if_needed(
    pg_pool: Option<&PgPool>,
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    dispatch_id: &str,
    slot_binding: &crate::db::dispatches::SlotThreadBinding,
    exclude_entry_id: Option<&str>,
) -> Result<bool, String> {
    let Some(thread_id) = slot_binding.thread_id.as_deref() else {
        return Ok(false);
    };

    let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
    let response = client
        .get(&thread_info_url)
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await
        .map_err(|err| format!("failed to inspect slot thread {thread_id}: {err}"))?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let thread_info = response
        .json::<serde_json::Value>()
        .await
        .map_err(|err| format!("failed to parse slot thread {thread_id}: {err}"))?;
    let total_message_sent = thread_info
        .get("total_message_sent")
        .and_then(|value| value.as_u64())
        .or_else(|| {
            thread_info
                .get("message_count")
                .and_then(|value| value.as_u64())
        })
        .unwrap_or(0);
    let message_limit_hit = total_message_sent > SLOT_THREAD_RESET_MESSAGE_LIMIT;
    let age_limit_hit = discord_thread_created_at(thread_id, &thread_info)
        .map(|created_at| {
            chrono::Utc::now().signed_duration_since(created_at)
                > chrono::Duration::days(SLOT_THREAD_RESET_MAX_AGE_DAYS)
        })
        .unwrap_or(false);

    if !message_limit_hit && !age_limit_hit {
        return Ok(false);
    }

    tracing::info!(
        "[dispatch] resetting stale slot thread before dispatch {}: agent={} slot={} messages={} age_limit_hit={}",
        dispatch_id,
        slot_binding.agent_id,
        slot_binding.slot_index,
        total_message_sent,
        age_limit_hit,
    );
    let pool = pg_pool.ok_or_else(|| {
        format!("postgres pool required while resetting stale slot thread for {dispatch_id}")
    })?;
    crate::services::auto_queue::runtime::reset_slot_thread_bindings_excluding_pg(
        pool,
        &slot_binding.agent_id,
        slot_binding.slot_index,
        Some(dispatch_id),
        exclude_entry_id,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn archive_duplicate_slot_threads(
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    pg_pool: Option<&PgPool>,
    expected_parent: u64,
    keep_thread_id: &str,
    candidate_thread_ids: &[String],
) {
    for thread_id in candidate_thread_ids {
        if thread_id == keep_thread_id {
            continue;
        }

        let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
        let response = match client
            .get(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to inspect duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        if !response.status().is_success() {
            continue;
        }

        let thread_info = match response.json::<serde_json::Value>().await {
            Ok(thread_info) => thread_info,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to parse duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        let parent_id = thread_info
            .get("parent_id")
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        if parent_id != expected_parent {
            continue;
        }

        let already_archived = thread_info
            .get("thread_metadata")
            .and_then(|metadata| metadata.get("archived"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if already_archived {
            continue;
        }

        match crate::services::discord::should_defer_thread_archive_pg(pg_pool, thread_id).await {
            Ok(true) => {
                tracing::warn!(
                    "[dispatch] Skipping duplicate slot thread archive for {thread_id}: active turn or fresh inflight still present"
                );
                continue;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Skipping duplicate slot thread archive for {thread_id}: active-check failed: {err}"
                );
                continue;
            }
        }

        match client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": true}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("[dispatch] Archived duplicate slot thread {thread_id}");
            }
            Ok(resp) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {}",
                    resp.status()
                );
            }
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {err}"
                );
            }
        }
    }
}

/// Discord delivery side-effects boundary.
/// Keep business rules local and swap transport behavior in tests.
pub(crate) trait DispatchTransport: Send + Sync {
    fn pg_pool(&self) -> Option<&PgPool> {
        None
    }

    fn send_dispatch(
        &self,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<DispatchNotifyDeliveryResult, String>> + Send;

    fn send_review_followup(
        &self,
        review_dispatch_id: String,
        card_id: String,
        channel_id_num: u64,
        message: String,
        kind: ReviewFollowupKind,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::{Arc, Mutex};

    #[test]
    fn discord_api_url_normalizes_boundaries() {
        assert_eq!(
            discord_api_url("https://discord.test/", "/channels/123/messages"),
            "https://discord.test/channels/123/messages"
        );
    }

    #[test]
    fn detects_discord_length_error_shapes() {
        assert!(is_discord_length_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"code":50035,"message":"BASE_TYPE_MAX_LENGTH"}"#,
        ));
        assert!(!is_discord_length_error(
            reqwest::StatusCode::FORBIDDEN,
            "BASE_TYPE_MAX_LENGTH",
        ));
    }

    #[test]
    fn discord_error_code_from_body_reads_typed_json_code() {
        assert_eq!(
            discord_error_code_from_body(r#"{"message":"Missing Access","code":50001}"#),
            Some(50001)
        );
        assert_eq!(discord_error_code_from_body("Missing Access 50001"), None);
    }

    #[test]
    fn parse_retry_after_reads_seconds_and_rejects_garbage() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

        // #2842: Discord sends Retry-After as (possibly fractional) seconds.
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("1.5"));
        assert_eq!(
            parse_retry_after(&headers),
            Some(std::time::Duration::from_secs_f64(1.5))
        );

        headers.insert(RETRY_AFTER, HeaderValue::from_static("0"));
        assert_eq!(parse_retry_after(&headers), Some(std::time::Duration::ZERO));

        // Unparseable / absent header => no backoff hint (caller uses default).
        headers.insert(RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&headers), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);

        // A huge/hostile finite value must NOT panic in Duration::from_secs_f64;
        // it is clamped to the max backoff ceiling instead.
        headers.insert(RETRY_AFTER, HeaderValue::from_static("1e30"));
        assert_eq!(
            parse_retry_after(&headers),
            Some(DISCORD_RATE_LIMIT_MAX_BACKOFF)
        );
    }

    #[test]
    fn thread_creation_fallback_preserves_existing_delivery_detail() {
        let result = DispatchNotifyDeliveryResult::success(
            "dispatch-1517",
            "notify",
            "delivered with minimal fallback",
        )
        .with_thread_creation_fallback("thread creation failed with 403");

        assert_eq!(result.status, "fallback");
        assert_eq!(
            result.fallback_kind.as_deref(),
            Some("ThreadCreationParentChannel")
        );
        assert_eq!(
            result.detail.as_deref(),
            Some("thread creation failed with 403; delivered with minimal fallback")
        );
    }

    fn roundtrip_delivery(delivery: DispatchNotifyDeliveryResult) -> DispatchNotifyDeliveryResult {
        let value = serde_json::to_value(&delivery).expect("delivery DTO serializes");
        serde_json::from_value(value).expect("delivery DTO deserializes")
    }

    #[test]
    fn delivery_result_dto_pins_success_path() {
        let delivery = roundtrip_delivery(DispatchNotifyDeliveryResult::success(
            "dispatch-ok",
            "notify",
            "sent",
        ));

        assert_eq!(delivery.status, "success");
        assert_eq!(delivery.dispatch_id, "dispatch-ok");
        assert_eq!(delivery.action, "notify");
        assert_eq!(delivery.detail.as_deref(), Some("sent"));
        assert_eq!(delivery.fallback_kind, None);
    }

    #[test]
    fn delivery_result_dto_pins_duplicate_path() {
        let delivery = roundtrip_delivery(DispatchNotifyDeliveryResult::duplicate(
            "dispatch-dup",
            "already sent",
        ));

        assert_eq!(delivery.status, "duplicate");
        assert_eq!(delivery.dispatch_id, "dispatch-dup");
        assert_eq!(
            delivery.correlation_id.as_deref(),
            Some("dispatch:dispatch-dup")
        );
        assert_eq!(
            delivery.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-dup:notify")
        );
    }

    #[test]
    fn delivery_result_dto_pins_permanent_failure_path() {
        let delivery = roundtrip_delivery(DispatchNotifyDeliveryResult::permanent_failure(
            "dispatch-fail",
            "notify",
            "discord rejected",
        ));

        assert_eq!(delivery.status, "permanent_failure");
        assert_eq!(delivery.dispatch_id, "dispatch-fail");
        assert_eq!(delivery.action, "notify");
        assert_eq!(delivery.detail.as_deref(), Some("discord rejected"));
    }

    #[tokio::test]
    async fn post_dispatch_message_retries_with_minimal_fallback_after_length_error() {
        #[derive(Default)]
        struct MockState {
            calls: usize,
            posted_messages: Vec<String>,
        }

        async fn post_message(
            State(state): State<Arc<Mutex<MockState>>>,
            body: String,
        ) -> impl IntoResponse {
            let content = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("content")
                        .and_then(|content| content.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let mut guard = state.lock().unwrap();
            guard.calls += 1;
            guard.posted_messages.push(content);
            if guard.calls == 1 {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "code": 50035,
                        "errors": {
                            "content": {
                                "_errors": [{
                                    "code": "BASE_TYPE_MAX_LENGTH",
                                    "message": "Must be 2000 or fewer in length."
                                }]
                            }
                        }
                    })),
                )
                    .into_response();
            }

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "id": "message-minimal" })),
            )
                .into_response()
        }

        let state = Arc::new(Mutex::new(MockState::default()));
        let app = Router::new()
            .route("/channels/{channel_id}/messages", post(post_message))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let primary_message = "A".repeat(180);
        let minimal_message = "minimal fallback message";
        let outcome = post_dispatch_message_to_channel_with_delivery(
            &reqwest::Client::new(),
            "announce-token",
            &base_url,
            "123",
            &primary_message,
            minimal_message,
            Some("dispatch-length-fallback"),
        )
        .await
        .unwrap();

        server.abort();

        assert_eq!(outcome.message_id, "message-minimal");
        assert_eq!(outcome.delivery.status, "fallback");
        assert_eq!(outcome.delivery.dispatch_id, "dispatch-length-fallback");
        assert_eq!(
            outcome.delivery.correlation_id.as_deref(),
            Some("dispatch:dispatch-length-fallback")
        );
        assert_eq!(
            outcome.delivery.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-length-fallback:notify")
        );
        assert_eq!(outcome.delivery.target_channel_id.as_deref(), Some("123"));
        assert_eq!(
            outcome.delivery.message_id.as_deref(),
            Some("message-minimal")
        );
        assert_eq!(
            outcome.delivery.fallback_kind.as_deref(),
            Some("MinimalFallback")
        );

        let guard = state.lock().unwrap();
        assert_eq!(guard.calls, 2);
        assert_eq!(
            guard.posted_messages,
            vec![primary_message, minimal_message.to_string()]
        );
    }
}
