use serde_json::{Value, json};

use super::*;
use crate::domain::campaign::{
    definition::{DefinitionErrorCode, DefinitionInput, ValidatedDefinition, validate_definition},
    pipeline_fields::{ConditionSource, ConditionType, Freshness, SubjectBinding},
};

fn graph_wire() -> Value {
    json!({
        "schema_version": 1, "vocabulary_version": 1, "evaluator_version": 1, "registry_version": 1,
        "start": "check",
        "states": [
            {"id": "work", "kind": "agent_task", "owner": "implementation",
             "title": "Implement", "executor": {"id": "r8.work", "version": 1},
             "instructions": "Follow the requirements",
             "timeout": {"after_seconds": 120, "transition_id": "expired"}},
            {"id": "decision", "kind": "human_decision"},
            {"id": "check", "kind": "auto_check", "executor": {"id": "r8.requirements", "version": 1}},
            {"id": "landing", "kind": "auto_action", "executor": {
                "id": "r8.landing", "version": 1, "parameters": {"effect_gates": ["merge", "deploy"]}}},
            {"id": "wait", "kind": "wait"},
            {"id": "done", "kind": "end_success"},
            {"id": "failed", "kind": "end_failure"}
        ],
        "transitions": [
            {"id": "proceed", "from": "work", "to": "landing", "priority": 20,
             "condition": {"all": [
                 {"field": "state.outcome", "op": "eq", "value": "succeeded"},
                 {"all": [
                     {"field": "work.seal_valid", "op": "eq", "value": true},
                     {"field": "verification.review_start_passed", "op": "eq", "value": true}
                 ]}
             ]}, "gate": "landing"},
            {"id": "expired", "from": "work", "to": "failed", "priority": 0,
             "condition": {"field": "timer.expired", "op": "eq", "value": true}},
            {"id": "repair", "from": "work", "to": "work", "priority": 10,
             "condition": {"field": "state.outcome", "op": "eq", "value": "quality_failed"},
             "retry": {"budget_id": "quality_repair", "max": 3, "then_to": "failed"}}
        ],
        "events": [
            {"id": "ready", "state_id": "check", "type": "dependency.satisfied"},
            {"id": "sealed", "state_id": "work", "type": "evidence.accepted"}
        ],
        "policy": {"design_enabled": false},
        "template_id": "campaign-development-9", "template_version": 1
    })
}

fn graph(input: Value) -> PipelineDefinition {
    serde_json::from_value(input).unwrap()
}

fn hash(input: Value) -> Digest {
    graph(input).execution_hash().unwrap()
}

fn legacy_wire() -> Value {
    json!({
        "repository": "adk",
        "title": "Campaign",
        "goal": "Keep the old revision readable",
        "profile_version": "v1",
        "nodes": [{
            "id": "a",
            "title": "Implement",
            "issue_url": "https://github.com/itismyfield/AgentDesk/issues/1",
            "acceptance": [{"id": "test", "description": "Targeted tests", "verification": "test"}],
            "dependencies": [],
            "group": null
        }]
    })
}

#[test]
fn pipeline_kinds_and_typed_conditions_round_trip() {
    let original = graph(graph_wire());
    assert_eq!(
        original
            .states
            .iter()
            .map(|state| state.kind)
            .collect::<Vec<_>>(),
        [
            PipelineKind::AgentTask,
            PipelineKind::HumanDecision,
            PipelineKind::AutoCheck,
            PipelineKind::AutoAction,
            PipelineKind::Wait,
            PipelineKind::EndSuccess,
            PipelineKind::EndFailure,
        ]
    );
    let encoded = serde_json::to_value(&original).unwrap();
    assert_eq!(graph(encoded), original);
    let unknown_kind = json!({"id": "x", "kind": "script"});
    assert!(serde_json::from_value::<PipelineState>(unknown_kind).is_err());
    let mut sparse = graph_wire();
    for key in ["events", "policy"] {
        sparse.as_object_mut().unwrap().remove(key);
    }
    let sparse = graph(sparse);
    assert!(sparse.events.is_empty());
    assert_eq!(sparse.policy, PipelinePolicy::default());
    for (field, value, expected) in [
        (
            "work.seal_valid",
            json!(true),
            ConditionValue::Boolean(true),
        ),
        ("review.p0_count", json!(0), ConditionValue::Integer(0)),
        (
            "review.verdict",
            json!("clean"),
            ConditionValue::String("clean".into()),
        ),
    ] {
        let condition =
            serde_json::from_value(json!({"field":field,"op":"eq","value":value})).unwrap();
        let PipelineCondition::Compare(comparison) = &condition else {
            panic!("expected a comparison");
        };
        assert_eq!(comparison.value, expected);
        assert_eq!(serde_json::to_value(condition).unwrap()["value"], value);
    }
    for value in [json!(null), json!(1.5), json!({}), json!([])] {
        assert!(
            serde_json::from_value::<PipelineCondition>(
                json!({"field":"review.p0_count","op":"eq","value":value})
            )
            .is_err()
        );
    }
    for invalid in [
        json!({"field":"shell.command","op":"eq","value":true}),
        json!({"field":"work.seal_valid","op":"regex","value":"true"}),
        json!({"all":[],"field":"work.seal_valid","op":"eq","value":true}),
    ] {
        assert!(serde_json::from_value::<PipelineCondition>(invalid).is_err());
    }
}

#[test]
fn pipeline_execution_hash_normalizes_order() {
    let wire = graph_wire();
    let expected = hash(wire.clone());

    for path in [
        "/states",
        "/transitions",
        "/events",
        "/transitions/0/condition/all",
        "/transitions/0/condition/all/1/all",
        "/states/3/executor/parameters/effect_gates",
    ] {
        let mut reordered = wire.clone();
        reordered
            .pointer_mut(path)
            .unwrap()
            .as_array_mut()
            .unwrap()
            .reverse();
        assert_eq!(expected, hash(reordered), "{path}");
    }
}

#[test]
fn pipeline_execution_hash_excludes_display_and_layout() {
    let wire = graph_wire();
    let expected = hash(wire.clone());
    let mut edited = wire.clone();
    edited["states"][0]["title"] = json!("Only visible text");
    edited["template_id"] = json!("copied");
    edited["template_version"] = json!(2);
    assert_eq!(expected, hash(edited));

    let mut editor = json!({"pipeline":wire, "layout":{"work":{"x":11,"y":20}}});
    let before = hash(editor["pipeline"].clone());
    editor["layout"]["work"] = json!({"x":99,"y":30});
    assert_eq!(before, hash(editor["pipeline"].clone()));

    editor["pipeline"]["layout"] = editor["layout"].clone();
    assert!(serde_json::from_value::<PipelineDefinition>(editor["pipeline"].clone()).is_err());
}

#[test]
fn pipeline_execution_hash_changes_with_execution_inputs() {
    let wire = graph_wire();
    let expected = hash(wire.clone());
    for (path, value) in [
        ("/schema_version", json!(2)),
        ("/vocabulary_version", json!(2)),
        ("/evaluator_version", json!(2)),
        ("/registry_version", json!(2)),
        ("/start", json!("work")),
        ("/states/0/id", json!("custom_work")),
        ("/states/0/kind", json!("wait")),
        ("/states/0/owner", json!("review")),
        ("/states/0/instructions", json!("Use a changed requirement")),
        ("/states/0/timeout/after_seconds", json!(121)),
        ("/states/0/timeout/transition_id", json!("proceed")),
        ("/states/0/executor/id", json!("custom.executor")),
        ("/states/0/executor/version", json!(2)),
        (
            "/states/3/executor/parameters/effect_gates",
            json!(["merge"]),
        ),
        ("/transitions/0/priority", json!(21)),
        ("/transitions/0/id", json!("changed_transition_id")),
        ("/transitions/0/from", json!("wait")),
        ("/transitions/0/to", json!("done")),
        ("/transitions/0/condition/all/0/value", json!("failed")),
        ("/transitions/0/gate", json!("review")),
        ("/transitions/2/retry/max", json!(0)),
        ("/transitions/2/retry/budget_id", json!("changed_budget")),
        ("/transitions/2/retry/then_to", json!("done")),
        ("/events/0/id", json!("changed_event_id")),
        ("/events/0/type", json!("timer.expired")),
        ("/events/0/state_id", json!("wait")),
        ("/policy/design_enabled", json!(true)),
    ] {
        let mut edited = wire.clone();
        *edited.pointer_mut(path).unwrap() = value;
        assert_ne!(expected, hash(edited), "{path}");
    }
}

#[test]
fn pipeline_fields_expose_typed_capabilities_and_subjects() {
    let landing = graph(graph_wire()).states.remove(3).executor.unwrap();
    let review = PipelineExecutor {
        id: "r8.review".into(),
        ..landing.clone()
    };
    let unknown = PipelineExecutor {
        version: 99,
        ..review.clone()
    };
    let count = PipelineField::ReviewP0Count.spec(PipelineKind::AgentTask, Some(&review));
    assert!(count.available);
    assert_eq!(count.value_type, ConditionType::Integer);
    assert!(count.operators.contains(&ConditionOperator::Gt));
    assert_eq!(count.subject_binding, SubjectBinding::CandidateProducer);
    assert_eq!(count.freshness, Freshness::CurrentCandidate);

    assert!(
        PipelineField::ReviewVerdict
            .spec(PipelineKind::AutoAction, Some(&landing))
            .available
    );
    assert!(
        !PipelineField::ReviewVerdict
            .spec(PipelineKind::Wait, Some(&review))
            .available
    );
    assert!(
        !PipelineField::ReviewVerdict
            .spec(PipelineKind::AgentTask, Some(&unknown))
            .available
    );
    assert!(
        !PipelineField::PrMerged
            .spec(PipelineKind::AgentTask, Some(&review))
            .available
    );
    assert!(
        !PipelineField::CiRequiredResult
            .spec(PipelineKind::AgentTask, None)
            .available
    );

    let ci = PipelineField::CiRequiredResult.spec(PipelineKind::AgentTask, Some(&review));
    assert!(ci.available);
    assert_eq!(ci.source, ConditionSource::CiObservation);
    assert_eq!(ci.enum_values, ["pass", "fail", "pending"]);
    assert_eq!(ci.operators, [ConditionOperator::Eq, ConditionOperator::Ne]);
    assert_eq!(serde_json::to_value(ci).unwrap()["type"], "enum");

    assert!(
        PipelineField::EventReceived
            .spec(PipelineKind::Wait, None)
            .available
    );
    assert!(
        !PipelineField::EventReceived
            .spec(PipelineKind::EndSuccess, None)
            .available
    );
    let pinned = PipelineField::PolicyDesignEnabled.spec(PipelineKind::Wait, None);
    assert_eq!(pinned.subject_binding, SubjectBinding::GraphPin);
    assert_eq!(pinned.freshness, Freshness::Pinned);
}

#[test]
fn pipeline_preserves_legacy_definition_snapshots() {
    let legacy = legacy_wire();
    let input = serde_json::from_value::<DefinitionInput>(legacy.clone()).unwrap();
    assert!(input.pipeline.is_none());
    assert_eq!(serde_json::to_value(&input).unwrap(), legacy);

    let validated = validate_definition(input).unwrap();
    let snapshot = serde_json::to_value(&validated).unwrap();
    assert_eq!(snapshot["input"], legacy);
    assert_eq!(
        serde_json::from_value::<ValidatedDefinition>(snapshot).unwrap(),
        validated
    );
}

#[test]
fn pipeline_wire_decodes_without_enabling_legacy_writes() {
    let mut wire = legacy_wire();
    wire["pipeline"] = graph_wire();
    let input = serde_json::from_value::<DefinitionInput>(wire).unwrap();
    let serialized = serde_json::to_value(&input).unwrap();
    assert!(serialized["pipeline"].is_object());
    let error = validate_definition(input).unwrap_err();
    assert_eq!(error.code, DefinitionErrorCode::InvalidField);
    assert_eq!(error.target.as_deref(), Some("pipeline"));
}
