//! Account selection belongs to the durable lease, not mutable channel routing.
use super::*;

pub(crate) fn pinned_auth_profile(
    channel: &str,
    provider: &ProviderKind,
    agent: Option<&str>,
) -> Result<Option<String>, String> {
    let runtime = durable::lock(&durable::coordinator().runtime);
    let Some(state) = runtime
        .states
        .get(channel)
        .filter(|state| state.lock_held())
    else {
        return Ok(None);
    };
    profile_for_writer(state, provider, agent)
}

fn profile_for_writer(
    state: &ChannelState,
    provider: &ProviderKind,
    agent: Option<&str>,
) -> Result<Option<String>, String> {
    let context = state
        .context
        .as_ref()
        .ok_or("recovery account context missing")?;
    if context
        .provider(state, &state.active_writer_agent_id)
        .as_ref()
        != Some(provider)
        || agent.is_some_and(|agent| agent != state.active_writer_agent_id)
        || (agent.is_none() && context.owner_provider == context.fallback_provider)
    {
        return Err("provider launch does not own the recovery account".into());
    }
    Ok(if state.active_writer_agent_id == state.owner_agent_id {
        context.owner_auth_profile.clone()
    } else {
        context.fallback_auth_profile.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHANNEL, enabled_runtime};
    use super::*;
    #[test]
    fn account_pin_survives_catalog_removal_and_switches_only_on_restore() {
        let mut runtime = enabled_runtime();
        runtime.catalog.agents.get_mut("claude").unwrap().provider = Some("codex".into());
        runtime
            .catalog
            .agents
            .get_mut("monitoring")
            .unwrap()
            .auth_profile = "backup-account".into();
        let binding = runtime.catalog.channels.get_mut(CHANNEL).unwrap();
        binding.owner_provider = ProviderKind::Codex;
        binding.owner_auth_profile = "channel-account".into();
        runtime.observe(ObserveInput {
            channel_id: CHANNEL.into(),
            primary_turn_id: "turn".into(),
            signal: DetectorSignal::TurnRateLimit,
        });
        runtime.clear_catalog();
        let state = &runtime.states[CHANNEL];
        assert_eq!(
            profile_for_writer(state, &ProviderKind::Codex, Some("monitoring"))
                .unwrap()
                .as_deref(),
            Some("backup-account")
        );
        assert!(profile_for_writer(state, &ProviderKind::Codex, Some("claude")).is_err());
        assert!(profile_for_writer(state, &ProviderKind::Codex, None).is_err());
        runtime.restore_owner(CHANNEL, true, "done").unwrap();
        assert_eq!(
            profile_for_writer(
                &runtime.states[CHANNEL],
                &ProviderKind::Codex,
                Some("claude")
            )
            .unwrap()
            .as_deref(),
            Some("channel-account")
        );
    }
}
