//! Input fallback selection settings; off stops new selection only.
use super::intake_routing_config::{IntakeRoutingMode, effective_intake_routing_config};
use crate::config::ClusterIntakeRoutingConfig;
use crate::config::runtime_profile::InputFallbackScope;

/// Stored custody, fences, deferred cleanup and undelivered events keep resolving while off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputFallback {
    Off,
    Active {
        scope: InputFallbackScope,
        pilot: Vec<String>,
        pair: [String; 2],
    },
}

impl InputFallback {
    pub(crate) fn resolve(
        config: &ClusterIntakeRoutingConfig,
        mode: IntakeRoutingMode,
    ) -> (Self, Option<&'static str>) {
        if !config.input_fallback_enabled {
            return (Self::Off, None);
        }
        let pair: Vec<String> = config
            .input_fallback_pair_instance_ids
            .iter()
            .map(|id| id.trim().to_owned())
            .collect();
        let [a, b] = pair.as_slice() else {
            return (Self::Off, Some("input_fallback_invalid_pair"));
        };
        if a.is_empty() || b.is_empty() || a == b {
            return (Self::Off, Some("input_fallback_invalid_pair"));
        }
        if mode != IntakeRoutingMode::Enforce {
            return (Self::Off, Some("input_fallback_requires_enforce"));
        }
        let pilot = config
            .input_fallback_pilot_channel_ids
            .iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        let active = Self::Active {
            scope: config.input_fallback_scope,
            pilot,
            pair: [a.clone(), b.clone()],
        };
        (active, None)
    }

    /// A thread follows its policy parent; the raw channel identity is never replaced.
    pub(crate) fn pair_for(&self, raw_channel: &str, policy_channel: &str) -> Option<&[String; 2]> {
        let Self::Active { scope, pilot, pair } = self else {
            return None;
        };
        let included = *scope == InputFallbackScope::Derived
            || pilot
                .iter()
                .any(|id| id == raw_channel || id == policy_channel);
        included.then_some(pair)
    }
}

/// An unloaded config is treated as off; the routing snapshot already reports that state.
pub(crate) fn current() -> (InputFallback, Option<&'static str>) {
    let Some(config) = crate::config::snapshot() else {
        return (InputFallback::Off, None);
    };
    InputFallback::resolve(
        &config.cluster.intake_routing,
        effective_intake_routing_config().mode,
    )
}

pub(crate) fn status_json() -> serde_json::Value {
    let (fallback, warning) = current();
    match fallback {
        InputFallback::Off => serde_json::json!({"state": "off", "warning": warning}),
        InputFallback::Active { scope, pilot, .. } => serde_json::json!({
            "state": "active",
            "scope": match scope {
                InputFallbackScope::Pilot => "pilot",
                InputFallbackScope::Derived => "derived",
            },
            "pilot_size": pilot.len(),
            "warning": warning,
        }),
    }
}

#[cfg(test)]
#[path = "input_fallback_config_tests.rs"]
mod tests;
