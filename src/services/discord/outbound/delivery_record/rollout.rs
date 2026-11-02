//! Delivery record health reporting and the confirmed-delivery outcome gate.

use super::*;

/// The durable frontier is the only dedup authority and there is no rollout
/// switch left; the block keeps its shape for existing dashboards and scripts.
pub(in crate::services::discord::outbound) fn delivery_record_rollout_health_json()
-> serde_json::Value {
    serde_json::json!({
        "shadow_enabled": false,
        "authority_enabled": true,
        "mode": "authority_only",
        "dedup_authority": "durable_delivery_record_frontier",
        "same_turn_backward_write_enforcement": "enforcing",
        "flag_source": {
            "shadow": "compiled_default",
            "authority": "compiled_default",
        },
        "warning_count": 0,
        "configuration_warnings": [],
    })
}

/// Only `Delivered` permits the confirmed-delivery funnel to advance durable offsets.
pub(in crate::services::discord) fn outcome_is_shadow_delivered(outcome: &DeliveryOutcome) -> bool {
    matches!(outcome, DeliveryOutcome::Delivered { .. })
}
