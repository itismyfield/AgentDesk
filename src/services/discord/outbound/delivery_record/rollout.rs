//! Delivery record rollout flags and health reporting.

use super::*;

// ---------------------------------------------------------------------------
// #3089 B1 / #4911 Phase A — persist confirmed delivery authority.
//
// A confirmed `Delivered` commit always persists the generation-scoped frontier
// and, when complete turn/destination identity is available, an exact receipt.
// The legacy shadow flag now controls only the durable-END divergence signal;
// read-authority rollout remains separately gated below.
// ---------------------------------------------------------------------------

/// #5071 T1 S8-1r2: where a rollout flag's value came from, which is a different
/// question from what the value is. `CompiledDefault` means `std::env::var` did not
/// yield a Unicode value (absent and non-Unicode values both take this path), so the
/// compiled value was used. `EnvOverride` means a Unicode value was present —
/// whatever it said — so a
/// node-local untracked `launchd.env`, not the repository, decided it. #5262's
/// completion contract asks for repo-owned flags, which is a claim about
/// provenance that `shadow_enabled`/`authority_enabled` alone cannot answer: once
/// the compiled default matches the pin, the two are indistinguishable by value.
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

/// A rollout flag's resolved value paired with its provenance.
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

/// The opt-in reading: absent → compiled default OFF, present → `EnvOverride`
/// whose value is ON only for `1`/`true` (trimmed, case-insensitive). Provenance
/// is decided by presence ALONE, so an explicit `=0` still reports `EnvOverride`
/// — that is the whole point of the distinction, since a pin that agrees with the
/// compiled default is otherwise invisible. Only the shadow flag still reads
/// this: #5071 T1 S8-2 moved the authority axis to a compiled default ON, which
/// is exactly the per-flag divergence the two named wrappers below were split to
/// allow.
fn resolve_opt_in_flag(raw: Option<&str>) -> FlagResolution {
    match raw {
        Some(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            FlagResolution::env_override(normalized == "1" || normalized == "true")
        }
        None => FlagResolution::compiled_default(false),
    }
}

/// Pure resolver for `AGENTDESK_DELIVERY_RECORD_SHADOW` (compiled default OFF).
pub(super) fn resolve_shadow_flag(raw: Option<&str>) -> FlagResolution {
    resolve_opt_in_flag(raw)
}

/// Pure resolver for `AGENTDESK_DELIVERY_RECORD_AUTHORITY` (compiled default ON
/// since #5071 T1 S8-2). Value parsing is identical to [`resolve_opt_in_flag`] —
/// a present value is ON only for `1`/`true` (trimmed, case-insensitive) and is
/// ALWAYS `EnvOverride`, including an explicit `=0` — but ABSENCE now resolves to
/// `compiled_default(true)`. That is the promotion: the durable delivered
/// frontier is the repo-owned dedup authority (#5262), so a node needs no
/// `launchd.env` pin to get it, and `=0` is the rollback rather than the pin
/// being the rollout.
pub(super) fn resolve_authority_flag(raw: Option<&str>) -> FlagResolution {
    match raw {
        Some(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            FlagResolution::env_override(normalized == "1" || normalized == "true")
        }
        None => FlagResolution::compiled_default(true),
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
        let resolution = resolve_shadow_flag(
            std::env::var("AGENTDESK_DELIVERY_RECORD_SHADOW")
                .ok()
                .as_deref(),
        );
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
        let resolution = resolve_authority_flag(
            std::env::var("AGENTDESK_DELIVERY_RECORD_AUTHORITY")
                .ok()
                .as_deref(),
        );
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
