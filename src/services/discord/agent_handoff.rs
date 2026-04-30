use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::health::{self, HealthRegistry};
use crate::db::agents::AgentChannelBindings;

const ANNOUNCE_BOT: &str = "announce";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentHandoffChannelKind {
    Cc,
    Cdx,
}

impl AgentHandoffChannelKind {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, AgentHandoffError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("cc") => Ok(Self::Cc),
            Some("cdx") => Ok(Self::Cdx),
            Some(_) => Err(AgentHandoffError::bad_request(
                "channel_kind must be cc or cdx",
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cc => "cc",
            Self::Cdx => "cdx",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AgentHandoffResponse {
    pub(crate) to_agent_id: String,
    pub(crate) channel_id: String,
    pub(crate) channel_kind: &'static str,
    pub(crate) message_id: String,
    pub(crate) bot: &'static str,
    pub(crate) prefixed: bool,
}

impl AgentHandoffResponse {
    pub(crate) fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHandoffError {
    status: StatusCode,
    body: Value,
}

impl AgentHandoffError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: json!({"error": message.into()}),
        }
    }

    fn agent_not_found(to_agent_id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: json!({"error": "agent not found", "to_agent_id": to_agent_id}),
        }
    }

    fn channel_kind_unset(
        to_agent_id: &str,
        channel_kind: AgentHandoffChannelKind,
        available_kinds: Vec<&'static str>,
    ) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: json!({
                "error": "channel_kind unset",
                "to_agent_id": to_agent_id,
                "channel_kind": channel_kind.as_str(),
                "available_kinds": available_kinds,
            }),
        }
    }

    fn announce_bot_not_configured() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({"error": "announce bot not configured"}),
        }
    }

    fn discord_send_failed(detail: String) -> Self {
        let discord_status = extract_discord_status(&detail);
        let mut body = json!({
            "error": "Discord send failed",
            "detail": detail,
        });
        if let Some(status) = discord_status {
            body["discord_status"] = json!(status);
        }
        Self {
            status: StatusCode::BAD_GATEWAY,
            body,
        }
    }

    fn upstream(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({"error": message.into()}),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn body(&self) -> Value {
        self.body.clone()
    }

    pub(crate) fn one_line(&self) -> String {
        let error = self
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("send-to-agent failed");
        match error {
            "agent not found" => {
                let to_agent_id = self
                    .body
                    .get("to_agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                format!("agent not found: {to_agent_id}")
            }
            "channel_kind unset" => {
                let channel_kind = self
                    .body
                    .get("channel_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let available = self
                    .body
                    .get("available_kinds")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                format!("channel_kind unset: {channel_kind}; available_kinds=[{available}]")
            }
            "Discord send failed" => self
                .body
                .get("detail")
                .and_then(Value::as_str)
                .map(|detail| format!("Discord send failed: {detail}"))
                .unwrap_or_else(|| "Discord send failed".to_string()),
            other => other.to_string(),
        }
    }
}

pub(crate) fn build_agent_handoff_content(
    from_agent_id: &str,
    to_agent_id: &str,
    message: &str,
    prefix: bool,
) -> String {
    if prefix {
        format!("[{from_agent_id} → {to_agent_id} 핸드오프]\n\n{message}")
    } else {
        message.to_string()
    }
}

pub(crate) async fn send_agent_handoff(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    from_agent_id: &str,
    to_agent_id: &str,
    message: &str,
    channel_kind: AgentHandoffChannelKind,
    prefix: bool,
) -> Result<AgentHandoffResponse, AgentHandoffError> {
    let from_agent_id = from_agent_id.trim();
    if from_agent_id.is_empty() {
        return Err(AgentHandoffError::bad_request("from_agent_id is required"));
    }
    let to_agent_id = to_agent_id.trim();
    if to_agent_id.is_empty() {
        return Err(AgentHandoffError::bad_request("to_agent_id is required"));
    }
    if message.is_empty() {
        return Err(AgentHandoffError::bad_request("message is required"));
    }

    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pg_pool, to_agent_id)
        .await
        .map_err(|error| AgentHandoffError::internal(format!("query agent channels: {error}")))?
        .ok_or_else(|| AgentHandoffError::agent_not_found(to_agent_id))?;

    let Some(channel_id) = channel_for_kind(&bindings, channel_kind) else {
        return Err(AgentHandoffError::channel_kind_unset(
            to_agent_id,
            channel_kind,
            available_channel_kinds(&bindings),
        ));
    };

    let content = build_agent_handoff_content(from_agent_id, to_agent_id, message, prefix);
    let target = format!("channel:{channel_id}");
    let (status, response_body) = health::send_message_with_backends(
        registry,
        None,
        Some(pg_pool),
        &target,
        &content,
        from_agent_id,
        ANNOUNCE_BOT,
        None,
    )
    .await;
    let status = parse_health_status_code(status);
    let response_json: Value = serde_json::from_str(&response_body)
        .unwrap_or_else(|_| json!({"error": response_body.clone()}));

    if !status.is_success() {
        return Err(map_send_failure(status, response_json));
    }

    Ok(AgentHandoffResponse {
        to_agent_id: to_agent_id.to_string(),
        channel_id,
        channel_kind: channel_kind.as_str(),
        message_id: response_json
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        bot: ANNOUNCE_BOT,
        prefixed: prefix,
    })
}

fn channel_for_kind(
    bindings: &AgentChannelBindings,
    channel_kind: AgentHandoffChannelKind,
) -> Option<String> {
    match channel_kind {
        AgentHandoffChannelKind::Cc => normalized_channel(bindings.discord_channel_cc.as_deref()),
        AgentHandoffChannelKind::Cdx => normalized_channel(bindings.discord_channel_cdx.as_deref()),
    }
}

fn available_channel_kinds(bindings: &AgentChannelBindings) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if normalized_channel(bindings.discord_channel_cc.as_deref()).is_some() {
        kinds.push("cc");
    }
    if normalized_channel(bindings.discord_channel_cdx.as_deref()).is_some() {
        kinds.push("cdx");
    }
    kinds
}

fn normalized_channel(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn map_send_failure(status: StatusCode, body: Value) -> AgentHandoffError {
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let lowered = error.to_ascii_lowercase();

    if lowered.contains("announce bot not configured") {
        return AgentHandoffError::announce_bot_not_configured();
    }
    if lowered.contains("discord send failed") {
        return AgentHandoffError::discord_send_failed(
            error
                .strip_prefix("Discord send failed:")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(&error)
                .to_string(),
        );
    }

    AgentHandoffError::upstream(status, body)
}

fn parse_health_status_code(status: &str) -> StatusCode {
    match status {
        "200 OK" => StatusCode::OK,
        "400 Bad Request" => StatusCode::BAD_REQUEST,
        "403 Forbidden" => StatusCode::FORBIDDEN,
        "404 Not Found" => StatusCode::NOT_FOUND,
        "422 Unprocessable Entity" => StatusCode::UNPROCESSABLE_ENTITY,
        "500 Internal Server Error" => StatusCode::INTERNAL_SERVER_ERROR,
        "502 Bad Gateway" => StatusCode::BAD_GATEWAY,
        "503 Service Unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn extract_discord_status(detail: &str) -> Option<u16> {
    for marker in ["HTTP ", "status "] {
        if let Some((_, rest)) = detail.split_once(marker) {
            let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            if let Ok(status) = digits.parse::<u16>() {
                return Some(status);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(cc: Option<&str>, cdx: Option<&str>) -> AgentChannelBindings {
        AgentChannelBindings {
            discord_channel_cc: cc.map(str::to_string),
            discord_channel_cdx: cdx.map(str::to_string),
            ..AgentChannelBindings::default()
        }
    }

    #[test]
    fn channel_kind_defaults_to_cc() {
        assert_eq!(
            AgentHandoffChannelKind::parse(None).unwrap(),
            AgentHandoffChannelKind::Cc
        );
    }

    #[test]
    fn channel_kind_rejects_unknown_value() {
        let error = AgentHandoffChannelKind::parse(Some("notify")).unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.body()["error"], "channel_kind must be cc or cdx");
    }

    #[test]
    fn handoff_prefix_can_be_enabled_or_disabled() {
        assert_eq!(
            build_agent_handoff_content("project-agentdesk", "adk-dashboard", "hello", true),
            "[project-agentdesk → adk-dashboard 핸드오프]\n\nhello"
        );
        assert_eq!(
            build_agent_handoff_content("project-agentdesk", "adk-dashboard", "hello", false),
            "hello"
        );
    }

    #[test]
    fn channel_selection_uses_explicit_kind_only() {
        let bindings = bindings(Some(" 111 "), Some("222"));
        assert_eq!(
            channel_for_kind(&bindings, AgentHandoffChannelKind::Cc).as_deref(),
            Some("111")
        );
        assert_eq!(
            channel_for_kind(&bindings, AgentHandoffChannelKind::Cdx).as_deref(),
            Some("222")
        );
    }

    #[test]
    fn channel_kind_unset_reports_available_kinds() {
        let error = AgentHandoffError::channel_kind_unset(
            "agent-a",
            AgentHandoffChannelKind::Cc,
            available_channel_kinds(&bindings(None, Some("222"))),
        );
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.body()["available_kinds"], json!(["cdx"]));
        assert_eq!(
            error.one_line(),
            "channel_kind unset: cc; available_kinds=[cdx]"
        );
    }

    #[test]
    fn discord_failure_maps_to_bad_gateway_with_status_when_available() {
        let error = map_send_failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "Discord send failed: HTTP 403 {\"message\":\"Missing Permissions\"}"}),
        );
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.body()["discord_status"], 403);
    }
}
