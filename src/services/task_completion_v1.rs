//! Dormant JSON-only task-completion wire contract for #4912.
//!
//! This module validates shadow receipts only. It does not grant completion,
//! lifecycle, delivery, or rendering authority.

use icu_properties::props::{BidiControl, DefaultIgnorableCodePoint, GeneralCategory};
use icu_properties::{CodePointMapData, CodePointSetData};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;

const TYPE_LITERAL: &str = "system";
const SUBTYPE_LITERAL: &str = "task_completion";
const SCHEMA_LITERAL: &str = "task_completion.v1";
const CODEX_PRODUCER_LITERAL: &str = "agentdesk.codex_tmux_wrapper";
const CODEX_BACKGROUND_TASK_ID: &str = "codex-background-event";
const MAX_ID_BYTES: usize = 256;
const REQUIRED_FIELDS: [&str; 8] = [
    "type",
    "subtype",
    "schema",
    "producer",
    "kind",
    "status",
    "task_id",
    "tool_use_id",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskCompletionKind {
    Background,
    Subagent,
    MonitorAutoTurn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskCompletionStatus {
    Completed,
    Failed,
    Killed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum TaskCompletionProducer {
    #[serde(rename = "agentdesk.codex_tmux_wrapper")]
    CodexTmuxWrapper,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct ToolUseId(String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TaskCompletionV1 {
    #[serde(rename = "type")]
    frame_type: &'static str,
    subtype: &'static str,
    schema: &'static str,
    producer: TaskCompletionProducer,
    kind: TaskCompletionKind,
    status: TaskCompletionStatus,
    task_id: TaskId,
    tool_use_id: Option<ToolUseId>,
}

impl TaskCompletionV1 {
    pub(crate) fn codex_background_completed() -> Self {
        Self {
            frame_type: TYPE_LITERAL,
            subtype: SUBTYPE_LITERAL,
            schema: SCHEMA_LITERAL,
            producer: TaskCompletionProducer::CodexTmuxWrapper,
            kind: TaskCompletionKind::Background,
            status: TaskCompletionStatus::Completed,
            task_id: TaskId(CODEX_BACKGROUND_TASK_ID.to_string()),
            tool_use_id: None,
        }
    }

    pub(crate) fn encode_canonical(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskCompletionV1Rejection {
    MalformedJson,
    DuplicateField,
    UnknownField,
    MissingField,
    WrongType,
    WrongLiteral,
    MalformedId,
    TrailingDocument,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskCompletionV1Admission {
    Legacy,
    TypedCandidate(TaskCompletionV1),
    Rejected(TaskCompletionV1Rejection),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CapturedValue {
    String(String),
    Null,
    Other,
}

impl<'de> Deserialize<'de> for CapturedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(CapturedValueVisitor)
    }
}

struct CapturedValueVisitor;

impl<'de> Visitor<'de> for CapturedValueVisitor {
    type Value = CapturedValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(CapturedValue::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(CapturedValue::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(CapturedValue::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(CapturedValue::Null)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(CapturedValue::Other)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(CapturedValue::Other)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(CapturedValue::Other)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(CapturedValue::Other)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(CapturedValue::Other)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(CapturedValue::Other)
    }
}

#[derive(Default)]
struct AdmissionProbe {
    fields: HashMap<String, CapturedValue>,
    duplicate: bool,
    unknown: bool,
    saw_object: bool,
    saw_candidate_marker: bool,
}

impl AdmissionProbe {
    fn is_candidate(&self) -> bool {
        self.saw_candidate_marker
    }

    fn observe_candidate_marker(&mut self, key: &str, value: &CapturedValue) {
        let CapturedValue::String(value) = value else {
            return;
        };
        self.saw_candidate_marker |= (key == "schema" && value == SCHEMA_LITERAL)
            || (key == "subtype" && value == SUBTYPE_LITERAL);
    }

    fn required_text(&self, key: &str) -> Result<&str, TaskCompletionV1Rejection> {
        match self.fields.get(key) {
            Some(CapturedValue::String(value)) => Ok(value),
            _ => Err(TaskCompletionV1Rejection::WrongType),
        }
    }

    fn validate(self) -> TaskCompletionV1Admission {
        if self.duplicate {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::DuplicateField);
        }
        if self.unknown {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::UnknownField);
        }
        if REQUIRED_FIELDS
            .iter()
            .any(|field| !self.fields.contains_key(*field))
        {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MissingField);
        }

        let frame_type = match self.required_text("type") {
            Ok(value) => value,
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        let subtype = match self.required_text("subtype") {
            Ok(value) => value,
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        let schema = match self.required_text("schema") {
            Ok(value) => value,
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        let producer = match self.required_text("producer") {
            Ok(value) => value,
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        if frame_type != TYPE_LITERAL
            || subtype != SUBTYPE_LITERAL
            || schema != SCHEMA_LITERAL
            || producer != CODEX_PRODUCER_LITERAL
        {
            return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::WrongLiteral);
        }

        let kind = match self.required_text("kind") {
            Ok("background") => TaskCompletionKind::Background,
            Ok("subagent") => TaskCompletionKind::Subagent,
            Ok("monitor_auto_turn") => TaskCompletionKind::MonitorAutoTurn,
            Ok(_) => {
                return TaskCompletionV1Admission::Rejected(
                    TaskCompletionV1Rejection::WrongLiteral,
                );
            }
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        let status = match self.required_text("status") {
            Ok("completed") => TaskCompletionStatus::Completed,
            Ok("failed") => TaskCompletionStatus::Failed,
            Ok("killed") => TaskCompletionStatus::Killed,
            Ok(_) => {
                return TaskCompletionV1Admission::Rejected(
                    TaskCompletionV1Rejection::WrongLiteral,
                );
            }
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        let task_id = match self.required_text("task_id") {
            Ok(value) if valid_id(value) => TaskId(value.to_string()),
            Ok(_) => {
                return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MalformedId);
            }
            Err(reason) => return TaskCompletionV1Admission::Rejected(reason),
        };
        let tool_use_id = match self.fields.get("tool_use_id") {
            Some(CapturedValue::Null) => None,
            Some(CapturedValue::String(value)) if valid_id(value) => Some(ToolUseId(value.clone())),
            Some(CapturedValue::String(_)) => {
                return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MalformedId);
            }
            _ => {
                return TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::WrongType);
            }
        };

        TaskCompletionV1Admission::TypedCandidate(TaskCompletionV1 {
            frame_type: TYPE_LITERAL,
            subtype: SUBTYPE_LITERAL,
            schema: SCHEMA_LITERAL,
            producer: TaskCompletionProducer::CodexTmuxWrapper,
            kind,
            status,
            task_id,
            tool_use_id,
        })
    }
}

struct AdmissionVisitor<'a> {
    probe: &'a mut AdmissionProbe,
}

impl<'de> Visitor<'de> for AdmissionVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.probe.saw_object = true;
        let mut decoded_keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let duplicate = !decoded_keys.insert(key.clone());
            let known = REQUIRED_FIELDS.contains(&key.as_str());
            let value = map.next_value::<CapturedValue>()?;
            self.probe.observe_candidate_marker(&key, &value);
            if duplicate {
                self.probe.duplicate = true;
            }
            if known {
                self.probe.fields.entry(key).or_insert(value);
            } else {
                self.probe.unknown = true;
            }
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
}

pub(crate) fn admit_raw(line: &str) -> TaskCompletionV1Admission {
    let mut probe = AdmissionProbe::default();
    let mut deserializer = serde_json::Deserializer::from_str(line);
    let parsed = deserializer.deserialize_any(AdmissionVisitor { probe: &mut probe });
    if parsed.is_err() {
        return if probe.is_candidate() {
            TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::MalformedJson)
        } else {
            TaskCompletionV1Admission::Legacy
        };
    }
    if deserializer.end().is_err() {
        return if probe.is_candidate() {
            TaskCompletionV1Admission::Rejected(TaskCompletionV1Rejection::TrailingDocument)
        } else {
            TaskCompletionV1Admission::Legacy
        };
    }
    if !probe.saw_object || !probe.is_candidate() {
        return TaskCompletionV1Admission::Legacy;
    }
    probe.validate()
}

fn forbidden_id_character(character: char) -> bool {
    character.is_whitespace()
        || character.is_control()
        || CodePointMapData::<GeneralCategory>::new().get(character) == GeneralCategory::Format
        || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(character)
        || CodePointSetData::new::<BidiControl>().contains(character)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(forbidden_id_character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn canonical() -> String {
        TaskCompletionV1::codex_background_completed()
            .encode_canonical()
            .unwrap()
    }

    fn assert_rejected(raw: &str) {
        assert!(
            matches!(admit_raw(raw), TaskCompletionV1Admission::Rejected(_)),
            "expected strict rejection: {raw}"
        );
    }

    fn with_id(key: &str, id: &str) -> String {
        let mut mutated = serde_json::from_str::<Value>(&canonical()).unwrap();
        mutated[key] = Value::String(id.to_string());
        mutated.to_string()
    }

    #[test]
    fn canonical_round_trip_and_field_permutation() {
        let frame = TaskCompletionV1::codex_background_completed();
        assert_eq!(
            admit_raw(&frame.encode_canonical().unwrap()),
            TaskCompletionV1Admission::TypedCandidate(frame)
        );
        assert!(matches!(
            admit_raw(
                r#"{"tool_use_id":null,"task_id":"codex-background-event","status":"completed","kind":"background","producer":"agentdesk.codex_tmux_wrapper","schema":"task_completion.v1","subtype":"task_completion","type":"system"}"#
            ),
            TaskCompletionV1Admission::TypedCandidate(_)
        ));
    }

    #[test]
    fn duplicate_and_escaped_duplicate_keys_are_rejected() {
        for key in REQUIRED_FIELDS {
            let raw = canonical();
            let value = serde_json::from_str::<Value>(&raw).unwrap()[key].clone();
            let insertion = format!(",\"{key}\":{value}");
            assert_rejected(&raw.replacen('}', &format!("{insertion}}}"), 1));
        }
        assert_rejected(
            r#"{"type":"system","subtype":"task_completion","schema":"task_completion.v1","producer":"agentdesk.codex_tmux_wrapper","kind":"background","status":"completed","status":"completed","task_id":"codex-background-event","tool_use_id":null}"#,
        );
        let escaped_duplicate = canonical().replacen(
            "\"status\":\"completed\"",
            "\"status\":\"completed\",\"sta\\u0074us\":\"completed\"",
            1,
        );
        assert_rejected(&escaped_duplicate);
    }

    #[test]
    fn missing_and_unknown_field_mutations_are_rejected() {
        let value = serde_json::from_str::<Value>(&canonical()).unwrap();
        for key in REQUIRED_FIELDS {
            let mut mutated = value.clone();
            mutated.as_object_mut().unwrap().remove(key);
            assert_rejected(&mutated.to_string());
        }
        for key in [
            "summary",
            "result",
            "label",
            "description",
            "url",
            "<@user>",
        ] {
            let mut mutated = value.clone();
            mutated[key] = Value::String("untrusted".to_string());
            assert_rejected(&mutated.to_string());
        }
    }

    #[test]
    fn literal_enum_and_type_mutations_are_rejected() {
        let base = serde_json::from_str::<Value>(&canonical()).unwrap();
        for (key, value) in [
            ("type", "System"),
            ("subtype", "task-completion"),
            ("schema", "task_completion.V1"),
            ("producer", "codex"),
            ("kind", "Background"),
            ("status", "success"),
        ] {
            let mut mutated = base.clone();
            mutated[key] = Value::String(value.to_string());
            assert_rejected(&mutated.to_string());
        }
        for key in [
            "type", "subtype", "schema", "producer", "kind", "status", "task_id",
        ] {
            let mut mutated = base.clone();
            mutated[key] = Value::Bool(true);
            if key == "schema" {
                mutated["subtype"] = Value::String(SUBTYPE_LITERAL.to_string());
            }
            if key == "subtype" {
                mutated["schema"] = Value::String(SCHEMA_LITERAL.to_string());
            }
            assert_rejected(&mutated.to_string());
        }
        let mut mutated = base;
        mutated["tool_use_id"] = Value::Bool(true);
        assert_rejected(&mutated.to_string());
    }

    #[test]
    fn whitespace_controls_and_oversized_ids_are_rejected_for_both_id_fields() {
        let oversized_ascii = "x".repeat(MAX_ID_BYTES + 1);
        let oversized_multibyte = format!("{}x", "é".repeat(MAX_ID_BYTES / 2));
        for id in [
            "",
            " leading",
            "trailing ",
            "internal space",
            "internal\ttab",
            "line\nbreak",
            "non\u{00a0}breaking",
            "em\u{2003}space",
            "narrow\u{202f}no-break",
            "ideographic\u{3000}space",
            "control\u{0007}bell",
            "control\u{007f}delete",
            oversized_ascii.as_str(),
            oversized_multibyte.as_str(),
        ] {
            for key in ["task_id", "tool_use_id"] {
                assert_rejected(&with_id(key, id));
            }
        }
    }

    // Unicode 16.0 / ICU4X 2.1.2 golden fingerprints. Each SHA-256 hashes the
    // sorted member scalars as four-byte big-endian integers. Regenerate these
    // literals only during an intentional Unicode profile upgrade.
    const FORMAT_GOLDEN: (usize, &str) = (
        170,
        "9b8328757c21f609db32f87ac55fb30060408c243041aa9ff602d8d79db0f471",
    );
    const DEFAULT_IGNORABLE_GOLDEN: (usize, &str) = (
        4_174,
        "af2bc3d9df7efe853444acbdac7278d97dea892bc187b24b50cca7e452f3077a",
    );
    const BIDI_CONTROL_GOLDEN: (usize, &str) = (
        12,
        "66619508d89567de1a781280be43dcf7522f806066355a6f9b80811d195c13d2",
    );
    const REJECTED_UNION_GOLDEN: (usize, &str) = (
        4_206,
        "35739b1ac598a2cc0e26a7ed6b92e96d24a65e63746f02fda41503060a0a4991",
    );

    fn property_scalars(predicate: impl Fn(char) -> bool) -> Vec<char> {
        (0..=char::MAX as u32)
            .filter_map(char::from_u32)
            .filter(|character| predicate(*character))
            .collect()
    }

    fn scalar_fingerprint(scalars: &[char]) -> String {
        use sha2::{Digest, Sha256};

        let mut digest = Sha256::new();
        for character in scalars {
            digest.update((*character as u32).to_be_bytes());
        }
        hex::encode(digest.finalize())
    }

    fn assert_golden_set(label: &str, scalars: &[char], golden: (usize, &str)) {
        assert_eq!(
            (scalars.len(), scalar_fingerprint(scalars)),
            (golden.0, golden.1.to_string()),
            "{label} Unicode 16.0 fingerprint drifted"
        );
    }

    #[test]
    fn unicode_property_sets_match_pinned_unicode_16_golden_and_raw_admission_rejects_union() {
        let general_category = CodePointMapData::<GeneralCategory>::new();
        let default_ignorable = CodePointSetData::new::<DefaultIgnorableCodePoint>();
        let bidi_control = CodePointSetData::new::<BidiControl>();

        let format = property_scalars(|character| {
            general_category.get(character) == GeneralCategory::Format
        });
        let default_ignorable = property_scalars(|character| default_ignorable.contains(character));
        let bidi_control = property_scalars(|character| bidi_control.contains(character));
        let rejected_union = property_scalars(|character| {
            general_category.get(character) == GeneralCategory::Format
                || CodePointSetData::new::<DefaultIgnorableCodePoint>().contains(character)
                || CodePointSetData::new::<BidiControl>().contains(character)
        });

        assert_golden_set("General_Category=Format", &format, FORMAT_GOLDEN);
        assert_golden_set(
            "Default_Ignorable_Code_Point",
            &default_ignorable,
            DEFAULT_IGNORABLE_GOLDEN,
        );
        assert_golden_set("Bidi_Control", &bidi_control, BIDI_CONTROL_GOLDEN);
        assert_golden_set("rejected union", &rejected_union, REJECTED_UNION_GOLDEN);

        for character in rejected_union {
            for key in ["task_id", "tool_use_id"] {
                assert_rejected(&with_id(key, &format!("visible{character}suffix")));
            }
        }
    }

    #[test]
    fn explicit_invisible_unicode_witnesses_hit_the_production_predicate() {
        for character in ['\u{202e}', '\u{200b}', '\u{feff}', '\u{2066}'] {
            assert!(forbidden_id_character(character));
        }
    }

    #[test]
    fn format_and_default_ignorable_json_mutations_reject_both_id_fields() {
        for character in [
            '\u{00ad}',
            '\u{034f}',
            '\u{061c}',
            '\u{180b}',
            '\u{200b}',
            '\u{200e}',
            '\u{202e}',
            '\u{2066}',
            '\u{2069}',
            '\u{fe00}',
            '\u{feff}',
            '\u{e0001}',
            '\u{e0020}',
            '\u{e007f}',
            '\u{e0100}',
        ] {
            for key in ["task_id", "tool_use_id"] {
                assert_rejected(&with_id(key, &format!("opaque{character}id")));
            }
        }
    }

    #[test]
    fn escaped_whitespace_ids_are_rejected_before_value_normalization() {
        for key in ["task_id", "tool_use_id"] {
            for escaped_id in [
                r#""internal space""#,
                r#""internal\ttab""#,
                r#""line\nbreak""#,
                r#""non breaking""#,
                r#""em space""#,
            ] {
                let canonical = canonical();
                let raw = canonical.replacen(
                    &format!(
                        "\"{key}\":{}",
                        if key == "task_id" {
                            r#""codex-background-event""#
                        } else {
                            "null"
                        }
                    ),
                    &format!("\"{key}\":{escaped_id}"),
                    1,
                );
                assert_rejected(&raw);
            }
        }
    }

    #[test]
    fn opaque_punctuation_and_utf8_byte_bound_are_admitted_for_both_id_fields() {
        let exact_ascii = "x".repeat(MAX_ID_BYTES);
        let exact_multibyte = "é".repeat(MAX_ID_BYTES / 2);
        for id in [
            "task-id_1:segment",
            "opaque.id/@resource+token=1,2;3",
            "작업-識別子_é:δ",
            "cafe\u{0301}-नमस्ते",
            exact_ascii.as_str(),
            exact_multibyte.as_str(),
        ] {
            for key in ["task_id", "tool_use_id"] {
                assert!(
                    matches!(
                        admit_raw(&with_id(key, id)),
                        TaskCompletionV1Admission::TypedCandidate(_)
                    ),
                    "expected opaque id admission for {key}: {id:?}"
                );
            }
        }
    }

    #[test]
    fn trailing_truncated_and_legacy_inputs_do_not_downgrade_candidates() {
        assert_rejected(&format!("{} {{}}", canonical()));
        assert_rejected(
            r#"{"type":"system","subtype":"task_completion","schema":"task_completion.v1""#,
        );
        assert_rejected(
            r#"{"schema":"task_completion.v1","schema":"other","type":"system","subtype":"task_notification","producer":"agentdesk.codex_tmux_wrapper","kind":"background","status":"completed","task_id":"codex-background-event","tool_use_id":null}"#,
        );
        assert!(matches!(
            admit_raw(r#"{"type":"system","subtype":"task_notification","summary":"legacy"}"#),
            TaskCompletionV1Admission::Legacy
        ));
        assert!(matches!(
            admit_raw("<task-notification>legacy</task-notification>"),
            TaskCompletionV1Admission::Legacy
        ));
    }
}
