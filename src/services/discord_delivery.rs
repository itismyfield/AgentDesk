use sqlx::PgPool;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

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
}

impl DispatchMessagePostError {
    pub(crate) fn new(kind: DispatchMessagePostErrorKind, detail: String) -> Self {
        Self { kind, detail }
    }

    pub(crate) fn kind(&self) -> DispatchMessagePostErrorKind {
        self.kind
    }

    pub(crate) fn is_length_error(&self) -> bool {
        self.kind == DispatchMessagePostErrorKind::MessageTooLong
    }
}

impl std::fmt::Display for DispatchMessagePostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for DispatchMessagePostError {}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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
            correlation_id: Some(dispatch_delivery_correlation_id(&dispatch_id)),
            semantic_event_id: Some(dispatch_delivery_semantic_event_id(&dispatch_id)),
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchMessagePostOutcome {
    pub(crate) message_id: String,
    pub(crate) delivery: DispatchNotifyDeliveryResult,
}

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
}
