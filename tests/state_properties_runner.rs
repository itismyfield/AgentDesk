//! Runs the stateful property suite against the library built without the rest
//! of the unit-test graph. Enable it with `--features state_properties`.

#[test]
fn run_state_property_models() {
    #[cfg(feature = "state_properties")]
    agentdesk::run_state_property_tests();
    #[cfg(not(feature = "state_properties"))]
    eprintln!("state_properties feature is off; suite not executed");
}
