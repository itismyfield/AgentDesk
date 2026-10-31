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

/// #3089 B2b read-authority flag (`AGENTDESK_DELIVERY_RECORD_AUTHORITY`, OnceLock,
/// compiled default ON since #5071 T1 S8-2). When ON (default) the dedup gates
/// consult the durable `delivered_frontier` (fused with in-memory) so the
/// "already-relayed → skip" decision survives a restart / cross-actor boundary.
/// An explicit `=0` is the rollback: the gates then read the legacy in-memory
/// `committed_relay_offset` verbatim.
fn delivery_record_authority_resolution() -> FlagResolution {
    // #3933: a per-thread test override (see `authority_test_seam`) lets a unit
    // test drive a chosen authority state through the real save path WITHOUT
    // poisoning the env-global `OnceLock` cache for sibling tests. Since #5071 T1
    // S8-2 the compiled default is ON, so the seam's remaining jobs are (a) pinning
    // the legacy OFF path explicitly and (b) staying deterministic when a developer
    // shell exports its own `=0`/`=1`. Production strips this branch entirely
    // (`cfg(test)`), so the flag stays byte-identical at runtime.
    #[cfg(test)]
    if let Some(forced) = authority_test_seam::current_override() {
        // Reported as `EnvOverride` for the same reason as the shadow seam: the
        // value did not come from what this repository compiled in.
        return FlagResolution::env_override(forced);
    }
    static CACHED: OnceLock<FlagResolution> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let resolution = resolve_env_flag("AGENTDESK_DELIVERY_RECORD_AUTHORITY", true);
        if resolution.enabled {
            tracing::info!(target: "agentdesk::services::discord::outbound::delivery_record","  ✓ delivery_record_authority: enabled");
        }
        resolution
    })
}

pub(in crate::services::discord) fn delivery_record_authority_enabled() -> bool {
    delivery_record_authority_resolution().enabled
}

pub(in crate::services::discord::outbound) fn delivery_record_rollout_health_json()
-> serde_json::Value {
    delivery_record_rollout_health_json_for_flags(
        delivery_record_shadow_resolution(),
        delivery_record_authority_resolution(),
    )
}

pub(super) fn delivery_record_rollout_health_json_for_flags(
    shadow: super::FlagResolution,
    authority: super::FlagResolution,
) -> serde_json::Value {
    let shadow_enabled = shadow.enabled;
    let authority_enabled = authority.enabled;
    let mode = match (shadow_enabled, authority_enabled) {
        (false, false) => "off",
        (true, false) => "shadow_only",
        (false, true) => "authority_only",
        (true, true) => "shadow_and_authority",
    };
    let dedup_authority = if authority_enabled {
        "durable_delivery_record_frontier"
    } else {
        "in_memory_committed_offset"
    };
    let same_turn_backward_write_enforcement = if authority_enabled {
        "enforcing"
    } else {
        "observe_only"
    };
    let mut configuration_warnings = Vec::new();
    if !authority_enabled {
        configuration_warnings.push(serde_json::json!(
            "delivery_record_authority_disabled: durable frontiers are not the default committed-offset authority"
        ));
    }
    if authority_enabled && !shadow_enabled {
        configuration_warnings.push(serde_json::json!(
            "delivery_record_shadow_disabled: authority is enabled without shadow mirror telemetry"
        ));
    }
    let warning_count = configuration_warnings.len();
    serde_json::json!({
        "shadow_enabled": shadow_enabled,
        "authority_enabled": authority_enabled,
        "mode": mode,
        "dedup_authority": dedup_authority,
        "same_turn_backward_write_enforcement": same_turn_backward_write_enforcement,
        // Provenance, not value: `compiled_default` on both axes is the operational
        // rollout signal for #5262's "repo-owned" requirement. A non-Unicode env
        // value is the documented ambiguity because `std::env::var` maps it to this
        // branch; `env_override` means a Unicode value was read from the environment.
        "flag_source": {
            "shadow": shadow.source.as_str(),
            "authority": authority.source.as_str(),
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
