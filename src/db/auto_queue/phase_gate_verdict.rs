//! Canonical phase-gate verdict reducer shared by policy hooks, finalize, and durable reconciliation.
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
            .filter(|value| is_js_truthy(value))
            .or_else(|| map.get("result"))
            .and_then(Value::as_str),
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

/// Return the complete policy decision so callers only coordinate its effects.
pub(crate) fn evaluate(context: Option<&Value>, result: &Value) -> Value {
    let resolution = resolve_verdict(context, result);
    let expected = context
        .and_then(|context| context.get("phase_gate"))
        .map(pass_verdict_of)
        .unwrap_or_else(|| DEFAULT_PASS_VERDICT.to_string());
    serde_json::json!({
        "verdict": resolution.verdict(),
        "inferred": matches!(resolution, VerdictResolution::Inferred(_)),
        "pass_verdict": expected,
        "matches": verdict_matches(resolution.verdict(), &expected, context, Some(result)),
        "diagnostic": diagnostic_verdict(result, &resolution),
    })
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
