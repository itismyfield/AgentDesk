//! Account selection for cached rate-limit pressure. Config reads happen at refresh.
use super::*;
static AGENT_PROFILE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
pub(super) fn account_key(provider: &str, profile_id: &str) -> String {
    if profile_id.is_empty() || profile_id == "default" {
        provider.to_string()
    } else {
        format!("{provider}:{profile_id}")
    }
}
pub(super) fn refresh_profiles(
    bindings: &std::collections::BTreeMap<String, crate::db::agents::AgentChannelBindings>,
) {
    let profiles = crate::services::discord::org_schema::list_profile_bindings();
    let primary = crate::services::discord::org_schema::provider_auth_primary_profiles();
    let mut snapshot = HashMap::new();
    for (agent_id, binding) in bindings {
        let Some(provider) = binding.resolved_primary_provider_kind() else {
            continue;
        };
        let channel = binding.primary_channel();
        let profile = profiles
            .iter()
            .filter(|entry| entry.provider == provider.as_str())
            .find(|entry| channel.is_some() && entry.channel_id == channel)
            .or_else(|| {
                profiles.iter().find(|entry| {
                    entry.agent_id == *agent_id
                        && entry.channel_id.is_none()
                        && entry.provider == provider.as_str()
                })
            })
            .map(|entry| entry.profile_id.clone())
            .or_else(|| primary.get(provider.as_str()).cloned())
            .unwrap_or_else(|| "default".to_string());
        snapshot.insert(agent_id.clone(), account_key(provider.as_str(), &profile));
    }
    *AGENT_PROFILE
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|p| p.into_inner()) = snapshot;
}
pub(super) fn clear_profiles() {
    AGENT_PROFILE
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}
pub(super) fn agent_account_key(agent_id: &str, provider: &str) -> String {
    AGENT_PROFILE
        .get_or_init(|| RwLock::new(HashMap::new()))
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(agent_id)
        .filter(|key| *key == provider || key.starts_with(&format!("{provider}:")))
        .cloned()
        .unwrap_or_else(|| provider.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_account_pressure_does_not_replace_default_pressure() {
        let payloads = vec![
            serde_json::json!({"provider":"codex", "profile_id":"default", "fetched_at":100, "buckets":[{"limit":100,"used":20,"reset":200}]}),
            serde_json::json!({"provider":"codex", "profile_id":"work", "fetched_at":100, "buckets":[{"limit":100,"used":100,"reset":200}]}),
        ];
        let pressure = pressure_snapshot_from_payloads(&payloads);
        assert_eq!(pressure.len(), 2);
        assert!(
            !evaluate_provider_pressure("codex", pressure.get("codex"), 100, 600, 101)
                .verdict
                .is_defer()
        );
        assert!(
            evaluate_provider_pressure("codex", pressure.get("codex:work"), 100, 600, 101)
                .verdict
                .is_defer()
        );
    }
}
