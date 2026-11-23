//! Moved from src/db/auto_queue/phase_gate_verdict.rs: every case builds its gate from the KE phase-gate registry.

use crate::db::auto_queue::phase_gate_verdict::*;
use serde_json::Value;
use serde_json::json;

fn gate_context() -> Value {
    json!({
        "phase_gate": crate::phase_gate::resolve_declaration_value("pr-confirm")
            .expect("pr-confirm declaration")
    })
}

fn passing_checks() -> Value {
    json!({
        "merge_verified": {"status": "pass"},
        "issue_closed": {"result": "passed"},
        "build_passed": "pass",
    })
}

#[test]
fn canonical_snapshot_infers_pass() {
    assert_eq!(
        resolve_verdict(Some(&gate_context()), &json!({"checks": passing_checks()})),
        VerdictResolution::Inferred(DEFAULT_PASS_VERDICT.to_string())
    );
}

#[test]
fn explicit_expected_verdict_cannot_bypass_missing_or_failed_checks() {
    for checks in [json!({}), json!({"merge_verified": "fail"})] {
        let result = json!({
            "verdict": DEFAULT_PASS_VERDICT,
            "checks": checks,
        });
        let resolution = resolve_verdict(Some(&gate_context()), &result);
        assert_eq!(
            resolution,
            VerdictResolution::Explicit(DEFAULT_PASS_VERDICT.to_string())
        );
        assert!(!verdict_matches(
            resolution.verdict(),
            DEFAULT_PASS_VERDICT,
            Some(&gate_context()),
            Some(&result),
        ));
    }
}

#[test]
fn generic_pass_requires_all_registry_checks() {
    let passing = json!({"verdict": "pass", "checks": passing_checks()});
    let failing = json!({
        "verdict": "pass",
        "checks": {
            "merge_verified": "pass",
            "issue_closed": "fail",
            "build_passed": "pass",
        }
    });
    assert!(verdict_matches(
        Some("pass"),
        DEFAULT_PASS_VERDICT,
        Some(&gate_context()),
        Some(&passing),
    ));
    assert!(!verdict_matches(
        Some("pass"),
        DEFAULT_PASS_VERDICT,
        Some(&gate_context()),
        Some(&failing),
    ));
}

#[test]
fn malformed_unknown_and_mismatched_snapshots_fail_closed() {
    let passing = json!({"checks": passing_checks()});
    let mut snapshots = vec![
        json!({}),
        json!({"phase_gate": {"kind": "ship-it"}}),
        json!({"phase_gate": {"kind": "pr-confirm"}}),
    ];
    let mut stale = gate_context();
    stale["phase_gate"]["declaration_version"] = json!(999);
    snapshots.push(stale);
    let mut reordered = gate_context();
    reordered["phase_gate"]["required_checks"]
        .as_array_mut()
        .expect("required checks")
        .reverse();
    snapshots.push(reordered);

    for context in snapshots {
        assert_eq!(
            resolve_verdict(Some(&context), &passing),
            VerdictResolution::Missing,
            "snapshot must fail closed: {context}",
        );
    }
}

#[test]
fn deploy_gate_never_accepts_agent_supplied_deploy_verified() {
    let context = json!({
        "phase_gate": crate::phase_gate::resolve_declaration_value("deploy-gate")
            .expect("deploy declaration")
    });
    let result = json!({
        "verdict": DEFAULT_PASS_VERDICT,
        "checks": {
            "build_passed": "pass",
            "deploy_verified": "pass",
        }
    });
    let resolution = resolve_verdict(Some(&context), &result);
    assert!(!verdict_matches(
        resolution.verdict(),
        DEFAULT_PASS_VERDICT,
        Some(&context),
        Some(&result),
    ));
}

#[test]
fn explicit_failure_blocks_inference() {
    let result = json!({
        "phase_gate_verdict": "manual_hold",
        "checks": passing_checks(),
    });
    assert_eq!(
        resolve_verdict(Some(&gate_context()), &result),
        VerdictResolution::Explicit("manual_hold".to_string())
    );
}

#[test]
fn extra_failing_check_refuses_inference() {
    let mut checks = passing_checks();
    checks["extra"] = json!("fail");
    assert_eq!(
        resolve_verdict(Some(&gate_context()), &json!({"checks": checks})),
        VerdictResolution::Missing
    );
}

#[test]
fn non_string_diagnostic_reports_only_closed_json_type() {
    let result = json!({
        "verdict": {"authorization": "Bearer secret"},
        "checks": {"build_passed": "fail"},
    });
    let resolution = resolve_verdict(Some(&gate_context()), &result);
    assert_eq!(
        diagnostic_verdict(&result, &resolution).as_deref(),
        Some("<non-string:object>")
    );
    assert!(
        diagnostic_verdict(&result, &resolution).is_none_or(|value| !value
            .contains("authorization")
            && !value.contains("Bearer secret"))
    );
}

#[test]
fn truthy_non_string_explicit_preserves_inference_compatibility() {
    let result = json!({"verdict": true, "checks": passing_checks()});
    assert_eq!(
        resolve_verdict(Some(&gate_context()), &result),
        VerdictResolution::Inferred(DEFAULT_PASS_VERDICT.to_string())
    );
}

#[test]
fn check_status_truthiness_preserves_result_fallback_boundary() {
    for (status, expected) in [
        (json!(true), false),
        (json!(1), false),
        (json!([]), false),
        (json!(["pass"]), false),
        (json!({}), false),
        (json!(false), true),
        (json!(0), true),
        (Value::Null, true),
        (json!(""), true),
    ] {
        let mut checks = passing_checks();
        checks["merge_verified"] = json!({"status":status,"result":"pass"});
        assert_eq!(
            evaluate(Some(&gate_context()), &json!({"checks":checks}))["matches"],
            expected,
            "status={status}"
        );
    }
    for entry in [
        json!({"result":["pass"]}),
        json!({"status":"","result":["pass"]}),
        json!({"result":{"toString":1}}),
    ] {
        let mut checks = passing_checks();
        checks["merge_verified"] = entry.clone();
        assert_eq!(
            evaluate(Some(&gate_context()), &json!({"checks":checks}))["matches"],
            false,
            "non-string result: {entry}"
        );
    }
}
