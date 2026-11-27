use super::{is_five_min_policy_tick, should_run_slo_api_friction_aggregation};

#[test]
fn slo_api_friction_aggregation_runs_while_leader_epoch_is_pending() {
    assert!(should_run_slo_api_friction_aggregation(0, true));
    assert!(should_run_slo_api_friction_aggregation(1, true));
    assert!(should_run_slo_api_friction_aggregation(9, true));
    assert!(should_run_slo_api_friction_aggregation(10, true));
}

#[test]
fn slo_api_friction_aggregation_falls_back_to_five_min_after_kickstart() {
    assert!(!should_run_slo_api_friction_aggregation(0, false));
    assert!(!should_run_slo_api_friction_aggregation(1, false));
    assert!(!should_run_slo_api_friction_aggregation(9, false));
    assert!(should_run_slo_api_friction_aggregation(10, false));
    assert!(should_run_slo_api_friction_aggregation(20, false));
}

#[test]
fn five_min_policy_tick_runs_on_every_tenth_nonzero_iteration() {
    assert!(!is_five_min_policy_tick(0));
    assert!(!is_five_min_policy_tick(1));
    assert!(!is_five_min_policy_tick(9));
    assert!(is_five_min_policy_tick(10));
    assert!(is_five_min_policy_tick(20));
}
