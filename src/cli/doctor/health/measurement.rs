use super::{ClassifiedReason, classify_degraded_reason};
use serde::Serialize;
use serde_json::Value;

pub(crate) type Measurement<T> = Result<T, FieldIssue>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct FieldIssue {
    pub(crate) field: String,
    pub(crate) code: &'static str,
    expected: &'static str,
    actual: &'static str,
}

impl FieldIssue {
    pub(crate) fn new(
        field: impl Into<String>,
        value: Option<&Value>,
        expected: &'static str,
    ) -> Self {
        let actual = match value {
            None => "missing",
            Some(Value::Null) => "null",
            Some(Value::Bool(_)) => "boolean",
            Some(Value::Number(_)) => "number",
            Some(Value::String(_)) => "string",
            Some(Value::Array(_)) => "array",
            Some(Value::Object(_)) => "object",
        };
        Self {
            field: field.into(),
            code: match actual {
                "missing" => "missing",
                "null" => "null",
                _ => "wrong_type",
            },
            expected,
            actual,
        }
    }
    pub(crate) fn describe(&self) -> String {
        format!(
            "unmeasured: {}/{}: expected {}, got {}",
            self.field, self.code, self.expected, self.actual
        )
    }
}

pub(crate) fn read<'a, T>(
    body: &'a Value,
    key: &str,
    expected: &'static str,
    parse: impl FnOnce(&'a Value) -> Option<T>,
) -> Measurement<T> {
    body.get(key)
        .and_then(parse)
        .ok_or_else(|| FieldIssue::new(key, body.get(key), expected))
}

pub(crate) fn boolean(body: &Value, key: &str) -> Measurement<bool> {
    read(body, key, "boolean", Value::as_bool)
}

pub(crate) fn count(body: &Value, key: &str) -> Measurement<usize> {
    read(body, key, "nonnegative integer fitting usize", |value| {
        value.as_u64().and_then(|n| usize::try_from(n).ok())
    })
}

pub(crate) fn degraded_reasons(body: &Value) -> Measurement<Vec<ClassifiedReason>> {
    let values = read(
        body,
        "degraded_reasons",
        "array of strings",
        Value::as_array,
    )?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value.as_str().map(classify_degraded_reason).ok_or_else(|| {
                let mut issue =
                    FieldIssue::new(format!("degraded_reasons[{index}]"), Some(value), "string");
                issue.code = "invalid_element";
                issue
            })
        })
        .collect()
}
