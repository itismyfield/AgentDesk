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
//! The semantics deliberately mirror
//! `policies/lib/auto-queue-phase-gate.js::_inferPhaseGatePassVerdict` so the
//! JS policy hook and the durable Rust path agree while the JS reducer is
//! still live.

use serde_json::Value;

/// Gate verdict used when a `phase_gate` context omits `pass_verdict`.
pub const DEFAULT_PASS_VERDICT: &str = "phase_gate_passed";

/// Result keys that carry an explicit verdict, in the same precedence order as
/// the JS `result.verdict || result.decision || result.phase_gate_verdict`
/// chain.
const EXPLICIT_VERDICT_KEYS: [&str; 3] = ["verdict", "decision", "phase_gate_verdict"];

/// The canonical result of reducing phase-gate context and result evidence.
///
/// `Explicit(None)` is intentional: a truthy non-string value blocks checks
/// inference but cannot be compared with the gate's string pass verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictResolution {
    Explicit(Option<String>),
    Inferred(String),
    Missing,
}

impl VerdictResolution {
    pub fn verdict(&self) -> Option<&str> {
        match self {
            Self::Explicit(Some(verdict)) | Self::Inferred(verdict) => Some(verdict),
            Self::Explicit(None) | Self::Missing => None,
        }
    }
}

/// JS-style truthiness for `result.verdict || result.decision ||
/// result.phase_gate_verdict`.
///
/// Any non-falsy value (boolean `true`, non-zero number, non-empty string, any
/// array/object) counts as an explicit verdict and therefore blocks inference —
/// including values that are not strings. Matching JS here matters because the
/// policy hook and the durable path must not disagree about whether a result
/// "already decided".
pub fn has_explicit_verdict(result: &Value) -> bool {
    first_explicit_value(result).is_some()
}

/// Trimmed, non-empty string form of the first truthy explicit verdict, if it
/// is a string. A truthy non-string value blocks later keys, matching the JS
/// `verdict || decision || phase_gate_verdict` precedence chain.
pub fn explicit_verdict(result: &Value) -> Option<String> {
    first_explicit_value(result)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Whether a single `result.checks` entry reports a pass.
///
/// Accepts the canonical `{"status": "pass"}` object form, the `{"result":
/// "pass"}` alias, and a bare `"pass"` string. `status` is only consulted when
/// it is a non-empty string so an empty `status` falls through to `result`,
/// mirroring the JS `entry.status || entry.result` chain (#2048 F12).
pub fn check_entry_is_pass(entry: &Value) -> bool {
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
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PASS_VERDICT)
        .to_string()
}

/// Checks-only inference against an already-extracted `phase_gate` context
/// object. Ignores any explicit verdict on `result` — callers that must honour
/// an explicit verdict use [`infer_pass_verdict`] instead.
///
/// Refuses to infer when the gate context is not an object, when no checks are
/// reported, when a declared check is missing from `result.checks`, or when any
/// declared-or-present check does not pass.
pub fn infer_pass_verdict_in_gate(phase_gate: &Value, result: &Value) -> Option<String> {
    if !phase_gate.is_object() {
        return None;
    }
    let checks = result.get("checks")?.as_object()?;
    if checks.is_empty() {
        return None;
    }

    if let Some(declared) = phase_gate.get("checks").and_then(Value::as_array) {
        for required in declared {
            let Some(name) = required.as_str() else {
                continue;
            };
            let Some(entry) = checks.get(name) else {
                return None;
            };
            if !check_entry_is_pass(entry) {
                return None;
            }
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
pub fn infer_pass_verdict_from_checks(context: Option<&Value>, result: &Value) -> Option<String> {
    infer_pass_verdict_in_gate(context?.get("phase_gate")?, result)
}

/// Full inference: never overrides an explicit verdict (even an explicit
/// failure), otherwise falls back to checks-only inference.
pub fn infer_pass_verdict(context: Option<&Value>, result: &Value) -> Option<String> {
    if has_explicit_verdict(result) {
        return None;
    }
    infer_pass_verdict_from_checks(context, result)
}

/// Resolve explicit evidence first, then checks-only inference.
pub fn resolve_verdict(context: Option<&Value>, result: &Value) -> VerdictResolution {
    if has_explicit_verdict(result) {
        return VerdictResolution::Explicit(explicit_verdict(result));
    }
    match infer_pass_verdict_from_checks(context, result) {
        Some(verdict) => VerdictResolution::Inferred(verdict),
        None => VerdictResolution::Missing,
    }
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
    let Some(actual) = actual.map(str::trim).filter(|value| !value.is_empty()) else {
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

fn first_explicit_value(result: &Value) -> Option<&Value> {
    EXPLICIT_VERDICT_KEYS
        .iter()
        .filter_map(|key| result.get(*key))
        .find(|value| is_js_truthy(value))
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
            assert!(
                has_explicit_verdict(&result),
                "{key} should count as explicit"
            );
            assert_eq!(
                infer_pass_verdict(Some(&context), &result),
                None,
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
        assert_eq!(infer_pass_verdict(Some(&context), &result), None);
        let resolution = resolve_verdict(Some(&context), &result);
        assert!(!verdict_matches(
            resolution.verdict(),
            "phase_gate_passed",
            Some(&context),
            Some(&result),
        ));
    }

    #[test]
    fn non_string_truthy_explicit_verdict_blocks_inference() {
        let context = gate_context(json!([]), "phase_gate_passed");
        for explicit in [json!(true), json!(1), json!({ "nested": 1 })] {
            let result = json!({
                "verdict": explicit,
                "checks": { "build_passed": "pass" },
            });
            assert_eq!(infer_pass_verdict(Some(&context), &result), None);
            assert_eq!(
                resolve_verdict(Some(&context), &result),
                VerdictResolution::Explicit(None),
            );
        }
    }

    #[test]
    fn truthy_non_string_verdict_prevents_later_decision_from_winning() {
        let context = gate_context(json!(["build_passed"]), "gate_ok");
        let result = json!({
            "verdict": true,
            "decision": "gate_ok",
            "checks": { "build_passed": "pass" },
        });

        assert_eq!(explicit_verdict(&result), None);
        assert_eq!(
            resolve_verdict(Some(&context), &result),
            VerdictResolution::Explicit(None),
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
                infer_pass_verdict(Some(&context), &result).as_deref(),
                Some("phase_gate_passed"),
            );
        }
    }

    /// #4884 divergence 2: the finalize path short-circuited on the presence of
    /// a `status` key even when it was empty, so `{"status": "", "result":
    /// "pass"}` read as a failure there and as a pass in the reconciler.
    #[test]
    fn empty_status_falls_back_to_result_alias() {
        assert!(check_entry_is_pass(&json!({ "status": "", "result": "pass" })));
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
        assert_eq!(infer_pass_verdict(Some(&context), &result), None);
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
        assert_eq!(infer_pass_verdict(Some(&context), &result), None);
    }

    #[test]
    fn empty_or_absent_checks_refuse_inference() {
        let context = gate_context(json!([]), "gate_ok");
        assert_eq!(
            infer_pass_verdict(Some(&context), &json!({ "checks": {} })),
            None
        );
        assert_eq!(infer_pass_verdict(Some(&context), &json!({})), None);
        assert_eq!(
            infer_pass_verdict(Some(&context), &json!({ "checks": [] })),
            None
        );
    }

    #[test]
    fn missing_phase_gate_context_refuses_inference() {
        let result = json!({ "checks": { "build_passed": "pass" } });
        assert_eq!(infer_pass_verdict(None, &result), None);
        assert_eq!(infer_pass_verdict(Some(&json!({})), &result), None);
        assert_eq!(
            infer_pass_verdict(Some(&json!({ "phase_gate": "nope" })), &result),
            None
        );
    }

    #[test]
    fn pass_verdict_defaults_when_absent_or_blank() {
        assert_eq!(pass_verdict_of(&json!({})), DEFAULT_PASS_VERDICT);
        assert_eq!(
            pass_verdict_of(&json!({ "pass_verdict": "  " })),
            DEFAULT_PASS_VERDICT
        );
        assert_eq!(pass_verdict_of(&json!({ "pass_verdict": "gate_ok" })), "gate_ok");
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
        assert!(!verdict_matches(Some("pass"), "gate_ok", Some(&context), None));
        assert!(verdict_matches(
            Some(" gate_ok "),
            "gate_ok",
            None,
            Some(&passing)
        ));
        assert!(!verdict_matches(None, "gate_ok", Some(&context), Some(&passing)));
        assert!(!verdict_matches(Some("   "), "gate_ok", Some(&context), Some(&passing)));
    }

    #[test]
    fn resolve_verdict_prefers_explicit_then_inferred() {
        let context = gate_context(json!(["build_passed"]), "gate_ok");
        let explicit = json!({ "decision": "gate_failed", "checks": { "build_passed": "pass" } });
        assert_eq!(
            resolve_verdict(Some(&context), &explicit),
            VerdictResolution::Explicit(Some("gate_failed".to_string()))
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
