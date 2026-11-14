//! Runtime auth projections retain the org loader's recovery registration.
use super::fallback::FallbackPolicy;
use crate::domain::provider::ProviderKind;
use crate::domain::provider_auth_profile::{ProfileBinding, ProviderAuthProfileDef};
use crate::services::org_catalog::{
    AuthCatalogSnapshot, PrimaryProfileMap, ProfileBindingInput, configured_auth_profile,
    load_runtime_auth_catalog, provider_primary_profile,
};
use std::collections::HashMap;

pub(crate) fn provider_auth_catalog() -> HashMap<String, ProviderAuthProfileDef> {
    load_runtime_auth_catalog()
        .map(|snapshot| snapshot.profiles)
        .unwrap_or_default()
}

pub(crate) fn provider_auth_primary_profiles() -> PrimaryProfileMap {
    load_runtime_auth_catalog()
        .map(|snapshot| snapshot.primary_profiles)
        .unwrap_or_default()
}

pub(crate) fn provider_auth_fallback_policies() -> HashMap<String, FallbackPolicy> {
    load_runtime_auth_catalog()
        .map(|snapshot| snapshot.fallback_policies)
        .unwrap_or_default()
}

pub(crate) fn list_profile_bindings() -> Vec<ProfileBinding> {
    load_runtime_auth_catalog()
        .map(list_profile_bindings_from_snapshot)
        .unwrap_or_default()
}

pub(crate) fn list_profile_bindings_from_snapshot(
    snapshot: AuthCatalogSnapshot,
) -> Vec<ProfileBinding> {
    snapshot
        .bindings
        .into_iter()
        .map(|input: ProfileBindingInput| {
            let provider = input
                .provider
                .as_deref()
                .and_then(ProviderKind::from_str)
                .map(|kind| kind.as_str().to_string())
                .unwrap_or_else(|| {
                    if input.channel_id.is_some() {
                        "unknown".to_string()
                    } else {
                        input
                            .provider
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string())
                    }
                });
            let explicit = configured_auth_profile(
                input.channel_profile.as_deref(),
                input.agent_profile.as_deref(),
            );
            let profile_id = explicit.map(str::to_string).unwrap_or_else(|| {
                provider_primary_profile(
                    Some(&snapshot.primary_profiles),
                    input.provider.as_deref(),
                )
            });
            ProfileBinding {
                agent_id: input.agent_id,
                provider,
                profile_id,
                channel_id: input.channel_id,
                is_explicit: explicit.is_some(),
            }
        })
        .collect()
}
