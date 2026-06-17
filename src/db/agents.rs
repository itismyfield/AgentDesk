use std::collections::BTreeMap;

use anyhow::Result;
use sqlx::{PgPool, Row as SqlxRow};

use crate::services::provider::ProviderKind;

// reason: used only by the legacy-sqlite-tests-gated agent alias path below;
// the production copy lives in db/postgres.rs. See #3034 / #3035.
#[allow(dead_code)]
const LEGACY_AGENT_PREFIX: &str = "openclaw-";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentChannelBindings {
    pub provider: Option<String>,
    pub discord_channel_id: Option<String>,
    pub discord_channel_alt: Option<String>,
    pub discord_channel_cc: Option<String>,
    pub discord_channel_cdx: Option<String>,
}

impl AgentChannelBindings {
    fn configured_provider_kind(&self) -> Option<ProviderKind> {
        self.provider.as_deref().and_then(ProviderKind::from_str)
    }

    fn primary_provider_kind(&self) -> Option<ProviderKind> {
        self.configured_provider_kind()
            .or_else(ProviderKind::default_channel_provider)
    }

    pub(crate) fn resolved_primary_provider_kind(&self) -> Option<ProviderKind> {
        let configured_provider = self.primary_provider_kind()?;
        if self
            .provider_specific_channel(&configured_provider)
            .is_some()
        {
            return Some(configured_provider);
        }

        configured_provider
            .preferred_counterparts()
            .into_iter()
            .find(|provider| self.provider_specific_channel(provider).is_some())
    }

    fn provider_specific_channel(&self, provider: &ProviderKind) -> Option<String> {
        match provider {
            ProviderKind::Claude => self.claude_channel(),
            ProviderKind::Codex => self.codex_channel(),
            ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Qwen
                if self.configured_provider_kind().as_ref() == Some(provider) =>
            {
                let primary = self.legacy_primary_channel()?;
                let explicit_claude = normalized_channel(self.discord_channel_cc.clone());
                if matches!(provider, ProviderKind::Gemini | ProviderKind::Qwen)
                    && explicit_claude.as_deref() == Some(primary.as_str())
                {
                    None
                } else {
                    Some(primary)
                }
            }
            _ => None,
        }
    }

    pub fn primary_channel(&self) -> Option<String> {
        if let Some(primary_provider) = self.resolved_primary_provider_kind() {
            if let Some(channel) = self.provider_specific_channel(&primary_provider) {
                return Some(channel);
            }
        }
        self.legacy_primary_channel()
            .or_else(|| self.codex_channel())
            .or_else(|| self.claude_channel())
    }

    pub fn counter_model_channel(&self) -> Option<String> {
        self.resolved_primary_provider_kind().and_then(|provider| {
            let primary_channel = self.provider_specific_channel(&provider)?;
            provider
                .preferred_counterparts()
                .into_iter()
                .find_map(|counterpart| self.provider_specific_channel(&counterpart))
                .filter(|channel| channel != &primary_channel)
        })
    }

    pub fn channel_for_provider(&self, provider: Option<&str>) -> Option<String> {
        match provider.and_then(ProviderKind::from_str) {
            Some(kind) => self
                .provider_specific_channel(&kind)
                .or_else(|| self.legacy_primary_channel()),
            _ => self.legacy_primary_channel(),
        }
    }

    pub fn all_channels(&self) -> Vec<String> {
        let mut channels = Vec::new();
        for value in [
            self.discord_channel_id.clone(),
            self.discord_channel_alt.clone(),
            self.discord_channel_cc.clone(),
            self.discord_channel_cdx.clone(),
        ] {
            if let Some(channel) = normalized_channel(value) {
                if !channels.contains(&channel) {
                    channels.push(channel);
                }
            }
        }
        channels
    }

    fn claude_channel(&self) -> Option<String> {
        normalized_channel(self.discord_channel_cc.clone())
            .or_else(|| normalized_channel(self.discord_channel_id.clone()))
    }

    fn codex_channel(&self) -> Option<String> {
        normalized_channel(self.discord_channel_cdx.clone())
            .or_else(|| normalized_channel(self.discord_channel_alt.clone()))
    }

    fn legacy_primary_channel(&self) -> Option<String> {
        normalized_channel(self.discord_channel_id.clone())
            .or_else(|| normalized_channel(self.discord_channel_cc.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(provider: &str, primary_channel: &str) -> AgentChannelBindings {
        AgentChannelBindings {
            provider: Some(provider.to_string()),
            discord_channel_id: Some(primary_channel.to_string()),
            ..AgentChannelBindings::default()
        }
    }

    #[test]
    fn resolved_primary_provider_preserves_gemini_primary_binding() {
        let bindings = binding("gemini", "1470000000000000001");

        assert_eq!(
            bindings.resolved_primary_provider_kind(),
            Some(ProviderKind::Gemini)
        );
        assert_eq!(
            bindings.primary_channel().as_deref(),
            Some("1470000000000000001")
        );
    }

    #[test]
    fn resolved_primary_provider_preserves_qwen_primary_binding() {
        let bindings = binding("qwen", "1470000000000000002");

        assert_eq!(
            bindings.resolved_primary_provider_kind(),
            Some(ProviderKind::Qwen)
        );
        assert_eq!(
            bindings.primary_channel().as_deref(),
            Some("1470000000000000002")
        );
    }

    #[test]
    fn resolved_primary_provider_does_not_treat_claude_fallback_as_gemini() {
        let bindings = AgentChannelBindings {
            provider: Some("gemini".to_string()),
            discord_channel_id: Some("1470000000000000003".to_string()),
            discord_channel_cc: Some("1470000000000000003".to_string()),
            ..AgentChannelBindings::default()
        };

        assert_eq!(
            bindings.resolved_primary_provider_kind(),
            Some(ProviderKind::Claude)
        );
        assert_eq!(
            bindings.primary_channel().as_deref(),
            Some("1470000000000000003")
        );
    }

    #[test]
    fn resolved_primary_provider_does_not_treat_claude_fallback_as_qwen() {
        let bindings = AgentChannelBindings {
            provider: Some("qwen".to_string()),
            discord_channel_id: Some("1470000000000000004".to_string()),
            discord_channel_cc: Some("1470000000000000004".to_string()),
            ..AgentChannelBindings::default()
        };

        assert_eq!(
            bindings.resolved_primary_provider_kind(),
            Some(ProviderKind::Claude)
        );
        assert_eq!(
            bindings.primary_channel().as_deref(),
            Some("1470000000000000004")
        );
    }
}

fn normalized_channel(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub async fn load_agent_channel_bindings_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<AgentChannelBindings>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         FROM agents
         WHERE id = $1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        Ok(AgentChannelBindings {
            provider: row.try_get("provider")?,
            discord_channel_id: row.try_get("discord_channel_id")?,
            discord_channel_alt: row.try_get("discord_channel_alt")?,
            discord_channel_cc: row.try_get("discord_channel_cc")?,
            discord_channel_cdx: row.try_get("discord_channel_cdx")?,
        })
    })
    .transpose()
}

pub async fn load_all_agent_channel_bindings_pg(
    pool: &PgPool,
) -> Result<BTreeMap<String, AgentChannelBindings>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
         FROM agents",
    )
    .fetch_all(pool)
    .await?;

    let mut bindings = BTreeMap::new();
    for row in rows {
        let agent_id: String = row.try_get("id")?;
        bindings.insert(
            agent_id,
            AgentChannelBindings {
                provider: row.try_get("provider")?,
                discord_channel_id: row.try_get("discord_channel_id")?,
                discord_channel_alt: row.try_get("discord_channel_alt")?,
                discord_channel_cc: row.try_get("discord_channel_cc")?,
                discord_channel_cdx: row.try_get("discord_channel_cdx")?,
            },
        );
    }

    Ok(bindings)
}

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

pub async fn resolve_agent_channel_for_provider_pg(
    pool: &PgPool,
    agent_id: &str,
    provider: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| bindings.channel_for_provider(provider)))
}

pub async fn resolve_agent_dispatch_channel_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
) -> Result<Option<String>, sqlx::Error> {
    Ok(load_agent_channel_bindings_pg(pool, agent_id)
        .await?
        .and_then(|bindings| {
            if matches!(dispatch_type, Some("review" | "e2e-test" | "consultation")) {
                bindings.counter_model_channel()
            } else {
                bindings.primary_channel()
            }
        }))
}
