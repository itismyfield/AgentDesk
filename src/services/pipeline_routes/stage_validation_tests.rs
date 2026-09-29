use super::*;
use serde_json::json;

fn stage(name: &str) -> PipelineStageInput {
    serde_json::from_value(json!({ "stage_name": name, "trigger_after": "review_pass" })).unwrap()
}

fn assert_bad_request(result: Result<(), PipelineRouteError>, field: &str) {
    match result.expect_err("unsupported stage save must fail closed") {
        PipelineRouteError::BadRequest { error, .. } => assert!(error.contains(field), "{error}"),
        other => panic!("expected bad request, got {other:?}"),
    }
}

#[test]
fn new_counter_stage_is_bad_request() {
    for provider in ["counter", " counter "] {
        let mut input = stage("qa");
        input.provider = Some(provider.into());
        input.agent_override_id = Some("reviewer-b".into());
        assert_bad_request(validate_supported_stage_changes(&[input], &[]), "counter");
    }
}

#[test]
fn new_stage_skip_condition_is_bad_request() {
    for skip in ["no_rs_changes", "future_condition"] {
        let mut input = stage("qa");
        input.skip_condition = Some(skip.into());
        assert_bad_request(
            validate_supported_stage_changes(&[input], &[]),
            "skip_condition",
        );
    }
}

#[test]
fn duplicate_effective_stage_order_is_bad_request() {
    for second_order in [Some(2), None] {
        let mut first = stage("lint");
        first.stage_order = Some(2);
        let mut second = stage("qa");
        second.stage_order = second_order;
        assert_bad_request(validate_pipeline_stages(&[first, second]), "stage_order");
    }
}

#[test]
fn existing_unsupported_values_can_be_saved_but_not_copied_or_changed() {
    let stored = [StoredStage {
        stage_name: Some("qa".into()),
        provider: Some("counter".into()),
        skip_condition: Some("no_rs_changes".into()),
        agent_override_id: Some("reviewer-a".into()),
        ..Default::default()
    }];
    let legacy = || {
        let mut input = stage("qa");
        input.provider = Some("counter".into());
        input.skip_condition = Some("no_rs_changes".into());
        input.agent_override_id = Some("reviewer-a".into());
        input
    };
    validate_supported_stage_changes(&[legacy()], &stored).unwrap();
    let mut renamed = legacy();
    renamed.stage_name = "new-qa".into();
    assert_bad_request(
        validate_supported_stage_changes(&[renamed], &stored),
        "counter",
    );
    let mut reassigned = legacy();
    reassigned.agent_override_id = Some("reviewer-b".into());
    assert_bad_request(
        validate_supported_stage_changes(&[reassigned], &stored),
        "counter",
    );
    let mut changed_skip = legacy();
    changed_skip.skip_condition = Some("future_condition".into());
    assert_bad_request(
        validate_supported_stage_changes(&[changed_skip], &stored),
        "skip_condition",
    );
    let mut cleared = legacy();
    cleared.provider = None;
    cleared.skip_condition = None;
    validate_supported_stage_changes(&[cleared], &stored).unwrap();
}

#[test]
fn supported_stages_with_distinct_orders_remain_valid() {
    let mut first = stage("lint");
    first.stage_order = Some(10);
    first.provider = Some("claude".into());
    first.agent_override_id = Some("reviewer-a".into());
    let mut second = stage("qa");
    second.skip_condition = Some(String::new());
    let stages = [first, second];
    validate_pipeline_stages(&stages).unwrap();
    validate_supported_stage_changes(&stages, &[]).unwrap();
}
