pub(crate) use crate::server::dto::dispatches::{
    DispatchMessagePostError, DispatchMessagePostErrorKind, DispatchMessagePostOutcome,
    DispatchNotifyDeliveryResult, ReviewFollowupKind,
};
use sqlx::PgPool;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

pub(crate) fn dispatch_delivery_correlation_id(dispatch_id: &str) -> String {
    format!("dispatch:{dispatch_id}")
}

pub(crate) fn dispatch_delivery_semantic_event_id(dispatch_id: &str) -> String {
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
    let response = client
        .post(&message_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
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
        return Err(DispatchMessagePostError::new(
            kind,
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
    let response = client
        .patch(url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({ "content": content }))
        .send()
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
        return Err(DispatchMessagePostError::new(
            kind,
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

pub(crate) async fn post_dispatch_message_to_channel(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    channel_id: &str,
    message: &str,
    minimal_message: &str,
) -> Result<String, DispatchMessagePostError> {
    post_dispatch_message_to_channel_with_delivery(
        client,
        token,
        base_url,
        channel_id,
        message,
        minimal_message,
        None,
    )
    .await
    .map(|outcome| outcome.message_id)
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
    use crate::services::discord::outbound::{HttpOutboundClient, OutboundDeduper};
    use poise::serenity_prelude::ChannelId;

    let outbound_client =
        HttpOutboundClient::new(client.clone(), token.to_string(), base_url.to_string());
    let dedup = OutboundDeduper::new();
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

    match deliver_outbound(&outbound_client, &dedup, outbound_msg).await {
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
        DeliveryResult::PermanentFailure { reason } => {
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
        db: Option<crate::db::Db>,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<DispatchNotifyDeliveryResult, String>> + Send;

    fn send_review_followup(
        &self,
        db: Option<crate::db::Db>,
        review_dispatch_id: String,
        card_id: String,
        channel_id_num: u64,
        message: String,
        kind: ReviewFollowupKind,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

pub(crate) async fn send_dispatch_with_delivery_guard<T: DispatchTransport>(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&PgPool>,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
    transport: &T,
) -> Result<DispatchNotifyDeliveryResult, String> {
    let pg_pool = pg_pool.or_else(|| transport.pg_pool());
    if !claim_dispatch_delivery_guard(pg_pool, dispatch_id).await? {
        return Ok(DispatchNotifyDeliveryResult::duplicate(
            dispatch_id,
            "dispatch delivery guard already recorded this semantic notify event",
        ));
    }

    let send_result = transport
        .send_dispatch(
            db.cloned(),
            agent_id.to_string(),
            title.to_string(),
            card_id.to_string(),
            dispatch_id.to_string(),
        )
        .await;

    finalize_dispatch_delivery_guard(pg_pool, dispatch_id, send_result.is_ok()).await;
    send_result
}

async fn claim_dispatch_delivery_guard(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
) -> Result<bool, String> {
    let pool = pg_pool.ok_or_else(|| "delivery guard requires postgres pool".to_string())?;
    let notified: Option<i32> = sqlx::query_scalar("SELECT 1 FROM kv_meta WHERE key = $1 LIMIT 1")
        .bind(format!("dispatch_notified:{dispatch_id}"))
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("check postgres delivery guard for {dispatch_id}: {error}"))?;
    if notified.is_some() {
        return Ok(false);
    }

    let result = sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(format!("dispatch_reserving:{dispatch_id}"))
    .bind(dispatch_id)
    .execute(pool)
    .await
    .map_err(|error| format!("claim postgres delivery guard for {dispatch_id}: {error}"))?;
    Ok(result.rows_affected() > 0)
}

async fn finalize_dispatch_delivery_guard(
    pg_pool: Option<&PgPool>,
    dispatch_id: &str,
    success: bool,
) {
    let Some(pool) = pg_pool else {
        return;
    };
    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
        .bind(format!("dispatch_reserving:{dispatch_id}"))
        .execute(pool)
        .await
        .ok();
    if success {
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(format!("dispatch_notified:{dispatch_id}"))
        .bind(dispatch_id)
        .execute(pool)
        .await
        .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
    use std::sync::{Arc, Mutex};

    #[test]
    fn duplicate_result_carries_dispatch_idempotency_keys() {
        let result = DispatchNotifyDeliveryResult::duplicate(
            "dispatch-1517",
            "dispatch delivery guard already recorded this semantic notify event",
        );

        assert_eq!(result.status, "duplicate");
        assert_eq!(result.dispatch_id, "dispatch-1517");
        assert_eq!(result.action, "notify");
        assert_eq!(
            result.correlation_id.as_deref(),
            Some("dispatch:dispatch-1517")
        );
        assert_eq!(
            result.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-1517:notify")
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
