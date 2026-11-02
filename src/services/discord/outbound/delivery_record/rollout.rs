//! Delivery record rollout flags and health reporting.

use super::*;

/// The legacy health labels distinguish a Unicode override from the compiled default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum FlagSource {
    CompiledDefault,
    EnvOverride,
}

impl FlagSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::CompiledDefault => "compiled_default",
            Self::EnvOverride => "env_override",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct FlagResolution {
    pub(in crate::services::discord) enabled: bool,
    pub(in crate::services::discord) source: super::FlagSource,
}

impl FlagResolution {
    pub(super) const fn compiled_default(enabled: bool) -> Self {
        Self {
            enabled,
            source: FlagSource::CompiledDefault,
        }
    }

    pub(super) const fn env_override(enabled: bool) -> Self {
        Self {
            enabled,
            source: FlagSource::EnvOverride,
        }
    }
}

pub(super) fn resolve_env_flag(name: &str, default: bool) -> FlagResolution {
    let resolved = crate::services::settings::resolve_env_flag(name, default);
    if resolved.source == crate::services::settings::SettingSource::EnvOverride {
        FlagResolution::env_override(resolved.value)
    } else {
        FlagResolution::compiled_default(resolved.value)
    }
}

/// #3089 B1 shadow-write flag (`AGENTDESK_DELIVERY_RECORD_SHADOW`, OnceLock,
/// default OFF), with its provenance. Divergence telemetry only; confirmed
/// persistence never consults this flag.
fn delivery_record_shadow_resolution() -> FlagResolution {
    #[cfg(test)]
    if let Some(forced) = shadow_test_seam::current_override() {
        // The seam forces a value the environment did not supply, so it reports
        // `EnvOverride` rather than claiming the repository compiled it in.
        // Provenance assertions therefore test `resolve_shadow_flag` directly.
        return FlagResolution::env_override(forced);
    }
    static CACHED: OnceLock<FlagResolution> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let resolution = resolve_env_flag("AGENTDESK_DELIVERY_RECORD_SHADOW", false);
        if resolution.enabled {
            tracing::info!(target: "agentdesk::services::discord::outbound::delivery_record","  ✓ delivery_record_shadow: enabled");
        }
        resolution
    })
}

pub(in crate::services::discord) fn delivery_record_shadow_enabled() -> bool {
    delivery_record_shadow_resolution().enabled
}

pub(in crate::services::discord::outbound) fn delivery_record_rollout_health_json()
-> serde_json::Value {
    delivery_record_rollout_health_json_for_flags(delivery_record_shadow_resolution())
}

/// The durable frontier is the only dedup authority, so the authority fields are
/// fixed; they stay in the health shape for existing dashboards and scripts.
pub(super) fn delivery_record_rollout_health_json_for_flags(
    shadow: super::FlagResolution,
) -> serde_json::Value {
    let shadow_enabled = shadow.enabled;
    let mode = if shadow_enabled {
        "shadow_and_authority"
    } else {
        "authority_only"
    };
    let mut configuration_warnings = Vec::new();
    if !shadow_enabled {
        configuration_warnings.push(serde_json::json!(
            "delivery_record_shadow_disabled: authority is enabled without shadow mirror telemetry"
        ));
    }
    let warning_count = configuration_warnings.len();
    serde_json::json!({
        "shadow_enabled": shadow_enabled,
        "authority_enabled": true,
        "mode": mode,
        "dedup_authority": "durable_delivery_record_frontier",
        "same_turn_backward_write_enforcement": "enforcing",
        // Provenance, not value: `compiled_default` on both axes is the operational
        // rollout signal for #5262's "repo-owned" requirement. A non-Unicode env
        // value is the documented ambiguity because `std::env::var` maps it to this
        // branch; `env_override` means a Unicode value was read from the environment.
        "flag_source": {
            "shadow": shadow.source.as_str(),
            "authority": FlagSource::CompiledDefault.as_str(),
        },
        "warning_count": warning_count,
        "configuration_warnings": configuration_warnings,
    })
}

/// Only `Delivered` permits the confirmed-delivery funnel to advance durable offsets.
pub(in crate::services::discord) fn outcome_is_shadow_delivered(outcome: &DeliveryOutcome) -> bool {
    matches!(outcome, DeliveryOutcome::Delivered { .. })
}

/// Divergence-telemetry gate. Confirmed frontier/receipt persistence does not
/// consult this flag; only the observe-only comparison does.
pub(super) fn should_shadow_mirror(is_delivered: bool, enabled: bool) -> bool {
    is_delivered && enabled
}

/// M4 divergence predicate (pure, testable): the durable frontier END must equal
/// the in-memory `confirmed_end_offset` just advanced. END is the risky datum.
pub(super) fn delivered_frontier_end_diverged(
    durable_end: u64,
    in_memory_confirmed_end: u64,
) -> bool {
    durable_end != in_memory_confirmed_end
}
