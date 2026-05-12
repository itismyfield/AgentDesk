use crate::db::automation_candidates::IterationOutcomeAction;

use super::{
    CandidateProgramInput, MaterializeCandidateInput, MaterializerError, allowed_path_matches,
    expected_next_iteration, is_final_program_iteration, iteration_outcome_action,
    normalize_candidate_metadata, normalize_changed_paths_report, validate_active_iteration_status,
    validate_iteration_budget, validate_iteration_sequence,
};

#[test]
fn accepts_exact_and_child_paths() {
    assert!(allowed_path_matches("src/foo", "src/foo"));
    assert!(allowed_path_matches("src/foo/bar.rs", "src/foo"));
}

#[test]
fn rejects_prefix_siblings_and_traversal() {
    assert!(!allowed_path_matches("src/foo2/bar.rs", "src/foo"));
    assert!(!allowed_path_matches("src/foo/../bar.rs", "src/foo"));
    assert!(!allowed_path_matches("../src/foo.rs", "src"));
}

#[test]
fn rejects_absolute_or_empty_paths() {
    assert!(!allowed_path_matches("/src/foo.rs", "src"));
    assert!(!allowed_path_matches("src/foo.rs", ""));
    assert!(!allowed_path_matches("", "src"));
}

#[test]
fn changed_paths_report_is_required() {
    assert!(normalize_changed_paths_report(None).is_err());
    assert!(normalize_changed_paths_report(Some(vec![])).is_err());
    assert!(normalize_changed_paths_report(Some(vec!["  ".to_string()])).is_err());
    assert_eq!(
        normalize_changed_paths_report(Some(vec![" src/foo.rs ".to_string()])).unwrap(),
        vec!["src/foo.rs"]
    );
}

#[test]
fn iteration_sequence_must_match_current_iteration_plus_one() {
    let program = serde_json::json!({ "current_iteration": 2 });
    assert!(validate_iteration_sequence(3, &program).is_ok());

    let error = validate_iteration_sequence(4, &program).unwrap_err();
    assert!(matches!(
        error,
        MaterializerError::IterationOutOfSequence {
            expected: 3,
            actual: 4
        }
    ));
}

#[test]
fn missing_current_iteration_starts_at_one() {
    let program = serde_json::json!({});
    assert_eq!(expected_next_iteration(&program), 1);
    assert!(validate_iteration_sequence(1, &program).is_ok());
}

#[test]
fn iteration_result_accepts_only_active_loop_statuses() {
    assert!(validate_active_iteration_status("ready").is_ok());
    assert!(validate_active_iteration_status("requested").is_ok());
    assert!(validate_active_iteration_status("in_progress").is_ok());

    let error = validate_active_iteration_status("review").unwrap_err();
    assert!(matches!(
        error,
        MaterializerError::InactiveLoopState { status } if status == "review"
    ));
}

#[test]
fn prepare_iteration_must_not_exceed_budget() {
    let program = serde_json::json!({"iteration_budget": 3});
    assert!(validate_iteration_budget(3, &program).is_ok());

    let error = validate_iteration_budget(4, &program).unwrap_err();
    assert!(matches!(
        error,
        MaterializerError::IterationBudgetExceeded { max: 3, actual: 4 }
    ));
}

#[test]
fn final_discard_does_not_requeue() {
    assert_eq!(
        iteration_outcome_action("discard", true),
        IterationOutcomeAction::DiscardFinalGate
    );
    assert_eq!(
        iteration_outcome_action("discard", false),
        IterationOutcomeAction::DiscardRequeue
    );
}

#[test]
fn materialized_metadata_marks_loop_candidate() {
    let metadata = normalize_candidate_metadata(&MaterializeCandidateInput {
        title: "candidate".to_string(),
        repo_id: None,
        priority: None,
        assigned_agent_id: None,
        description: Some("desc".to_string()),
        source: Some("routine_recommender".to_string()),
        dedupe_key: Some("pattern:1".to_string()),
        start_ready: false,
        program: CandidateProgramInput {
            repo_dir: "/repo".to_string(),
            allowed_write_paths: vec!["src/services".to_string()],
            metric_name: "failure_count".to_string(),
            metric_target: 0.0,
            metric_direction: Some("lower".to_string()),
            final_gate: Some("manual_review".to_string()),
            iteration_budget: Some(4),
        },
    })
    .expect("valid metadata");

    // pipeline_stage_id alone is the discriminator — no enabled/loop_enabled flags
    assert_eq!(
        metadata["automation_candidate"]["source"],
        "routine_recommender"
    );
    assert_eq!(metadata["automation_candidate"]["dedupe_key"], "pattern:1");
    assert_eq!(metadata["program"]["metric_direction"], "lower_is_better");
    assert_eq!(
        metadata["program"]["iteration_budget"],
        serde_json::json!(4)
    );
}

#[test]
fn materialized_metadata_rejects_placeholder_repo_dir() {
    let error = normalize_candidate_metadata(&MaterializeCandidateInput {
        title: "candidate".to_string(),
        repo_id: None,
        priority: None,
        assigned_agent_id: None,
        description: None,
        source: None,
        dedupe_key: None,
        start_ready: false,
        program: CandidateProgramInput {
            repo_dir: "<required: absolute repo path>".to_string(),
            allowed_write_paths: vec!["src".to_string()],
            metric_name: "failure_count".to_string(),
            metric_target: 0.0,
            metric_direction: None,
            final_gate: None,
            iteration_budget: None,
        },
    })
    .expect_err("placeholder repo_dir must fail validation");

    assert!(error.to_string().contains("absolute repo path"));
}

#[test]
fn program_iteration_budget_clamps() {
    assert!(is_final_program_iteration(
        3,
        &serde_json::json!({"iteration_budget": 3})
    ));
    assert!(!is_final_program_iteration(
        2,
        &serde_json::json!({"iteration_budget": 3})
    ));
    assert!(is_final_program_iteration(
        10,
        &serde_json::json!({"iteration_budget": 99})
    ));
}
