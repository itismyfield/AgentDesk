//! Account selection for cached rate-limit pressure. Config reads happen at refresh.
use super::*;
static AGENT_FALLBACKS: OnceLock<RwLock<HashMap<String, Vec<String>>>> = OnceLock::new();
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
    let catalog = crate::services::discord::org_schema::provider_auth_catalog();
    let policies = crate::services::discord::org_schema::provider_auth_fallback_policies();
    let mut alternatives = HashMap::new();
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
        let candidates = policies
            .get(provider.as_str())
            .cloned()
            .unwrap_or_default()
            .candidates(&provider, &profile, &catalog)
            .into_iter()
            .skip(1)
            .filter(|id| {
                crate::services::provider_auth_profile::resolve(
                    provider.clone(),
                    Some(id),
                    None,
                    &catalog,
                )
                .is_ok()
            })
            .map(|id| account_key(provider.as_str(), &id))
            .collect();
        alternatives.insert(agent_id.clone(), candidates);
        snapshot.insert(agent_id.clone(), account_key(provider.as_str(), &profile));
    }
    *AGENT_FALLBACKS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|p| p.into_inner()) = alternatives;
    *AGENT_PROFILE
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|p| p.into_inner()) = snapshot;
}
pub(super) fn clear_profiles() {
    AGENT_FALLBACKS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
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
/// If the primary is exhausted, allow dispatch to a healthy alternate. Spawn
/// resolves the same account list again; unknown usage does not mean exhausted.
pub(super) fn evaluate_with_fallbacks(
    provider: &str,
    primary_key: &str,
    alternatives: &[String],
    map: &HashMap<String, ProviderPressureSnapshot>,
    danger: u64,
    stale: i64,
    now: i64,
) -> ProviderPressureDecision {
    let primary = evaluate_provider_pressure(provider, map.get(primary_key), danger, stale, now);
    if primary.verdict.is_defer()
        && evaluate_provider_pressure(provider, map.get(primary_key), 100, stale, now)
            .verdict
            .is_defer()
    {
        for key in alternatives {
            let alternate = evaluate_provider_pressure(provider, map.get(key), danger, stale, now);
            if !alternate.verdict.is_defer() {
                return alternate;
            }
        }
    }
    primary
}

pub(super) fn agent_fallback_keys(agent_id: &str) -> Vec<String> {
    AGENT_FALLBACKS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(agent_id)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhausted_primary_can_dispatch_to_backup_but_missing_or_exhausted_candidates_do_not_bypass_gate()
     {
        let pressure = pressure_snapshot_from_payloads(&[
            serde_json::json!({"provider":"codex", "profile_id":"default", "fetched_at":100, "buckets":[{"limit":100,"used":100,"reset":200}]}),
            serde_json::json!({"provider":"codex", "profile_id":"full", "fetched_at":100, "buckets":[{"limit":100,"used":100,"reset":200}]}),
            serde_json::json!({"provider":"codex", "profile_id":"ready", "fetched_at":100, "buckets":[{"limit":100,"used":10,"reset":200}]}),
        ]);
        let evaluate = |keys: &[String]| {
            evaluate_with_fallbacks("codex", "codex", keys, &pressure, 90, 600, 101).verdict
        };
        assert!(evaluate(&[]).is_defer());
        assert!(evaluate(&["codex:full".into()]).is_defer());
        assert!(!evaluate(&["codex:full".into(), "codex:ready".into()]).is_defer());
        assert!(!evaluate(&["codex:unknown-usage".into()]).is_defer());
    }

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
