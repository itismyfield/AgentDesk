//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. The `/api/discord/send` authorization ladder
//! (content → target → source label → role-map binding → bot resolution;
//! #2047 Findings 7/9/10) and the caller-class source-label gate.

use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use sqlx::PgPool;

use super::manual_delivery::{
    ManualNotification, ManualOutboundDeliveryId, ResolvedManualMessage,
    SerenityManualOutboundClient, send_authorized_manual_message,
};
use super::send_target::{
    SendTargetResolutionError, resolve_send_target_channel_id_with_backends,
    routine_thread_parent_hint,
};
use crate::services::discord::health::{HealthRegistry, resolve_bot_http};
use crate::services::discord::outbound::shared_outbound_deduper;
use crate::services::provider::ProviderKind;

pub use super::source_registry::SendCallerClass;
use super::source_registry::validate_send_source_for;

/// Handle POST /api/discord/send — agent-to-agent native routing.
/// Accepts JSON: {"target":"channel:<id>|channel:<name>|agent:<roleId>", "content":"...", "source":"role-id", "bot":"announce|notify", "summary":"..."}
///
/// `summary` is optional minimal fallback content if Discord rejects the
/// length-truncated primary send.
pub(crate) async fn send_message_with_backends(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
) -> (&'static str, String) {
    send_message_with_backends_and_delivery_id(
        registry, pg_pool, target, content, source, bot, summary, None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_message_with_backends_and_delivery_id(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
) -> (&'static str, String) {
    send_message_with_backends_and_delivery_options_for_caller(
        registry,
        pg_pool,
        target,
        content,
        source,
        bot,
        summary,
        delivery_id,
        ManualOutboundOptions::default(),
        SendCallerClass::LoopbackInternal,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_message_with_backends_and_delivery_id_for_caller(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    options: ManualOutboundOptions,
    caller_class: SendCallerClass,
) -> (&'static str, String) {
    send_message_with_backends_and_delivery_options_for_caller(
        registry,
        pg_pool,
        target,
        content,
        source,
        bot,
        summary,
        delivery_id,
        options,
        caller_class,
    )
    .await
}

const HEADLESS_TURN_OUTBOX_SOURCE: &str = "headless_turn";

pub(super) fn production_manual_outbound_client(
    http: std::sync::Arc<serenity::Http>,
) -> SerenityManualOutboundClient {
    SerenityManualOutboundClient { http }
}

pub(in crate::services::discord) fn dm_default_agent_authorizes_unmapped_private_channel(
    is_private_channel: bool,
    source: &str,
    provider: &ProviderKind,
    session_bound_to_provider: bool,
) -> bool {
    if !is_private_channel {
        return false;
    }

    let source = source.trim();
    crate::services::discord::agentdesk_config::dm_default_agent_allows_outbound_source(
        provider, source,
    ) || (source == HEADLESS_TURN_OUTBOX_SOURCE
        && session_bound_to_provider
        && crate::services::discord::agentdesk_config::resolve_dm_default_agent(provider).is_some())
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ManualOutboundOptions {
    pub(crate) output_origin:
        Option<crate::services::cluster::intake_router_hook::owner_record::output::Artifact>,
    pub(crate) allow_unbound_internal_channel: bool,
    /// Grants the durable message-outbox producer access to its reserved
    /// delivery-id namespace and inline Discord nonce rollout. This is
    /// intentionally independent from unbound-channel authorization.
    pub(crate) trusted_internal_outbox: bool,
    pub(crate) channel_owner_provider: Option<ProviderKind>,
    pub(crate) image_attachment:
        Option<std::sync::Arc<crate::services::message_outbox::StoredOutboxAttachment>>,
    pub(crate) record_transcript: bool,
    pub(crate) transcript_source_label: Option<String>,
}

/// Provider-owned notices use the same channel runtime credentials as tool cards.
async fn resolve_manual_bot_http(
    registry: &HealthRegistry,
    bot: &str,
    channel_id: ChannelId,
    options: &ManualOutboundOptions,
) -> Result<std::sync::Arc<serenity::Http>, (&'static str, String)> {
    if let Some(provider) = options.channel_owner_provider.as_ref() {
        return crate::services::discord::session_notice::provider_http(
            registry, provider, channel_id,
        )
        .await
        .map_err(|error| ("503 Service Unavailable", error));
    }
    resolve_bot_http(registry, bot).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_message_with_backends_and_delivery_options(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    options: ManualOutboundOptions,
) -> (&'static str, String) {
    send_message_with_backends_and_delivery_options_for_caller(
        registry,
        pg_pool,
        target,
        content,
        source,
        bot,
        summary,
        delivery_id,
        options,
        SendCallerClass::LoopbackInternal,
    )
    .await
}

fn rejects_untrusted_message_outbox_delivery_id(
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    trusted_internal_outbox_path: bool,
) -> bool {
    !trusted_internal_outbox_path
        && delivery_id.is_some_and(|delivery_id| {
            delivery_id
                .correlation_id
                .trim_start()
                .starts_with("message_outbox:")
        })
}

#[allow(clippy::too_many_arguments)]
async fn send_message_with_backends_and_delivery_options_for_caller(
    registry: &HealthRegistry,
    pg_pool: Option<&PgPool>,
    target: &str,
    content: &str,
    source: &str,
    bot: &str,
    summary: Option<&str>,
    delivery_id: Option<ManualOutboundDeliveryId<'_>>,
    options: ManualOutboundOptions,
    caller_class: SendCallerClass,
) -> (&'static str, String) {
    if rejects_untrusted_message_outbox_delivery_id(delivery_id, options.trusted_internal_outbox) {
        return (
            "400 Bad Request",
            serde_json::json!({
                "ok": false,
                "error": "delivery_id correlation namespace is reserved"
            })
            .to_string(),
        );
    }
    if options.image_attachment.is_some() && !options.trusted_internal_outbox {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"binary attachment requires trusted outbox"}"#.to_string(),
        );
    }
    if content.is_empty() {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"content is required"}"#.to_string(),
        );
    }

    let channel_id_raw = match resolve_send_target_channel_id_with_backends(pg_pool, target).await {
        Ok(id) => id,
        Err(SendTargetResolutionError::BadRequest(message)) => {
            return (
                "400 Bad Request",
                serde_json::json!({"ok": false, "error": message}).to_string(),
            );
        }
        Err(SendTargetResolutionError::NotFound(message)) => {
            return (
                "404 Not Found",
                serde_json::json!({"ok": false, "error": message}).to_string(),
            );
        }
        Err(SendTargetResolutionError::Internal(message)) => {
            return (
                "500 Internal Server Error",
                serde_json::json!({"ok": false, "error": message}).to_string(),
            );
        }
    };

    let channel_id = ChannelId::new(channel_id_raw);

    // Validate source is a known agent role_id or internal system source.
    // Issue #2047 Finding 9 — don't echo the caller-supplied label back in the
    // response body. That made enumerating the whitelist trivial and gave a
    // log-injection assist. The full label is preserved in `tracing::warn!`
    // for operators.
    let source_allowed = if caller_class == SendCallerClass::LoopbackInternal {
        is_allowed_send_source(source)
    } else {
        is_allowed_send_source_for(source, caller_class)
    };
    if !source_allowed {
        tracing::warn!(
            source,
            bot,
            caller_class = ?caller_class,
            "/api/discord/send rejected: source label not allowed for caller class"
        );
        return (
            "403 Forbidden",
            r#"{"ok":false,"error":"source not allowed for this caller"}"#.to_string(),
        );
    }

    // Verify target channel exists in role-map (authorization check).
    // If the target is a thread, resolve its parent channel and check that instead.
    // Pass channel name so byChannelName-style configs can match.
    if crate::services::discord::settings::resolve_role_binding(channel_id, None).is_none() {
        let routine_parent_hint = routine_thread_parent_hint(pg_pool, channel_id).await;
        let mut authorized = false;
        let mut target_channel_accessible = false;
        // Try resolving as a thread: fetch channel info and check parent_id.
        //
        // Issue #2047 Finding 10 — also use the fetched channel name to retry
        // the byChannelName fallback for the target channel itself. The first
        // `resolve_role_binding(channel_id, None)` above can only match
        // `byChannelId` entries; a channel registered with `byChannelName`
        // only was previously blocked even though it is legitimately mapped.
        if let Ok(http) = resolve_manual_bot_http(registry, bot, channel_id, &options).await {
            if let Ok(channel) = channel_id.to_channel(&*http).await {
                target_channel_accessible = true;
                let is_private_channel = matches!(&channel, serenity::Channel::Private(_));
                if !authorized
                    && registry
                        .dm_default_agent_authorizes_private_channel(
                            channel_id,
                            is_private_channel,
                            source,
                        )
                        .await
                {
                    authorized = true;
                    tracing::info!(
                        target_channel_id = channel_id.get(),
                        source,
                        bot,
                        "allowing outbound delivery to dm_default_agent-bound private channel"
                    );
                }
                // `Channel::guild` consumes the value, so derive the target
                // channel name first via `clone()`; the original `channel`
                // is then consumed by the thread/parent walk below.
                let target_name = channel.clone().guild().map(|gc| gc.name.clone());
                // First: byChannelName retry on the *target* channel itself.
                if !authorized
                    && crate::services::discord::settings::resolve_role_binding(
                        channel_id,
                        target_name.as_deref(),
                    )
                    .is_some()
                {
                    authorized = true;
                }
                if let Some(guild_channel) = channel.guild() {
                    if let Some(parent_id) = guild_channel.parent_id {
                        if let Some(expected_parent) = routine_parent_hint {
                            if expected_parent != parent_id {
                                tracing::warn!(
                                    target_channel_id = channel_id.get(),
                                    actual_parent_id = parent_id.get(),
                                    expected_parent_id = expected_parent.get(),
                                    "routine thread parent hint did not match Discord parent"
                                );
                            }
                        }
                        // Resolve parent channel name for byChannelName configs
                        let parent_name = if let Ok(parent_ch) = parent_id.to_channel(&*http).await
                        {
                            parent_ch.guild().map(|pg| pg.name.clone())
                        } else {
                            None
                        };
                        if crate::services::discord::settings::resolve_role_binding(
                            parent_id,
                            parent_name.as_deref(),
                        )
                        .is_some()
                        {
                            authorized = true;
                        }
                    }
                }
            }
        }
        if !authorized
            && options.allow_unbound_internal_channel
            && is_allowed_send_source_for(source, caller_class)
            && target.trim_start().starts_with("channel:")
            && target_channel_accessible
        {
            authorized = true;
            tracing::warn!(
                target_channel_id = channel_id.get(),
                source,
                bot,
                "allowing trusted internal Discord relay to unbound but accessible channel"
            );
        }
        if !authorized {
            return (
                "403 Forbidden",
                r#"{"ok":false,"error":"channel not in role-map"}"#.to_string(),
            );
        }
    }

    // Utility bot aliases resolve through UtilityBotRole before provider lookup.
    let http = match resolve_manual_bot_http(registry, bot, channel_id, &options).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };

    let outbound_client = production_manual_outbound_client(http);
    send_authorized_manual_message(
        &outbound_client,
        shared_outbound_deduper(),
        ResolvedManualMessage {
            channel_id_raw,
            target,
            pg_pool,
            record_transcript: options.record_transcript,
            transcript_source_label: options.transcript_source_label.as_deref(),
            notification: ManualNotification {
                channel_id: &channel_id_raw.to_string(),
                content,
                bot,
                source,
                summary,
                delivery_id,
                headless_nonce_enabled: false,
                attachment: None,
            },
        },
        &options,
    )
    .await
}

/// Backward-compatible label gate. New code paths should call
/// [`is_allowed_send_source_for`] with an explicit caller-class. This wrapper
/// behaves as if the call came from `LoopbackInternal` so existing in-process
/// publishers (lifecycle notifier, headless turn, …) keep working without
/// surface-level rewrites.
fn is_allowed_send_source(source: &str) -> bool {
    is_allowed_send_source_for(source, SendCallerClass::LoopbackInternal)
}

/// Issue #2047 Finding 7 — gate the `source` label by caller-class.
pub fn is_allowed_send_source_for(source: &str, caller: SendCallerClass) -> bool {
    validate_send_source_for(source, caller).is_ok()
}

#[cfg(test)]
mod send_source_tests {
    use super::{
        SendCallerClass, dm_default_agent_authorizes_unmapped_private_channel,
        is_allowed_send_source, is_allowed_send_source_for,
        rejects_untrusted_message_outbox_delivery_id,
    };
    use crate::services::provider::ProviderKind;

    #[tokio::test]
    async fn provider_notice_uses_channel_owner_tool_card_credentials() {
        use crate::services::discord::{health::HealthRegistry, make_shared_data_for_tests};
        use std::sync::Arc;
        let registry = HealthRegistry::new();
        let first = make_shared_data_for_tests();
        let owner = make_shared_data_for_tests();
        first.settings.write().await.allowed_channel_ids = vec![41];
        owner.settings.write().await.allowed_channel_ids = vec![42];
        first
            .http
            .cached_bot_token
            .set("first-token".into())
            .unwrap();
        owner
            .http
            .cached_bot_token
            .set("owner-token".into())
            .unwrap();
        registry.register("claude".into(), Arc::clone(&first)).await;
        registry.register("claude".into(), Arc::clone(&owner)).await;
        registry
            .register_http(
                "claude".into(),
                first.serenity_http_or_token_fallback().unwrap(),
            )
            .await;
        let options = super::ManualOutboundOptions {
            trusted_internal_outbox: true,
            channel_owner_provider: Some(ProviderKind::Claude),
            ..Default::default()
        };
        let http = super::resolve_manual_bot_http(
            &registry,
            "claude",
            super::ChannelId::new(42),
            &options,
        )
        .await
        .unwrap();
        assert!(http.token().ends_with("owner-token"));
        first.settings.write().await.allowed_channel_ids.push(42);
        assert_eq!(
            super::resolve_manual_bot_http(
                &registry,
                "claude",
                super::ChannelId::new(42),
                &options
            )
            .await
            .unwrap_err()
            .0,
            "503 Service Unavailable"
        );
        assert!(
            super::resolve_manual_bot_http(
                &HealthRegistry::new(),
                "claude",
                super::ChannelId::new(42),
                &options
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn reserved_gate_requires_explicit_outbox_authority_and_preserves_voice_delivery() {
        let voice = super::ManualOutboundDeliveryId {
            correlation_id: "voice:7001:8002:utterance",
            semantic_event_id: "announce:generation:1",
        };
        let outbox = super::ManualOutboundDeliveryId {
            correlation_id: "message_outbox:headless_turn:terminal:session-a",
            semantic_event_id: "message_outbox:41:deliver",
        };

        assert!(!rejects_untrusted_message_outbox_delivery_id(
            Some(voice),
            false
        ));
        assert!(rejects_untrusted_message_outbox_delivery_id(
            Some(outbox),
            false
        ));
        // Unbound-channel permission is not message-outbox authority.
        let unbound_only = super::ManualOutboundOptions {
            allow_unbound_internal_channel: true,
            ..Default::default()
        };
        assert!(rejects_untrusted_message_outbox_delivery_id(
            Some(outbox),
            unbound_only.trusted_internal_outbox,
        ));
        let trusted_outbox = super::ManualOutboundOptions {
            trusted_internal_outbox: true,
            ..Default::default()
        };
        assert!(!rejects_untrusted_message_outbox_delivery_id(
            Some(outbox),
            trusted_outbox.trusted_internal_outbox,
        ));
    }

    #[test]
    fn headless_turn_is_allowed_internal_send_source() {
        assert!(is_allowed_send_source("headless_turn"));
        assert!(is_allowed_send_source("lifecycle_notifier"));
        assert!(is_allowed_send_source("routine-runtime"));
        assert!(is_allowed_send_source("slo_alerter"));
        assert!(is_allowed_send_source("quality_regression_alerter"));
        assert!(is_allowed_send_source("auto-queue-monitor"));
        assert!(is_allowed_send_source("inventory"));
        assert!(is_allowed_send_source("voice"));
        assert!(!is_allowed_send_source("not-a-real-source"));
    }

    #[test]
    fn dashboard_cannot_impersonate_system_or_headless_turn() {
        // Issue #2047 Finding 7 — dashboards / browser callers must not be
        // able to claim 강력 internal labels.
        assert!(!is_allowed_send_source_for(
            "system",
            SendCallerClass::Dashboard
        ));
        assert!(!is_allowed_send_source_for(
            "headless_turn",
            SendCallerClass::Dashboard
        ));
        assert!(!is_allowed_send_source_for(
            "auto-queue",
            SendCallerClass::Dashboard
        ));
    }

    #[test]
    fn cli_cannot_impersonate_loopback_only_labels() {
        assert!(!is_allowed_send_source_for("system", SendCallerClass::Cli));
        assert!(!is_allowed_send_source_for(
            "kanban-rules",
            SendCallerClass::Cli
        ));
        assert!(is_allowed_send_source_for(
            "agentdesk-cli",
            SendCallerClass::Cli
        ));
        assert!(is_allowed_send_source_for("operator", SendCallerClass::Cli));
    }

    #[test]
    fn dashboard_can_use_dashboard_or_known_agent_role_labels() {
        assert!(is_allowed_send_source_for(
            "dashboard",
            SendCallerClass::Dashboard
        ));

        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap(); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
        std::fs::create_dir_all(temp.path().join("config")).unwrap(); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
        std::fs::write(
            temp.path().join("config/agentdesk.yaml"),
            r#"
server: {}
agents:
  - id: project-agentdesk
    name: AgentDesk
    provider: codex
    channels:
      codex:
        id: "123"
        prompt_file: "/tmp/project-agentdesk.md"
        workspace: "/tmp"
        provider: codex
"#,
        )
        .unwrap(); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        checkpoint(&[("AGENTDESK_ROOT_DIR", temp.path().as_os_str())]);

        assert!(is_allowed_send_source_for(
            "project-agentdesk",
            SendCallerClass::Dashboard
        ));
    }

    use crate::test_env_panic_probe::{assert_root_restored, checkpoint};

    #[test]
    fn dashboard_role_labels_restores_env_after_panic_present() {
        assert_root_restored(true, dashboard_can_use_dashboard_or_known_agent_role_labels);
    }

    #[test]
    fn dashboard_role_labels_restores_env_after_panic_absent() {
        assert_root_restored(
            false,
            dashboard_can_use_dashboard_or_known_agent_role_labels,
        );
    }

    #[test]
    fn unknown_caller_class_only_allows_known_agents() {
        // Without a verified caller class we still let messages through when
        // the source matches a registered agent role id (the agent identity
        // itself is the attestation). Strong internal labels are denied.
        assert!(!is_allowed_send_source_for(
            "system",
            SendCallerClass::Unknown
        ));
        assert!(!is_allowed_send_source_for(
            "dashboard",
            SendCallerClass::Unknown
        ));
    }

    #[test]
    fn loopback_internal_keeps_existing_internal_label_acceptance() {
        for label in [
            "kanban-rules",
            "triage-rules",
            "review-automation",
            "auto-queue",
            "system",
            "headless_turn",
            "lifecycle_notifier",
            "routine-runtime",
        ] {
            assert!(
                is_allowed_send_source_for(label, SendCallerClass::LoopbackInternal),
                "loopback caller must keep accepting `{label}`"
            );
        }
    }

    #[test]
    fn dm_default_agent_allows_headless_private_channel_when_provider_bound() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::tempdir().unwrap(); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
        std::fs::create_dir_all(temp.path().join("config")).unwrap(); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
        std::fs::write(
            temp.path().join("config/agentdesk.yaml"),
            r#"
server: {}
discord:
  dm_default_agent: family-counsel
agents:
  - id: family-counsel
    name: Family Counsel
    provider: claude
    channels:
      claude:
        id: "123"
        prompt_file: "/tmp/family-counsel.md"
        workspace: "/tmp"
        provider: claude
"#,
        )
        .unwrap(); // agentdesk-audit: allow-unwrap — test setup in #[cfg(test)] mod
        let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        checkpoint(&[("AGENTDESK_ROOT_DIR", temp.path().as_os_str())]);

        assert!(dm_default_agent_authorizes_unmapped_private_channel(
            true,
            "headless_turn",
            &ProviderKind::Claude,
            true,
        ));
        assert!(!dm_default_agent_authorizes_unmapped_private_channel(
            true,
            "headless_turn",
            &ProviderKind::Claude,
            false,
        ));
        assert!(!dm_default_agent_authorizes_unmapped_private_channel(
            false,
            "headless_turn",
            &ProviderKind::Claude,
            true,
        ));
    }

    #[test]
    fn dm_default_agent_restores_env_after_panic_present() {
        assert_root_restored(
            true,
            dm_default_agent_allows_headless_private_channel_when_provider_bound,
        );
    }

    #[test]
    fn dm_default_agent_restores_env_after_panic_absent() {
        assert_root_restored(
            false,
            dm_default_agent_allows_headless_private_channel_when_provider_bound,
        );
    }

    #[test]
    fn from_header_parses_known_caller_classes() {
        assert_eq!(
            SendCallerClass::from_header("cli"),
            Some(SendCallerClass::Cli)
        );
        assert_eq!(
            SendCallerClass::from_header("AgentDesk-CLI"),
            Some(SendCallerClass::Cli)
        );
        assert_eq!(
            SendCallerClass::from_header("dashboard"),
            Some(SendCallerClass::Dashboard)
        );
        assert_eq!(
            SendCallerClass::from_header("loopback"),
            Some(SendCallerClass::LoopbackInternal)
        );
        assert_eq!(
            SendCallerClass::from_header("dcserver"),
            Some(SendCallerClass::LoopbackInternal)
        );
        assert_eq!(SendCallerClass::from_header(""), None);
        assert_eq!(SendCallerClass::from_header("attacker"), None);
    }
}

#[cfg(test)]
pub(crate) use super::manual_delivery::image_delivery_tests::deliver_options_on_wire;
