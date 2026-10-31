use super::*;
use crate::config::ClusterIntakeRoutingMode;

#[test]
fn input_fallback_defaults_off_and_invalid_pair_warns_only_when_enabled() {
    let enforce = IntakeRoutingMode::Enforce;
    let mut routing = ClusterIntakeRoutingConfig {
        enabled: true,
        mode: ClusterIntakeRoutingMode::Enforce,
        ..Default::default()
    };
    assert_eq!(
        InputFallback::resolve(&routing, enforce),
        (InputFallback::Off, None),
        "off must not warn about its pair"
    );
    routing.input_fallback_enabled = true;
    for pair in [
        vec![],
        vec!["mac-a".to_string()],
        vec!["mac-a".into(), " mac-a".into()],
        vec!["mac-a".into(), " ".into()],
    ] {
        routing.input_fallback_pair_instance_ids = pair;
        assert_eq!(
            InputFallback::resolve(&routing, enforce),
            (InputFallback::Off, Some("input_fallback_invalid_pair"))
        );
    }
    routing.input_fallback_pair_instance_ids = vec!["mac-a".into(), "mac-b".into()];
    routing.input_fallback_pilot_channel_ids = vec!["parent".into()];
    let pair = ["mac-a".to_string(), "mac-b".to_string()];
    let (pilot, warning) = InputFallback::resolve(&routing, enforce);
    assert_eq!(warning, None);
    assert_eq!(pilot.pair_for("thread", "parent"), Some(&pair));
    assert_eq!(pilot.pair_for("other", "other"), None);
    routing.input_fallback_scope = InputFallbackScope::Derived;
    let (derived, _) = InputFallback::resolve(&routing, enforce);
    assert_eq!(derived.pair_for("other", "other"), Some(&pair));
    assert_eq!(
        InputFallback::resolve(&routing, IntakeRoutingMode::Observe),
        (InputFallback::Off, Some("input_fallback_requires_enforce"))
    );
}
