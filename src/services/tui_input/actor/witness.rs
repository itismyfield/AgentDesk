//! Exact parent-record acceptance and ordered completion evidence.

use crate::services::tui_input::rows::{AttemptEvidence, Row};
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, ShadowProvider};
use serde_json::Value;

pub(super) fn frame(key: u64, row: &Row) -> Option<(String, Vec<u64>)> {
    let mut ids = vec![key];
    if let Some(values) = row.input.get("source_message_ids") {
        for value in values.as_array()? {
            let id = value.as_u64().filter(|id| *id != 0)?;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let text = row.input.get("text")?.as_str()?;
    let marks = ids
        .iter()
        .map(|id| format!("[adk:source:{id}]"))
        .collect::<Vec<_>>()
        .join("\n");
    let rendered = row
        .input
        .get("rendered_prompt")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{marks}\n{text}\n[adk:end]"));
    if ids
        .iter()
        .any(|id| !rendered.contains(&format!("[adk:source:{id}]")))
    {
        return None;
    }
    if let Some(segments) = row
        .input
        .get("source_text_segments")
        .and_then(Value::as_array)
    {
        if ids.iter().any(|id| {
            !segments.iter().any(|segment| {
                segment["message_id"].as_u64() == Some(*id)
                    && segment["text"]
                        .as_str()
                        .is_some_and(|text| rendered.contains(text))
            })
        }) {
            return None;
        }
    }
    Some((rendered, ids))
}

pub(super) fn scan(
    evidence: &AttemptEvidence,
    completion: bool,
) -> Result<Option<(u64, Option<String>)>, String> {
    let mut capture = SourceCapture::open(evidence.binding.source.clone(), evidence.eof)
        .map_err(|e| e.to_string())?;
    let mut accepted = None;
    let mut native = None;
    for _ in 0..64 {
        let batch = match capture.poll(1024 * 1024) {
            CaptureOutcome::Batch(batch) => batch,
            CaptureOutcome::Anomaly(anomaly) => return Err(anomaly.detail),
        };
        if batch.records.is_empty() {
            return Ok(None);
        }
        for record in batch.records {
            let value: Value = serde_json::from_slice(&record.line).map_err(|e| e.to_string())?;
            if value.get("isSidechain") == Some(&Value::Bool(true)) {
                return Err("sidechain witness".into());
            }
            for fact in classify(evidence.binding.provider, &value) {
                match fact {
                    RecordFact::TurnStart(id) if accepted.is_none() => native = id,
                    RecordFact::Prompt(_, text) => {
                        if accepted.is_some() {
                            return Ok(None);
                        }
                        if text != evidence.rendered_prompt {
                            continue;
                        }
                        if evidence.binding.provider == ShadowProvider::Claude {
                            native = row_key(&value);
                        } else if let Some(id) = value["payload"]["turn_id"].as_str() {
                            native = Some(id.to_owned());
                        }
                        if !completion {
                            return Ok(Some((record.end, native)));
                        }
                        if evidence.record_end != Some(record.end)
                            || evidence.native_turn_id != native
                        {
                            return Ok(None);
                        }
                        accepted = Some(record.end);
                    }
                    RecordFact::Idle(id) if accepted.is_some() => {
                        let matched = match evidence.binding.provider {
                            ShadowProvider::Claude => id.is_none() && native.is_some(),
                            ShadowProvider::Codex => id.is_some() && id == native,
                        };
                        if matched {
                            return Ok(Some((accepted.unwrap(), native)));
                        }
                    }
                    RecordFact::TurnStart(_) if accepted.is_some() => return Ok(None),
                    RecordFact::Blocked(reason) => return Err(reason),
                    _ => {}
                }
            }
        }
    }
    Err("witness scan budget exhausted".into())
}
