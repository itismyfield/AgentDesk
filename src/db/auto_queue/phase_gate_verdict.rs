//! Canonical phase-gate verdict reducer shared by finalize and durable reconciliation paths.
//!
//! A pass is accepted only against an exact current registry snapshot. Explicit verdict strings
//! remain available for diagnostics, but they do not bypass declaration or check validation.

use serde_json::Value;

pub const DEFAULT_PASS_VERDICT: &str = "phase_gate_passed";
const EXPLICIT_VERDICT_KEYS: [&str; 3] = ["verdict", "decision", "phase_gate_verdict"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictResolution {
    Explicit(String),
    Inferred(String),
    Missing,
}

impl VerdictResolution {
    pub fn verdict(&self) -> Option<&str> {
        match self {
            Self::Explicit(verdict) | Self::Inferred(verdict) => Some(verdict),
            Self::Missing => None,
        }
    }
}

fn explicit_verdict(result: &Value) -> Option<String> {
    EXPLICIT_VERDICT_KEYS
        .iter()
        .filter_map(|key| result.get(*key).and_then(Value::as_str))
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn is_js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number
            .as_f64()
            .map(|raw| raw != 0.0 && !raw.is_nan())
            .unwrap_or(false),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn check_entry_is_pass(entry: &Value) -> bool {
    let raw = match entry {
        Value::String(text) => Some(text.as_str()),
        Value::Object(map) => map
            .get("status")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| map.get("result").and_then(Value::as_str)),
        _ => None,
    };
    raw.map(|status| status.eq_ignore_ascii_case("pass") || status.eq_ignore_ascii_case("passed"))
        .unwrap_or(false)
}

pub fn pass_verdict_of(phase_gate: &Value) -> String {
    phase_gate
        .get("pass_verdict")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PASS_VERDICT)
        .to_string()
}

fn infer_pass_verdict_in_gate(phase_gate: &Value, result: &Value) -> Option<String> {
    let declared = crate::phase_gate::dispatch_result_checks(phase_gate)?;
    let checks = result.get("checks")?.as_object()?;
    if checks.is_empty() {
        return None;
    }
    for required in declared {
        if !checks.get(required).is_some_and(check_entry_is_pass) {
            return None;
        }
    }
    if !checks.values().all(check_entry_is_pass) {
        return None;
    }
    Some(pass_verdict_of(phase_gate))
}

fn infer_pass_verdict_from_checks(context: Option<&Value>, result: &Value) -> Option<String> {
    infer_pass_verdict_in_gate(context?.get("phase_gate")?, result)
}

pub fn authoritative_context(
    context: Option<&Value>,
    persisted_legacy_default: bool,
) -> Option<Value> {
    crate::phase_gate::authoritative_evaluation_context(context?, persisted_legacy_default)
}

pub fn resolve_verdict(context: Option<&Value>, result: &Value) -> VerdictResolution {
    if let Some(verdict) = explicit_verdict(result) {
        return VerdictResolution::Explicit(verdict);
    }
    match infer_pass_verdict_from_checks(context, result) {
        Some(verdict) => VerdictResolution::Inferred(verdict),
        None => VerdictResolution::Missing,
    }
}

/// Preserve a closed diagnostic class without serializing arbitrary untrusted values.
pub fn diagnostic_verdict(result: &Value, resolution: &VerdictResolution) -> Option<String> {
    resolution.verdict().map(str::to_string).or_else(|| {
        EXPLICIT_VERDICT_KEYS
            .iter()
            .filter_map(|key| result.get(*key))
            .find(|value| is_js_truthy(value))
            .map(|value| match value {
                Value::Bool(_) => "<non-string:boolean>",
                Value::Number(_) => "<non-string:number>",
                Value::Array(_) => "<non-string:array>",
                Value::Object(_) => "<non-string:object>",
                Value::Null | Value::String(_) => "<non-string:unknown>",
            })
            .map(str::to_string)
    })
}

/// Explicit and generic pass verdicts both require independently validated registry checks.
pub fn verdict_matches(
    actual: Option<&str>,
    expected: &str,
    context: Option<&Value>,
    result: Option<&Value>,
) -> bool {
    let Some(actual) = actual.filter(|value| !value.is_empty()) else {
        return false;
    };
    if actual != expected && !matches!(actual, "pass" | "passed") {
        return false;
    }
    result
        .and_then(|result| infer_pass_verdict_from_checks(context, result))
        .as_deref()
        == Some(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
