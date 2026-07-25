//! #4884: canonical phase-gate verdict reducer inputs.
//!
//! Every durable phase-gate decision — the finalize path's verdict injection
//! (`src/dispatch/dispatch_status.rs`), the reconciler that runs for CRUD /
//! watcher / bridge-recovery completions
//! (`reconcile_phase_gate_for_terminal_dispatch_on_pg_tx`), and the sibling
//! aggregation inside that reconciler — must answer the same two questions the
//! same way:
//!
//!   1. does this dispatch result already carry an explicit verdict?
//!   2. if not, do the reported `checks` justify inferring the gate's
//!      `pass_verdict`?
//!
//! Before this module those questions had two divergent Rust answers, so the
//! same dispatch result could pass on one completion entry point and fail on
//! another (see the module tests for the two concrete divergences that were
//! fixed). This module is the single Rust authority; it is pure, so it can be
//! unit-tested without Postgres and reused from both the dispatch layer and
//! the db layer.
//!
//! Rust preserves the established finalize-path compatibility contract while
//! the JavaScript reducer is still live. In particular, non-string values in
//! explicit verdict fields are ignored for inference, as they were before
//! #4884; see the known JavaScript deltas below.

use serde_json::Value;

/// Gate verdict used when a `phase_gate` context omits `pass_verdict`.
pub const DEFAULT_PASS_VERDICT: &str = "phase_gate_passed";

/// Result keys that carry an explicit verdict, in the same precedence order as
/// the JS `result.verdict || result.decision || result.phase_gate_verdict`
/// chain.
const EXPLICIT_VERDICT_KEYS: [&str; 3] = ["verdict", "decision", "phase_gate_verdict"];

/// The canonical result of reducing phase-gate context and result evidence.
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

/// Non-empty string form of the first explicit verdict field.
///
/// Non-string values intentionally do not block inference. This preserves the
/// pre-#4884 finalize behavior, where `as_str()` treated them as absent and an
/// all-passing checks payload received the configured pass verdict. Strings are
/// kept byte-for-byte so blank and whitespace-only values retain the historical
/// behavior instead of silently changing verdict matching.
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

/// Whether a single `result.checks` entry reports a pass.
///
/// Accepts the canonical `{"status": "pass"}` object form, the `{"result":
/// "pass"}` alias, and a bare `"pass"` string. `status` is only consulted when
/// it is a non-empty string so an empty `status` falls through to `result`
/// (#2048 F12).
///
/// Known JavaScript delta: a truthy non-string `status` blocks the JavaScript
/// `result` fallback, while Rust ignores non-string status values and may use a
/// string `result` alias. This reducer preserves the pre-#4884 Rust behavior.
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

/// The `pass_verdict` declared by a `phase_gate` context object, falling back
/// to [`DEFAULT_PASS_VERDICT`].
pub fn pass_verdict_of(phase_gate: &Value) -> String {
    phase_gate
        .get("pass_verdict")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PASS_VERDICT)
        .to_string()
}

/// Checks-only inference against an already-extracted `phase_gate` context
/// object. Ignores any explicit verdict on `result`; the canonical
/// [`resolve_verdict`] entry point checks explicit evidence first.
///
/// Refuses to infer when the gate context is not an object, when no checks are
/// reported, when a declared check is missing from `result.checks`, or when any
/// declared-or-present check does not pass.
///
/// Known JavaScript deltas: JavaScript accepts an array as `result.checks`, and
/// skips falsy declared-check items. Rust requires an object map and rejects any
/// non-string declared item. The stricter declaration handling prevents a future
/// descriptor migration from silently dropping the completeness guard (#4882).
fn infer_pass_verdict_in_gate(phase_gate: &Value, result: &Value) -> Option<String> {
    if !phase_gate.is_object() {
        return None;
    }
    let checks = result.get("checks")?.as_object()?;
    if checks.is_empty() {
        return None;
    }

    let Some(declared) = phase_gate.get("checks").and_then(Value::as_array) else {
        tracing::warn!(
            "[phase_gate] refusing verdict inference without an explicit declared-check array"
        );
        return None;
    };
    if declared.is_empty() {
        return None;
    }
    for required in declared {
        let Some(name) = required.as_str() else {
            tracing::warn!(
                declared_check = %required,
                "[phase_gate] refusing verdict inference for non-string declared check"
            );
            return None;
        };
        let Some(entry) = checks.get(name) else {
            return None;
        };
        if !check_entry_is_pass(entry) {
            return None;
        }
    }

    // Every *present* entry must also pass: a partial payload where the
    // declared checks pass but an extra check fails must not advance the gate.
    if !checks.values().all(check_entry_is_pass) {
        return None;
    }

    Some(pass_verdict_of(phase_gate))
}

/// Checks-only inference against a full dispatch context (`context.phase_gate`).
fn infer_pass_verdict_from_checks(context: Option<&Value>, result: &Value) -> Option<String> {
    infer_pass_verdict_in_gate(context?.get("phase_gate")?, result)
}

/// Resolve explicit evidence first, then checks-only inference.
pub fn resolve_verdict(context: Option<&Value>, result: &Value) -> VerdictResolution {
    if let Some(verdict) = explicit_verdict(result) {
        return VerdictResolution::Explicit(verdict);
    }
    match infer_pass_verdict_from_checks(context, result) {
        Some(verdict) => VerdictResolution::Inferred(verdict),
        None => VerdictResolution::Missing,
    }
}

/// Preserve the most useful explicit class for durable failure diagnostics.
///
/// The reducer's inferred/string verdict remains authoritative for matching. If
/// resolution is missing, a truthy non-string explicit field is represented by
/// a closed JSON-type label. Never serialize the payload itself: verdict values
/// can contain credentials or other untrusted data that reaches DB and logs.
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

/// Whether `actual` satisfies the gate's `expected` pass verdict.
///
/// A generic `pass` / `passed` verdict is accepted only when the reported
/// checks independently justify `expected`, so a bare `"pass"` cannot advance a
/// gate whose checks did not actually pass.
pub fn verdict_matches(
    actual: Option<&str>,
    expected: &str,
    context: Option<&Value>,
    result: Option<&Value>,
) -> bool {
    let Some(actual) = actual.filter(|value| !value.is_empty()) else {
        return false;
    };
    if actual == expected {
        return true;
    }
    if !matches!(actual, "pass" | "passed") {
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

    fn gate_context(checks: Value, pass_verdict: &str) -> Value {
        json!({
            "phase_gate": {
                "checks": checks,
                "pass_verdict": pass_verdict,
            }
        })
    }

    #[test]
    fn explicit_verdict_blocks_inference_across_all_three_keys() {
        let context = gate_context(json!(["merge_verified"]), "phase_gate_passed");
        for key in ["verdict", "decision", "phase_gate_verdict"] {
            let result = json!({
                key: "fail",
                "checks": { "merge_verified": { "status": "pass" } },
            });
            assert_eq!(
                resolve_verdict(Some(&context), &result),
                VerdictResolution::Explicit("fail".to_string()),
                "{key} must not be overridden by checks-only inference"
            );
            assert_eq!(explicit_verdict(&result).as_deref(), Some("fail"));
        }
    }

    /// #4884 divergence 1: the finalize path only inspected `verdict` /
    /// `decision`, so a result whose explicit failure lived on
    /// `phase_gate_verdict` had a pass verdict injected over it while the
    /// durable reconciler refused to infer. Same evidence, opposite outcome
    /// depending on the completion entry point.
    #[test]
    fn phase_gate_verdict_key_is_honoured_as_explicit() {
        let context = gate_context(json!(["merge_verified"]), "phase_gate_passed");
        let result = json!({
            "phase_gate_verdict": "phase_gate_failed",
            "checks": { "merge_verified": { "status": "pass" } },
        });
        let resolution = resolve_verdict(Some(&context), &result);
        assert_eq!(
            resolution,
            VerdictResolution::Explicit("phase_gate_failed".to_string())
        );
        assert!(!verdict_matches(
            resolution.verdict(),
            "phase_gate_passed",
            Some(&context),
            Some(&result),
        ));
    }

    #[test]
    fn non_string_explicit_fields_preserve_finalize_inference_compatibility() {
        let context = gate_context(json!(["build_passed"]), "phase_gate_passed");
        for explicit in [json!(true), json!(1), json!({ "nested": 1 })] {
            let result = json!({
                "verdict": explicit,
                "checks": { "build_passed": "pass" },
            });
            assert_eq!(
                resolve_verdict(Some(&context), &result),
                VerdictResolution::Inferred("phase_gate_passed".to_string()),
            );
        }
    }

    #[test]
    fn non_string_diagnostic_reports_only_closed_json_type() {
        let context = gate_context(json!(["build_passed"]), "phase_gate_passed");
        let result = json!({
            "verdict": {"authorization": "Bearer secret"},
            "checks": {"build_passed": "fail"},
        });
        let resolution = resolve_verdict(Some(&context), &result);

        assert_eq!(resolution, VerdictResolution::Missing);
        let diagnostic = diagnostic_verdict(&result, &resolution);
        assert_eq!(diagnostic.as_deref(), Some("<non-string:object>"));
        assert!(
            diagnostic.as_deref().is_none_or(|value| {
                !value.contains("authorization") && !value.contains("Bearer secret")
            }),
            "diagnostic must not contain arbitrary verdict payload: {diagnostic:?}"
        );
    }

    #[test]
    fn non_string_verdict_allows_later_string_decision() {
        let context = gate_context(json!(["build_passed"]), "gate_ok");
        let result = json!({
            "verdict": true,
            "decision": "manual_hold",
            "checks": { "build_passed": "pass" },
        });

        assert_eq!(
            resolve_verdict(Some(&context), &result),
            VerdictResolution::Explicit("manual_hold".to_string()),
        );
    }

    #[test]
    fn falsy_explicit_verdict_does_not_block_inference() {
        let context = gate_context(json!(["build_passed"]), "phase_gate_passed");
        for falsy in [json!(null), json!(false), json!(0), json!("")] {
            let result = json!({
                "verdict": falsy,
                "checks": { "build_passed": "pass" },
            });
            assert_eq!(
                resolve_verdict(Some(&context), &result),
                VerdictResolution::Inferred("phase_gate_passed".to_string()),
            );
        }
    }

    /// #4884 divergence 2: the finalize path short-circuited on the presence of
    /// a `status` key even when it was empty, so `{"status": "", "result":
    /// "pass"}` read as a failure there and as a pass in the reconciler.
    #[test]
    fn empty_status_falls_back_to_result_alias() {
        assert!(check_entry_is_pass(
            &json!({ "status": "", "result": "pass" })
        ));
        assert!(check_entry_is_pass(&json!({ "status": "PASSED" })));
        assert!(check_entry_is_pass(&json!("pass")));
        assert!(!check_entry_is_pass(&json!({ "status": "fail" })));
        assert!(!check_entry_is_pass(&json!({})));
        assert!(!check_entry_is_pass(&json!(7)));
    }

    #[test]
    fn missing_declared_check_refuses_inference() {
        let context = gate_context(json!(["merge_verified", "issue_closed"]), "gate_ok");
        let result = json!({ "checks": { "merge_verified": { "status": "pass" } } });
        assert_eq!(
            resolve_verdict(Some(&context), &result),
            VerdictResolution::Missing
        );
    }

    #[test]
    fn extra_failing_check_refuses_inference() {
        let context = gate_context(json!(["merge_verified"]), "gate_ok");
        let result = json!({
            "checks": {
                "merge_verified": { "status": "pass" },
                "issue_closed": { "status": "fail" },
            }
        });
        assert_eq!(
            resolve_verdict(Some(&context), &result),
            VerdictResolution::Missing
        );
    }

    #[test]
    fn absent_or_invalid_declared_checks_fail_closed() {
        let passing = json!({ "checks": { "merge_verified": "pass" } });
        for declared in [json!(null), json!("merge_verified"), json!([])] {
            let context = gate_context(declared, "gate_ok");
            assert_eq!(
                resolve_verdict(Some(&context), &passing),
                VerdictResolution::Missing,
            );
        }
    }

    #[test]
    fn non_string_declared_check_fails_closed() {
        let context = gate_context(json!([{"name": "merge_verified"}]), "gate_ok");
        let result = json!({ "checks": { "merge_verified": "pass" } });
        assert_eq!(
            resolve_verdict(Some(&context), &result),
            VerdictResolution::Missing,
        );
    }

    #[test]
    fn truthy_non_string_status_does_not_hide_string_result_alias() {
        let context = gate_context(json!(["merge_verified"]), "gate_ok");
        for status in [json!(true), json!(1)] {
            let result = json!({
                "checks": {
                    "merge_verified": { "status": status, "result": "pass" }
                }
            });
            assert_eq!(
                resolve_verdict(Some(&context), &result),
                VerdictResolution::Inferred("gate_ok".to_string()),
            );
        }
    }

    #[test]
    fn empty_or_absent_checks_refuse_inference() {
        let context = gate_context(json!([]), "gate_ok");
        assert_eq!(
            resolve_verdict(Some(&context), &json!({ "checks": {} })),
            VerdictResolution::Missing
        );
        assert_eq!(
            resolve_verdict(Some(&context), &json!({})),
            VerdictResolution::Missing
        );
        assert_eq!(
            resolve_verdict(Some(&context), &json!({ "checks": [] })),
            VerdictResolution::Missing
        );
    }

    #[test]
    fn missing_phase_gate_context_refuses_inference() {
        let result = json!({ "checks": { "build_passed": "pass" } });
        assert_eq!(resolve_verdict(None, &result), VerdictResolution::Missing);
        assert_eq!(
            resolve_verdict(Some(&json!({})), &result),
            VerdictResolution::Missing
        );
        assert_eq!(
            resolve_verdict(Some(&json!({ "phase_gate": "nope" })), &result),
            VerdictResolution::Missing
        );
    }

    #[test]
    fn pass_verdict_defaults_only_when_absent_and_preserves_raw_strings() {
        assert_eq!(pass_verdict_of(&json!({})), DEFAULT_PASS_VERDICT);
        assert_eq!(pass_verdict_of(&json!({ "pass_verdict": "" })), "");
        assert_eq!(pass_verdict_of(&json!({ "pass_verdict": "  " })), "  ");
        assert_eq!(
            pass_verdict_of(&json!({ "pass_verdict": " gate_ok " })),
            " gate_ok "
        );
    }

    #[test]
    fn generic_pass_matches_only_when_checks_justify_it() {
        let context = gate_context(json!(["merge_verified"]), "gate_ok");
        let passing = json!({ "checks": { "merge_verified": { "status": "pass" } } });
        let failing = json!({ "checks": { "merge_verified": { "status": "fail" } } });

        assert!(verdict_matches(
            Some("pass"),
            "gate_ok",
            Some(&context),
            Some(&passing)
        ));
        assert!(!verdict_matches(
            Some("pass"),
            "gate_ok",
            Some(&context),
            Some(&failing)
        ));
        assert!(!verdict_matches(
            Some("pass"),
            "gate_ok",
            Some(&context),
            None
        ));
        assert!(!verdict_matches(
            Some(" gate_ok "),
            "gate_ok",
            None,
            Some(&passing)
        ));
        assert!(!verdict_matches(
            None,
            "gate_ok",
            Some(&context),
            Some(&passing)
        ));
        assert!(!verdict_matches(
            Some("   "),
            "gate_ok",
            Some(&context),
            Some(&passing)
        ));
    }

    #[test]
    fn raw_whitespace_pass_verdict_matches_consistently() {
        let context = gate_context(json!(["build_passed"]), " gate_ok ");
        let result = json!({
            "verdict": " gate_ok ",
            "checks": {"build_passed": "pass"}
        });
        let resolution = resolve_verdict(Some(&context), &result);
        assert_eq!(
            resolution,
            VerdictResolution::Explicit(" gate_ok ".to_string())
        );
        assert!(verdict_matches(
            resolution.verdict(),
            " gate_ok ",
            Some(&context),
            Some(&result)
        ));
        assert!(!verdict_matches(
            resolution.verdict(),
            "gate_ok",
            Some(&context),
            Some(&result)
        ));
    }

    #[test]
    fn resolve_verdict_prefers_explicit_then_inferred() {
        let context = gate_context(json!(["build_passed"]), "gate_ok");
        let explicit = json!({ "decision": "gate_failed", "checks": { "build_passed": "pass" } });
        assert_eq!(
            resolve_verdict(Some(&context), &explicit),
            VerdictResolution::Explicit("gate_failed".to_string())
        );

        let inferred = json!({ "checks": { "build_passed": "pass" } });
        assert_eq!(
            resolve_verdict(Some(&context), &inferred),
            VerdictResolution::Inferred("gate_ok".to_string())
        );

        let undecided = json!({ "checks": { "build_passed": "fail" } });
        assert_eq!(
            resolve_verdict(Some(&context), &undecided),
            VerdictResolution::Missing
        );
    }
}
