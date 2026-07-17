//! Send-to-agent dispatch (#3038).
//!
//! Extracted from `services::discord::health`: parses an announce-bot
//! send-to-agent request body and routes the message to a peer agent via
//! the `agent:<role_id>` outbound target. This is the agent-to-agent
//! relay entry point; it is intentionally separate from the health-check
//! logic it used to be fused with.
//!
//! Behavior is unchanged from the prior in-`health` implementation: this
//! is a pure move/regroup. Delivery still flows through
//! `outbound::send_gate::send_message_with_backends` (also re-exported as
//! `health::send_message_with_backends` for compatibility).

use sqlx::PgPool;

use super::send_gate::send_message_with_backends;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::health::HealthRegistry;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedSendToAgentRequest {
    pub(crate) role_id: String,
    pub(crate) message: String,
    pub(crate) mode: UtilityBotRole,
}

pub(crate) fn parse_send_to_agent_body(
    body: &str,
) -> Result<ParsedSendToAgentRequest, &'static str> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let role_id = json
        .get("role_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if role_id.is_empty() {
        return Err("role_id is required");
    }

    let message = json
        .get("message")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    if message.is_empty() {
        return Err("message is required");
    }

    let mode = json
        .get("mode")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(UtilityBotRole::from_alias)
        .unwrap_or(UtilityBotRole::Announce);
    if json
        .get("mode")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty() && UtilityBotRole::from_alias(value).is_none())
    {
        return Err("mode must be announce or notify");
    }

    Ok(ParsedSendToAgentRequest {
        role_id,
        message,
        mode,
    })
}

pub async fn handle_send_to_agent(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    body: &str,
) -> (&'static str, String) {
    let request = match parse_send_to_agent_body(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": error}).to_string(),
            );
        }
    };

    let target = format!("agent:{}", request.role_id);
    send_message_with_backends(
        registry,
        pg_pool,
        &target,
        &request.message,
        "system",
        request.mode.alias(),
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_request_defaults_mode_announce() {
        let parsed =
            parse_send_to_agent_body(r#"{"role_id":"td","message":"hi"}"#).expect("should parse");
        assert_eq!(
            parsed,
            ParsedSendToAgentRequest {
                role_id: "td".to_string(),
                message: "hi".to_string(),
                mode: UtilityBotRole::Announce,
            }
        );
    }

    #[test]
    fn trims_role_id_and_respects_explicit_mode() {
        let parsed =
            parse_send_to_agent_body(r#"{"role_id":"  td  ","message":"hi","mode":"notify"}"#)
                .expect("should parse");
        assert_eq!(parsed.role_id, "td");
        assert_eq!(parsed.mode, UtilityBotRole::Notify);
    }

    #[test]
    fn rejects_invalid_json() {
        assert_eq!(parse_send_to_agent_body("not json"), Err("invalid JSON"));
    }

    #[test]
    fn rejects_missing_role_id() {
        assert_eq!(
            parse_send_to_agent_body(r#"{"message":"hi"}"#),
            Err("role_id is required")
        );
    }

    #[test]
    fn rejects_empty_role_id() {
        assert_eq!(
            parse_send_to_agent_body(r#"{"role_id":"   ","message":"hi"}"#),
            Err("role_id is required")
        );
    }

    #[test]
    fn rejects_missing_message() {
        assert_eq!(
            parse_send_to_agent_body(r#"{"role_id":"td"}"#),
            Err("message is required")
        );
    }

    #[test]
    fn rejects_unknown_mode() {
        assert_eq!(
            parse_send_to_agent_body(r#"{"role_id":"td","message":"hi","mode":"shout"}"#),
            Err("mode must be announce or notify")
        );
    }
}
