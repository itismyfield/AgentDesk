//! Live provider selection after channel bindings have been loaded.

#[path = "session_evidence.rs"]
pub(crate) mod session_evidence;

use sqlx::PgPool;

use crate::db::agents::{load_agent_channel_bindings_pg, resolve_agent_channel_for_provider_pg};
use crate::db::dispatches::latest_completed_review_provider_pg;
use crate::dispatch::dispatch_destination_provider_override;
use crate::domain::dispatch_route::use_counter_model_channel;

pub async fn resolve_agent_primary_channel_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| bindings.primary_channel()))
}

pub async fn resolve_agent_counter_model_channel_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| bindings.counter_model_channel()))
}

pub async fn resolve_agent_dispatch_channel_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| {
            if use_counter_model_channel(dispatch_type) {
                bindings.counter_model_channel()
            } else {
                bindings.primary_channel()
            }
        }))
}

async fn resolve_agent_channel_with_provider_override_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
    provider_override: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(provider) = provider_override.filter(|provider| !provider.trim().is_empty()) {
        if let Some(channel) = resolve_agent_channel_for_provider_pg(pool, agent_id, Some(provider))
            .await
            .map_err(|error| {
                format!("resolve postgres provider channel for {agent_id} ({provider}): {error}")
            })?
        {
            return Ok(Some(channel));
        }
    }

    resolve_agent_dispatch_channel_pg(pool, agent_id, dispatch_type)
        .await
        .map_err(|error| format!("resolve postgres dispatch channel for {agent_id}: {error}"))
}

pub(crate) async fn resolve_dispatch_delivery_channel_pg(
    pool: &PgPool,
    agent_id: &str,
    card_id: &str,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> Result<Option<String>, String> {
    let provider_override = if dispatch_type == Some("review") {
        dispatch_destination_provider_override(dispatch_type, dispatch_context)
    } else if dispatch_type == Some("review-decision") {
        match dispatch_destination_provider_override(dispatch_type, dispatch_context) {
            Some(provider) => Some(provider),
            None => latest_completed_review_provider_pg(pool, card_id).await?,
        }
    } else {
        None
    };

    resolve_agent_channel_with_provider_override_pg(
        pool,
        agent_id,
        dispatch_type,
        provider_override.as_deref(),
    )
    .await
}

pub(crate) async fn resolve_review_followup_channel_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<String>, String> {
    resolve_agent_primary_channel_pg(pool, agent_id)
        .await
        .map_err(|error| format!("resolve postgres primary review channel for {agent_id}: {error}"))
}

#[cfg(test)]
mod tests {
    use crate::db::agents::AgentChannelBindings;
    use crate::domain::provider::ProviderKind;
    use std::cell::RefCell;

    fn child(name: &str, marker: &str) -> bool {
        if std::env::var_os(marker).is_some() {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(marker, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success()
                && stdout.lines().filter(|line| line.starts_with("test result:")).count() == 1
                && stdout.lines().any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
            "{stdout}\n{}", String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    fn install_default(provider: &str) {
        let mut config = crate::config::Config::default();
        config.onboarding.default_provider = Some(provider.to_string());
        crate::config_live_reload::install(config);
    }

    #[test]
    fn live_default_and_match_order_preserve_provider_fallback() {
        if !child(
            "services::agent_channels::tests::live_default_and_match_order_preserve_provider_fallback",
            "ADK_AGENT_CHANNELS_CHILD",
        ) {
            return;
        }
        let mut bindings = AgentChannelBindings {
            provider: None,
            discord_channel_id: Some(" legacy ".to_string()),
            discord_channel_cc: Some(" cc ".to_string()),
            discord_channel_cdx: Some(" cdx ".to_string()),
            discord_channel_alt: Some(" alt ".to_string()),
        };
        install_default("claude");
        assert_eq!(bindings.primary_channel().as_deref(), Some("cc"));
        install_default("codex");
        assert_eq!(bindings.primary_channel().as_deref(), Some("cdx"));
        assert_eq!(bindings.counter_model_channel().as_deref(), Some("cc"));
        bindings.provider = Some("unknown-provider".to_string());
        assert_eq!(bindings.primary_channel().as_deref(), Some("cdx"));
        bindings.provider = Some("claude".to_string());
        assert_eq!(bindings.primary_channel().as_deref(), Some("cc"));
        bindings.provider = None;
        install_default("claude");
        let calls = RefCell::new(Vec::new());
        assert_eq!(
            bindings.provider_for_channel(|value| {
                calls.borrow_mut().push(value.to_string());
                if value == "legacy" {
                    install_default("codex");
                }
                value == "legacy"
            }),
            Some(ProviderKind::Codex)
        );
        assert_eq!(*calls.borrow(), ["cc", "cdx", "alt", "legacy"]);
        calls.borrow_mut().clear();
        assert_eq!(
            bindings.provider_for_channel(|value| {
                calls.borrow_mut().push(value.to_string());
                value == "cdx"
            }),
            Some(ProviderKind::Codex)
        );
        assert_eq!(*calls.borrow(), ["cc", "cdx"]);
        assert_eq!(bindings.provider_for_channel(|_| true), None);
        assert_eq!(bindings.provider_for_channel(|_| false), None);
    }
}
