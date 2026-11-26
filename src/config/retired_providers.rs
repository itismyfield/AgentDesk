//! Config entries that still name a removed provider. They load unchanged but never run.

use super::Config;
use crate::domain::provider::retired_provider_id;

fn names_retired(raw: Option<&str>) -> bool {
    raw.and_then(retired_provider_id).is_some()
}

/// Config paths naming a retired provider, sorted so the warning text is stable.
pub(crate) fn retired_provider_refs(config: &Config) -> Vec<String> {
    let mut refs: Vec<String> = config
        .providers
        .keys()
        .filter(|key| names_retired(Some(key.as_str())))
        .map(|key| format!("providers.{key}"))
        .collect();
    for agent in &config.agents {
        if names_retired(Some(agent.provider.as_str())) {
            refs.push(format!("agents.{}.provider", agent.id));
        }
        for (key, channel) in agent.channels.iter() {
            if names_retired(Some(key)) || names_retired(channel.provider().as_deref()) {
                refs.push(format!("agents.{}.channels.{key}", agent.id));
            }
        }
    }
    for (name, bot) in &config.discord.bots {
        if names_retired(bot.provider.as_deref()) {
            refs.push(format!("discord.bots.{name}.provider"));
        }
    }
    if names_retired(config.onboarding.default_provider.as_deref()) {
        refs.push("onboarding.default_provider".to_string());
    }
    for (suffix, provider) in &config.onboarding.provider_suffix_map {
        if names_retired(provider.as_deref()) {
            refs.push(format!("onboarding.provider_suffix_map.{suffix}"));
        }
    }
    refs.sort();
    refs
}

/// Boot calls this once: a single warning lists every retired reference instead of failing boot.
pub fn warn_retired_provider_refs(config: &Config) {
    let refs = retired_provider_refs(config);
    if !refs.is_empty() {
        tracing::warn!(
            references = %refs.join(", "),
            "config names a retired provider; these entries are ignored and their bindings do not run"
        );
    }
}

#[cfg(test)]
#[path = "retired_providers_tests.rs"]
mod tests;
