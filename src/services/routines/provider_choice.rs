//! Per-routine provider and model choice: request checks, the route a run starts on, and the
//! API view of explicit versus inherited values.
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::db::agents::{AgentChannelBindings, load_agent_channel_bindings_pg};
use crate::domain::provider::ProviderKind;

const MODEL_MAX_CHARS: usize = 128;

pub use super::execution_mode::ProviderChoice;

#[path = "provider_choice/persistent_codex_patch.rs"]
mod persistent_codex_patch;
pub(crate) use persistent_codex_patch::PersistentModelPatch;

/// Provider, owning channel and model a routine turn starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRoute {
    pub provider: ProviderKind,
    pub channel: String,
    pub model: Option<String>,
}

/// An agent's channel bindings plus the provider-keyed config channels mirrored into `agents`.
pub struct AgentProviderChannels {
    bindings: AgentChannelBindings,
    configured: Option<Value>,
}

impl AgentProviderChannels {
    pub async fn load(pool: &PgPool, agent_id: &str) -> Result<Self> {
        let bindings = load_agent_channel_bindings_pg(pool, agent_id)
            .await
            .map_err(|error| anyhow!("load agent bindings for {agent_id}: {error}"))?
            .ok_or_else(|| anyhow!("agent {agent_id} not found"))?;
        Self::with_bindings(pool, agent_id, bindings).await
    }

    pub async fn with_bindings(
        pool: &PgPool,
        agent_id: &str,
        bindings: AgentChannelBindings,
    ) -> Result<Self> {
        let configured: Option<Option<Value>> =
            sqlx::query_scalar("SELECT provider_channels FROM agents WHERE id = $1")
                .bind(agent_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| anyhow!("load provider channels for {agent_id}: {error}"))?;
        Ok(Self {
            bindings,
            configured: configured.flatten(),
        })
    }

    /// A channel the agent owns for `provider`; another provider's channel never stands in.
    fn channel(&self, provider: &ProviderKind) -> Option<String> {
        let owned = self
            .bindings
            .provider_specific_channel(provider)
            .filter(|channel| {
                self.bindings
                    .provider_for_channel(|candidate| candidate == channel)
                    .as_ref()
                    == Some(provider)
            });
        owned.or_else(|| {
            let configured = self.configured.as_ref()?.get(provider.as_str())?.as_str()?;
            Some(configured.trim().to_string()).filter(|channel| !channel.is_empty())
        })
    }

    /// No choice keeps the primary provider and channel; a chosen provider without a channel of
    /// its own fails instead of running on another provider.
    pub fn route(&self, agent_id: &str, choice: &ProviderChoice) -> Result<ProviderRoute> {
        let primary = self.bindings.resolved_primary_provider_kind();
        let chosen = match choice.provider.as_deref() {
            Some(name) => Some(parse_provider(name).map_err(|error| anyhow!(error))?),
            None => None,
        };
        let provider = match chosen {
            Some(provider) if primary.as_ref() != Some(&provider) => {
                let channel = self.channel(&provider).ok_or_else(|| {
                    anyhow!(
                        "agent {agent_id} has no {} provider channel for this routine",
                        provider.as_str()
                    )
                })?;
                return Ok(ProviderRoute {
                    provider,
                    channel,
                    model: choice.model.clone(),
                });
            }
            _ => primary
                .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?,
        };
        let channel = self
            .bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
        Ok(ProviderRoute {
            provider,
            channel,
            model: choice.model.clone(),
        })
    }
}

pub fn parse_provider(raw: &str) -> Result<ProviderKind, String> {
    ProviderKind::from_str(raw)
        .filter(ProviderKind::is_supported)
        .ok_or_else(|| format!("unknown provider '{}'", raw.trim()))
}

fn normalize_model(raw: &str) -> Result<String, String> {
    let model = raw.trim();
    if model.is_empty() || model.chars().count() > MODEL_MAX_CHARS {
        return Err(format!("model must be 1 to {MODEL_MAX_CHARS} characters"));
    }
    if model
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("model must not contain whitespace".into());
    }
    Ok(model.to_string())
}

/// Canonical provider id and trimmed model; a model always names the provider it belongs to.
pub fn choice_for_request(
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<ProviderChoice, String> {
    let provider = provider
        .map(|raw| parse_provider(raw).map(|kind| kind.as_str().to_string()))
        .transpose()?;
    let model = model.map(normalize_model).transpose()?;
    if model.is_some() && provider.is_none() {
        return Err("model requires provider".into());
    }
    Ok(ProviderChoice { provider, model })
}

/// PATCH: omitted fields keep `current`; an explicit null clears the field.
pub fn choice_for_patch(
    provider: Option<Option<&str>>,
    model: Option<Option<&str>>,
    current: &ProviderChoice,
) -> Result<ProviderChoice, String> {
    choice_for_request(
        provider.unwrap_or(current.provider.as_deref()),
        model.unwrap_or(current.model.as_deref()),
    )
}

/// The primary agent is required for a choice; primary and fallback must each own its channel.
pub(crate) async fn refusal_for_agents(
    pool: &PgPool,
    choice: &ProviderChoice,
    agents: &[Option<&str>],
    persistent_model_patch: Option<&PersistentModelPatch<'_>>,
) -> Result<Option<String>> {
    if choice.provider.is_none() && persistent_model_patch.is_none() {
        return Ok(None);
    }
    if choice.provider.is_some() && agents.first().copied().flatten().is_none() {
        return Ok(Some("provider requires agent_id".into()));
    }
    for agent_id in agents.iter().flatten() {
        let channels = AgentProviderChannels::load(pool, agent_id).await?;
        let route = match channels.route(agent_id, choice) {
            Ok(route) => route,
            Err(error) if choice.provider.is_some() => return Ok(Some(error.to_string())),
            Err(_) => continue,
        };
        if let Some(patch) = persistent_model_patch
            && route.provider == ProviderKind::Codex
            && persistent_codex_patch::refuses(pool, patch, agent_id, &route).await?
        {
            return Ok(Some(
                "Cannot change or clear the model of a persistent Codex routine using a named account. Choose the model when creating a new routine.".into(),
            ));
        }
    }
    Ok(None)
}

/// API view: explicit values plus the provider a run would inherit from the agent today.
pub fn projection(choice: &ProviderChoice, inherited: Option<&ProviderKind>) -> Value {
    let source = |explicit: bool| if explicit { "explicit" } else { "inherited" };
    json!({
        "provider": choice.provider,
        "model": choice.model,
        "effective_provider": choice
            .provider
            .clone()
            .or_else(|| inherited.map(|provider| provider.as_str().to_string())),
        "provider_source": source(choice.provider.is_some()),
        "model_source": source(choice.model.is_some()),
    })
}

/// The provider a routine session runs on: the started attempt's, else the routine's choice.
pub async fn session_route(
    pool: &PgPool,
    bindings: AgentChannelBindings,
    agent_id: &str,
    routine_id: &str,
    started: Option<&Value>,
) -> Result<ProviderRoute> {
    let started = started
        .and_then(|result| result.get("provider"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let provider = match started {
        Some(provider) => Some(provider),
        None => {
            sqlx::query_scalar::<_, Option<String>>("SELECT provider FROM routines WHERE id = $1")
                .bind(routine_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| anyhow!("load routine {routine_id} provider: {error}"))?
                .flatten()
        }
    };
    AgentProviderChannels::with_bindings(pool, agent_id, bindings)
        .await?
        .route(
            agent_id,
            &ProviderChoice {
                provider,
                model: None,
            },
        )
}

/// Recovery keeps the started attempt's provider, including an inherited choice pinned as null.
pub async fn owned_session_route(
    pool: &PgPool,
    bindings: AgentChannelBindings,
    agent_id: &str,
    routine_id: &str,
    ownership_token: &str,
) -> Result<ProviderRoute> {
    let choice: Option<ProviderChoice> = sqlx::query_as(
        "SELECT COALESCE(result_json ->> 'provider', run_spec ->> 'provider') AS provider,
                NULL::text AS model
         FROM routine_runs WHERE routine_id = $1 AND owned_tmux_session = $2
         ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .bind(routine_id)
    .bind(ownership_token)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow!("load routine {routine_id} owned provider: {error}"))?;
    match choice {
        Some(choice) => AgentProviderChannels::with_bindings(pool, agent_id, bindings)
            .await?
            .route(agent_id, &choice),
        None => session_route(pool, bindings, agent_id, routine_id, None).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels(
        provider: &str,
        id: Option<&str>,
        cc: Option<&str>,
        configured: Value,
    ) -> AgentProviderChannels {
        let bindings = AgentChannelBindings {
            provider: Some(provider.into()),
            discord_channel_id: id.map(str::to_string),
            discord_channel_cc: cc.map(str::to_string),
            ..AgentChannelBindings::default()
        };
        AgentProviderChannels {
            bindings,
            configured: Some(configured),
        }
    }

    #[test]
    fn a_chosen_provider_runs_only_on_a_channel_of_its_own() {
        let route = |agent: &AgentProviderChannels, provider: &str| {
            let choice = ProviderChoice {
                provider: Some(provider.into()),
                model: None,
            };
            agent
                .route("agent", &choice)
                .map(|route| (route.provider, route.channel))
        };
        // The Claude lookup falls back to the primary column, which here is the Antigravity channel.
        let antigravity = channels("antigravity", Some("9"), None, json!({}));
        assert!(route(&antigravity, "claude").is_err());
        let claude = channels("claude", None, Some("1"), json!({"antigravity": "3"}));
        assert_eq!(
            route(&claude, "antigravity").unwrap(),
            (ProviderKind::Antigravity, "3".into())
        );
        assert!(route(&claude, "gemini").is_err());
    }
}
