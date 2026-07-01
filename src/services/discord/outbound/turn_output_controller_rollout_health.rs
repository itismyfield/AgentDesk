//! #3794 turn-output controller rollout health (read-only, additive).
//!
//! Mirrors the #3746 `delivery_record_rollout` health slice
//! ([`super::delivery_record::delivery_record_rollout_health_json`]): a
//! side-effect-free JSON snapshot of the six `AGENTDESK_*_CONTROLLER` rollout
//! flags (#3089 A2b/A3/A4/A5/A6a/A6b) so an operator can see, per node, which
//! delivery authority is effective for each owner — the unified turn-output
//! controller vs the legacy per-owner delivery path.
//!
//! ## Why this reads the process env directly (not the `*_controller_enabled()`
//! getters)
//!
//! Each owner's `*_controller_enabled()` getter is an `OnceLock`-cached env read
//! that ALSO emits an `info!` telemetry line on its first (enabled) evaluation.
//! Reading those getters from the health path would (a) perturb their init
//! timing / telemetry ordering and (b) require widening the visibility of the
//! two most deeply-nested getters (`watcher_terminal` under
//! `tmux::tmux_watcher::terminal_send`, `turn_bridge_terminal` under
//! `turn_bridge::terminal_controller_cutover`), which are `#3016` finalize
//! hotfiles. So health instead takes a **side-effect-free** snapshot of the same
//! process env with the EXACT predicate the getters use ([`controller_flag_enabled`]).
//! The launchd env is written once at process start and is immutable for the
//! process lifetime, so this snapshot equals the effective state the
//! `OnceLock` getters observe. Purely observational — it touches no getter, no
//! `OnceLock`, and no delivery path.

/// The six turn-output controller rollout flags (#3089), each paired with the
/// owner label surfaced in the health JSON and the env var its getter reads.
/// Kept in lock-step with the getters:
/// - `sink_short_replace`     → `session_relay_sink::sink_short_replace_controller_enabled` (A2b)
/// - `standby_relay`          → `standby_relay::standby_relay_controller_enabled` (A3)
/// - `watcher_terminal`       → `tmux_watcher::terminal_send::watcher_terminal_controller_enabled` (A4)
/// - `turn_bridge_terminal`   → `turn_bridge::terminal_controller_cutover::turn_bridge_terminal_controller_enabled` (A5)
/// - `recovery_relay`         → `recovery_paths::controller_cutover::recovery_relay_controller_enabled` (A6a)
/// - `tui_prompt_relay`       → `tui_prompt_relay_controller_cutover::tui_prompt_relay_controller_enabled` (A6b)
const CONTROLLER_FLAGS: [(&str, &str); 6] = [
    (
        "sink_short_replace",
        "AGENTDESK_SINK_SHORT_REPLACE_CONTROLLER",
    ),
    ("standby_relay", "AGENTDESK_STANDBY_RELAY_CONTROLLER"),
    ("watcher_terminal", "AGENTDESK_WATCHER_TERMINAL_CONTROLLER"),
    (
        "turn_bridge_terminal",
        "AGENTDESK_TURN_BRIDGE_TERMINAL_CONTROLLER",
    ),
    ("recovery_relay", "AGENTDESK_RECOVERY_RELAY_CONTROLLER"),
    ("tui_prompt_relay", "AGENTDESK_TUI_PROMPT_RELAY_CONTROLLER"),
];

/// Parse one controller flag from the process env with the EXACT predicate the
/// six `*_controller_enabled()` getters use (`== "1"` / `== "true"`, trimmed,
/// ASCII-lowercased). Read-only: unlike the getters this does NOT touch their
/// `OnceLock` and never emits telemetry.
fn controller_flag_enabled(var: &str) -> bool {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .is_some_and(|v| v == "1" || v == "true")
}

/// Read-only health JSON for the turn-output controller rollout. Snapshots the
/// six controller env flags and delegates to the pure
/// [`turn_output_controller_rollout_health_json_for_flags`] renderer.
pub(super) fn turn_output_controller_rollout_health_json() -> serde_json::Value {
    let mut enabled = [false; CONTROLLER_FLAGS.len()];
    for (slot, (_, var)) in enabled.iter_mut().zip(CONTROLLER_FLAGS.iter()) {
        *slot = controller_flag_enabled(var);
    }
    turn_output_controller_rollout_health_json_for_flags(enabled)
}

/// Pure renderer (testable): given the effective on/off state of the six
/// controller owners, produce the rollout health object. Structurally
/// consistent with `delivery_record_rollout` — `owners`/`enabled_count`/
/// `effective_authority`/`configuration_warnings`/`warning_count`.
///
/// `effective_authority` classifies the node's delivery authority:
/// - `"controller"` — all six owners route the unified controller.
/// - `"legacy"`     — no owner routes the controller (compiled default; the env
///   rollback lever is engaged for every owner).
/// - `"mixed"`      — some owners route the controller, the rest still legacy.
fn turn_output_controller_rollout_health_json_for_flags(
    enabled: [bool; CONTROLLER_FLAGS.len()],
) -> serde_json::Value {
    let owner_count = CONTROLLER_FLAGS.len();
    let enabled_count = enabled.iter().filter(|on| **on).count();

    let effective_authority = if enabled_count == owner_count {
        "controller"
    } else if enabled_count == 0 {
        "legacy"
    } else {
        "mixed"
    };

    let owners: serde_json::Map<String, serde_json::Value> = CONTROLLER_FLAGS
        .iter()
        .zip(enabled.iter())
        .map(|((label, var), on)| {
            (
                (*label).to_string(),
                serde_json::json!({ "enabled": *on, "env_var": *var }),
            )
        })
        .collect();

    let mut configuration_warnings = Vec::new();
    if enabled_count == 0 {
        configuration_warnings.push(serde_json::json!(
            "turn_output_controller_all_disabled: every owner routes the legacy per-owner delivery path (compiled default; env rollback lever engaged)"
        ));
    } else if enabled_count < owner_count {
        configuration_warnings.push(serde_json::json!(format!(
            "turn_output_controller_partial_rollout: {enabled_count}/{owner_count} owners route the unified controller; the remainder route legacy"
        )));
    }
    let warning_count = configuration_warnings.len();

    serde_json::json!({
        "owners": owners,
        "owner_count": owner_count,
        "enabled_count": enabled_count,
        "effective_authority": effective_authority,
        "warning_count": warning_count,
        "configuration_warnings": configuration_warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_disabled_is_legacy_authority_with_observable_warning() {
        let json = turn_output_controller_rollout_health_json_for_flags([false; 6]);
        assert_eq!(json["enabled_count"], 0);
        assert_eq!(json["owner_count"], 6);
        assert_eq!(json["effective_authority"], "legacy");
        assert_eq!(json["warning_count"], 1);
        assert_eq!(json["owners"]["sink_short_replace"]["enabled"], false);
        assert_eq!(
            json["owners"]["sink_short_replace"]["env_var"],
            "AGENTDESK_SINK_SHORT_REPLACE_CONTROLLER"
        );
        assert!(
            json["configuration_warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap()
                    .starts_with("turn_output_controller_all_disabled"))
        );
    }

    #[test]
    fn all_enabled_is_controller_authority_with_no_warning() {
        let json = turn_output_controller_rollout_health_json_for_flags([true; 6]);
        assert_eq!(json["enabled_count"], 6);
        assert_eq!(json["effective_authority"], "controller");
        assert_eq!(json["warning_count"], 0);
        assert_eq!(json["configuration_warnings"].as_array().unwrap().len(), 0);
        for (label, _) in CONTROLLER_FLAGS {
            assert_eq!(json["owners"][label]["enabled"], true);
        }
    }

    #[test]
    fn partial_rollout_is_mixed_authority_and_warns_with_count() {
        // Enable only the first two owners (A2b sink, A3 standby).
        let json = turn_output_controller_rollout_health_json_for_flags([
            true, true, false, false, false, false,
        ]);
        assert_eq!(json["enabled_count"], 2);
        assert_eq!(json["effective_authority"], "mixed");
        assert_eq!(json["warning_count"], 1);
        assert_eq!(json["owners"]["standby_relay"]["enabled"], true);
        assert_eq!(json["owners"]["watcher_terminal"]["enabled"], false);
        let warning = json["configuration_warnings"][0].as_str().unwrap();
        assert!(warning.starts_with("turn_output_controller_partial_rollout"));
        assert!(warning.contains("2/6"));
    }

    #[test]
    fn every_owner_label_and_env_var_is_surfaced() {
        let json = turn_output_controller_rollout_health_json_for_flags([false; 6]);
        let owners = json["owners"].as_object().unwrap();
        assert_eq!(owners.len(), 6);
        for (label, var) in CONTROLLER_FLAGS {
            assert_eq!(owners[label]["env_var"], var);
        }
    }
}
