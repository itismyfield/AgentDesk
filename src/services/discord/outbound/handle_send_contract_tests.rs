//! #3038 Phase A characterization tests — pin the `/api/discord/send`
//! body-parsing 400 responses and the legacy `channel_id` fallback /
//! `channel:` prefixing behavior of `handle_send` before the health.rs
//! directory decomposition. All expectations capture current behavior
//! as-is (no seam; empty `HealthRegistry`).

use crate::services::discord::health::HealthRegistry;
use crate::services::message_outbox::source_registry::SendCallerClass;

use crate::services::discord::outbound::send_api::{handle_send, handle_send_with_caller};

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
        crate::services::discord::outbound::send_target::SEND_TARGET_CONTRACT
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
