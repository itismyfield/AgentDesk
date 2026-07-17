//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. HTTP-facing `/api/discord/send` and
//! `/api/discord/send-dm` handlers (body parsing, delivery-id assembly,
//! reserved voice-namespace rejection).

use sqlx::PgPool;

use super::manual_delivery::{
    ManualDeliveryOutcome, ManualOutboundDeliveryId, SerenityManualOutboundClient,
    deliver_manual_dm_notification, is_reserved_voice_correlation_namespace,
};
use super::send_gate::{SendCallerClass, send_message_with_backends_and_delivery_id_for_caller};
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::health::{HealthRegistry, resolve_bot_http};
use crate::services::discord::outbound::shared_outbound_deduper;

pub async fn handle_send<'a>(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    body: &str,
) -> (&'a str, String) {
    handle_send_with_caller(registry, pg_pool, body, SendCallerClass::LoopbackInternal).await
}

pub async fn handle_send_with_caller<'a>(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    body: &str,
    caller_class: SendCallerClass,
) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let raw_target = json
        .get("target")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("channel_id").and_then(|v| v.as_str()))
        .unwrap_or("");
    let target = if json.get("target").and_then(|v| v.as_str()).is_none()
        && !raw_target.trim().is_empty()
        && !raw_target.trim_start().starts_with("channel:")
    {
        format!("channel:{raw_target}")
    } else {
        raw_target.to_string()
    };
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("message").and_then(|v| v.as_str()))
        .unwrap_or("");
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    let bot = json
        .get("bot")
        .and_then(|v| v.as_str())
        .unwrap_or(UtilityBotRole::Announce.alias());
    let summary = json.get("summary").and_then(|v| v.as_str());
    let delivery_id = match (
        json.get("correlation_id").and_then(|v| v.as_str()),
        json.get("semantic_event_id").and_then(|v| v.as_str()),
    ) {
        (Some(correlation_id), Some(semantic_event_id))
            if !correlation_id.trim().is_empty() && !semantic_event_id.trim().is_empty() =>
        {
            Some(ManualOutboundDeliveryId {
                correlation_id,
                semantic_event_id,
            })
        }
        _ => None,
    };
    if delivery_id.is_some_and(is_reserved_voice_correlation_namespace) {
        return (
            "400 Bad Request",
            serde_json::json!({
                "ok": false,
                "error": "delivery_id correlation namespace is reserved"
            })
            .to_string(),
        );
    }

    send_message_with_backends_and_delivery_id_for_caller(
        registry,
        pg_pool,
        &target,
        content,
        source,
        bot,
        summary,
        delivery_id,
        caller_class,
    )
    .await
}

/// Handle POST /api/discord/send-dm — send a DM to a Discord user.
/// Accepts JSON:
/// {"user_id":"...", "content":"...", "bot":"announce|notify|claude|codex"}
pub async fn handle_senddm(registry: &HealthRegistry, body: &str) -> (&'static str, String) {
    let request = match parse_senddm_body(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": error}).to_string(),
            );
        }
    };

    let http = match resolve_bot_http(registry, &request.bot).await {
        Ok(h) => h,
        Err(resp) => {
            // #3643: a DM that resolves no bot HTTP handle (unknown/misconfigured
            // bot name) previously failed silently server-side — only the caller
            // saw the HTTP error. Log the resolved bot + target so a recurring DM
            // routing failure is attributable to the bot, not guessed.
            tracing::warn!(
                bot = %request.bot,
                user_id = request.user_id,
                status = resp.0,
                "DM send aborted: failed to resolve bot HTTP handle (unknown/misconfigured bot?)"
            );
            return resp;
        }
    };
    let user_id_text = request.user_id.to_string();
    let dm_delivery_id = request.delivery_id();

    match deliver_manual_dm_notification(
        &SerenityManualOutboundClient { http },
        shared_outbound_deduper(),
        request.user_id,
        &request.content,
        &request.bot,
        None,
        dm_delivery_id
            .as_ref()
            .map(|delivery_id| ManualOutboundDeliveryId {
                correlation_id: &delivery_id.0,
                semantic_event_id: &delivery_id.1,
            }),
    )
    .await
    {
        ManualDeliveryOutcome::Sent {
            message_id,
            delivery,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                bot = %request.bot,
                "  [{ts}] 📨 DM: → user {} via shared outbound (bot={})",
                request.user_id,
                request.bot
            );
            let mut response = serde_json::json!({
                "ok": true,
                "user_id": user_id_text,
                "message_id": message_id,
            });
            if let Some(delivery) = delivery {
                response["delivery"] = serde_json::Value::String(delivery.to_string());
            }
            ("200 OK", response.to_string())
        }
        ManualDeliveryOutcome::Failed { detail } => {
            // #3643: surface the bot identity + Discord error (e.g. 50001 Missing
            // Access) server-side. Without this the failure was visible only to
            // the caller, so reports could not distinguish wrong-bot routing from
            // an announce-bot permission/shared-guild gap. `detail` carries the
            // serenity error string including the Discord error code.
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                bot = %request.bot,
                user_id = request.user_id,
                detail = %detail,
                "  [{ts}] ⚠ DM: send failed → user {} (bot={})",
                request.user_id,
                request.bot
            );
            (
                "500 Internal Server Error",
                format!(r#"{{"ok":false,"error":"DM send failed: {}"}}"#, detail),
            )
        }
    }
}

#[derive(Debug, PartialEq)]
struct SendDmRequest {
    user_id: u64,
    content: String,
    bot: String,
    correlation_id: Option<String>,
    semantic_event_id: Option<String>,
    idempotency_key: Option<String>,
}

impl SendDmRequest {
    fn delivery_id(&self) -> Option<(String, String)> {
        let correlation_id = self
            .correlation_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("senddm:{}", self.user_id));
        let semantic_event_id = self
            .semantic_event_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.idempotency_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|key| format!("senddm:{}:{}", self.user_id, normalize_senddm_key(key)))
            });
        semantic_event_id.map(|semantic_event_id| (correlation_id, semantic_event_id))
    }
}

fn normalize_senddm_key(value: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .take(160)
        .collect();
    if normalized.is_empty() {
        "message".to_string()
    } else {
        normalized
    }
}

fn parse_senddm_body(body: &str) -> Result<SendDmRequest, String> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let user_id = parsed["user_id"]
        .as_str()
        .and_then(|value| value.parse().ok())
        .or_else(|| parsed["user_id"].as_u64())
        .ok_or("user_id required (string or number)")?;
    if user_id == 0 {
        return Err("user_id required (string or number)".to_string());
    }

    let content = parsed["content"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or("content required")?
        .to_string();
    let bot = parsed["bot"]
        .as_str()
        .unwrap_or(UtilityBotRole::Announce.alias())
        .to_string();
    let correlation_id = parsed["correlation_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let semantic_event_id = parsed["semantic_event_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let idempotency_key = parsed["idempotency_key"]
        .as_str()
        .or_else(|| parsed["idempotency_id"].as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(SendDmRequest {
        user_id,
        content,
        bot,
        correlation_id,
        semantic_event_id,
        idempotency_key,
    })
}

#[cfg(test)]
mod senddm_parse_tests {
    //! #3038 Phase A characterization tests — pin the `/api/discord/send-dm`
    //! body parsing (`parse_senddm_body` / `normalize_senddm_key`) and the
    //! `SendDmRequest::delivery_id` correlation assembly before the health.rs
    //! directory decomposition.

    use super::{SendDmRequest, normalize_senddm_key, parse_senddm_body};

    #[test]
    fn parse_senddm_body_accepts_string_or_numeric_user_id() {
        let from_string = parse_senddm_body(r#"{"user_id":"42","content":"hello"}"#).unwrap();
        assert_eq!(
            from_string,
            SendDmRequest {
                user_id: 42,
                content: "hello".to_string(),
                bot: "announce".to_string(),
                correlation_id: None,
                semantic_event_id: None,
                idempotency_key: None,
            }
        );

        let from_number =
            parse_senddm_body(r#"{"user_id":42,"content":"hello","bot":"notify"}"#).unwrap();
        assert_eq!(from_number.user_id, 42);
        assert_eq!(from_number.bot, "notify");
    }

    #[test]
    fn parse_senddm_body_pins_required_field_error_strings() {
        assert_eq!(
            parse_senddm_body("not json"),
            Err("invalid JSON".to_string())
        );
        assert_eq!(
            parse_senddm_body(r#"{"content":"hello"}"#),
            Err("user_id required (string or number)".to_string())
        );
        assert_eq!(
            parse_senddm_body(r#"{"user_id":"0","content":"hello"}"#),
            Err("user_id required (string or number)".to_string())
        );
        assert_eq!(
            parse_senddm_body(r#"{"user_id":"42"}"#),
            Err("content required".to_string())
        );
        assert_eq!(
            parse_senddm_body(r#"{"user_id":"42","content":""}"#),
            Err("content required".to_string())
        );
    }

    #[test]
    fn parse_senddm_body_accepts_idempotency_id_alias() {
        let request = parse_senddm_body(
            r#"{"user_id":"42","content":"hello","idempotency_id":"morning-brief"}"#,
        )
        .unwrap();
        assert_eq!(request.idempotency_key.as_deref(), Some("morning-brief"));
    }

    #[test]
    fn normalize_senddm_key_sanitizes_and_truncates() {
        assert_eq!(
            normalize_senddm_key("camelCaseKey-09:ok_v1.2"),
            "camelCaseKey-09:ok_v1.2"
        );
        assert_eq!(
            normalize_senddm_key("snake_case key/with spaces"),
            "snake_case_key_with_spaces"
        );
        assert_eq!(normalize_senddm_key(""), "message");
        assert_eq!(normalize_senddm_key(&"x".repeat(200)).len(), 160);
    }

    #[test]
    fn delivery_id_uses_explicit_correlation_and_semantic_ids() {
        let request = parse_senddm_body(
            r#"{"user_id":"42","content":"hello","correlation_id":" corr-1 ","semantic_event_id":" sem-1 "}"#,
        )
        .unwrap();
        assert_eq!(
            request.delivery_id(),
            Some(("corr-1".to_string(), "sem-1".to_string()))
        );
    }

    #[test]
    fn delivery_id_derives_semantic_id_from_idempotency_key() {
        let request = parse_senddm_body(
            r#"{"user_id":"42","content":"hello","idempotency_key":"daily briefing/morning"}"#,
        )
        .unwrap();
        assert_eq!(
            request.delivery_id(),
            Some((
                "senddm:42".to_string(),
                "senddm:42:daily_briefing_morning".to_string()
            ))
        );
    }

    #[test]
    fn delivery_id_requires_a_semantic_source() {
        let request =
            parse_senddm_body(r#"{"user_id":"42","content":"hello","correlation_id":"corr-only"}"#)
                .unwrap();
        assert_eq!(request.delivery_id(), None);
    }
}

#[cfg(test)]
mod handle_send_contract_tests {
    //! #3038 Phase A characterization tests — pin the `/api/discord/send`
    //! body-parsing 400 responses and the legacy `channel_id` fallback /
    //! `channel:` prefixing behavior of `handle_send` before the health.rs
    //! directory decomposition. All expectations capture current behavior
    //! as-is (no seam; empty `HealthRegistry`).

    use crate::services::discord::health::{HealthRegistry, SendCallerClass};

    use super::{handle_send, handle_send_with_caller};

    struct TestRuntimeRoot {
        previous_root: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TestRuntimeRoot {
        fn new() -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
            let temp = tempfile::tempdir().expect("temp runtime root"); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
            Self {
                previous_root,
                _temp: temp,
                _lock: lock,
            }
        }
    }

    impl Drop for TestRuntimeRoot {
        fn drop(&mut self) {
            match &self.previous_root {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    async fn assert_source_gate_body(
        caller_class: SendCallerClass,
        source: &str,
        expected_status: &str,
        expected_error: &str,
    ) {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        let body = serde_json::json!({
            "target": "channel:999999999999999999",
            "content": "hi",
            "source": source,
        })
        .to_string();

        let (status, body) = handle_send_with_caller(&registry, None, &body, caller_class).await;
        assert_eq!(status, expected_status);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap(); // agentdesk-audit: allow-unwrap — test response JSON should parse
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], expected_error);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_json_body_is_a_400_with_pinned_body() {
        let registry = HealthRegistry::new();
        let (status, body) = handle_send(&registry, None, "not json").await;
        assert_eq!(status, "400 Bad Request");
        assert_eq!(body, r#"{"ok":false,"error":"invalid JSON"}"#);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_content_is_a_400_with_pinned_body() {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        let (status, body) = handle_send(&registry, None, r#"{"target":"channel:123"}"#).await;
        assert_eq!(status, "400 Bad Request");
        assert_eq!(body, r#"{"ok":false,"error":"content is required"}"#);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_target_is_a_400_invalid_target() {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        let (status, body) = handle_send(&registry, None, r#"{"content":"hi"}"#).await;
        assert_eq!(status, "400 Bad Request");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(
            body["error"],
            super::super::send_target::SEND_TARGET_CONTRACT
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_id_fallback_reaches_target_resolution_like_explicit_target() {
        let _runtime_root = TestRuntimeRoot::new();
        let registry = HealthRegistry::new();
        // Legacy `channel_id` (numeric, no `channel:` prefix) must be accepted
        // via the fallback + prefixing path and produce the exact same
        // downstream response as the explicit `target` spelling.
        let (fallback_status, fallback_body) = handle_send(
            &registry,
            None,
            r#"{"channel_id":"999999999999999999","content":"hi","source":"system"}"#,
        )
        .await;
        let (explicit_status, explicit_body) = handle_send(
            &registry,
            None,
            r#"{"target":"channel:999999999999999999","content":"hi","source":"system"}"#,
        )
        .await;
        assert_eq!(fallback_status, explicit_status);
        assert_eq!(fallback_body, explicit_body);
        // Both spellings clear target parsing and reach the authorization
        // ladder: on an empty registry the unmapped channel is rejected by
        // the role-map gate, not by target parsing.
        assert_eq!(fallback_status, "403 Forbidden");
        assert_eq!(
            fallback_body,
            r#"{"ok":false,"error":"channel not in role-map"}"#
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cli_caller_can_use_cli_sources_but_not_system() {
        assert_source_gate_body(
            SendCallerClass::Cli,
            "agentdesk-cli",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Cli,
            "operator",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Cli,
            "system",
            "403 Forbidden",
            "source not allowed for this caller",
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dashboard_unknown_and_loopback_use_caller_specific_source_gate() {
        assert_source_gate_body(
            SendCallerClass::Dashboard,
            "dashboard",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Dashboard,
            "headless_turn",
            "403 Forbidden",
            "source not allowed for this caller",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::Unknown,
            "slo_alerter",
            "403 Forbidden",
            "source not allowed for this caller",
        )
        .await;
        assert_source_gate_body(
            SendCallerClass::LoopbackInternal,
            "headless_turn",
            "403 Forbidden",
            "channel not in role-map",
        )
        .await;
    }
}
