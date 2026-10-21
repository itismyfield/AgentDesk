use super::recovery_context_take_order_tests::source_guard_tests::{
    INTAKE_TURN, RUNTIME_TRANSITION, transition_provider_gaps,
};

#[test]
fn c1b_off_role_override_keeps_runtime_transition_on_original_provider() {
    let gaps = transition_provider_gaps(INTAKE_TURN, RUNTIME_TRANSITION);
    assert_eq!(gaps, Vec::<String>::new());
}
