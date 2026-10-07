//! Exact parent-record acceptance and ordered completion evidence.

use std::collections::HashMap;

use super::token;
use crate::services::tui_input::attempt::{AttemptMeta, Witness, WitnessKind};
use crate::services::tui_input::rows::{AttemptEvidence, Row, Rows};
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, ShadowProvider, SourceBinding, SourceId, SourceRange,
};
use crate::services::tui_o::writer::input_facts::{InputFacts, Ordered};
use serde_json::Value;

pub(super) fn frame(key: u64, row: &Row) -> Option<(String, Vec<u64>)> {
    if key == 0 {
        return None;
    }
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
    let mut marked = Vec::new();
    for line in rendered
        .lines()
        .filter(|line| line.starts_with("[adk:source:"))
    {
        let id = line
            .strip_prefix("[adk:source:")?
            .strip_suffix(']')?
            .parse::<u64>()
            .ok()?;
        if marked.contains(&id) {
            return None;
        }
        marked.push(id);
    }
    if marked.len() != ids.len()
        || ids.iter().any(|id| !marked.contains(id))
        || !rendered.contains(text)
        || rendered.lines().last() != Some("[adk:end]")
    {
        return None;
    }
    if let Some(segments) = row.input.get("source_text_segments") {
        let segments = segments.as_array()?;
        if segments.len() != ids.len()
            || ids.iter().any(|id| {
                segments
                    .iter()
                    .filter(|segment| {
                        segment["message_id"].as_u64() == Some(*id)
                            && segment["text"]
                                .as_str()
                                .is_some_and(|text| !text.is_empty() && rendered.contains(text))
                    })
                    .count()
                    != 1
            })
        {
            return None;
        }
    }
    Some((rendered, ids))
}

pub(crate) fn scan(
    evidence: &AttemptEvidence,
    completion: bool,
) -> Result<Option<(u64, Option<String>)>, String> {
    if evidence.binding.provider == ShadowProvider::Codex {
        crate::services::codex_tui::rollout_index::strict_parent_session(
            &evidence.binding.source.path,
            &evidence.binding.source.session_id,
        )
        .map_err(|e| e.to_string())?;
    }
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
                    // Input after ours, before its closer, leaves our turn's end unprovable.
                    RecordFact::Prompt(..) | RecordFact::TurnStart(_) if accepted.is_some() => {
                        return Err("foreign turn before the accepted one closed".into());
                    }
                    RecordFact::Prompt(_, text) => {
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
                    RecordFact::Blocked(reason) => return Err(reason),
                    _ => {}
                }
            }
        }
    }
    Err("witness scan budget exhausted".into())
}

/// What one read of the parent source proved about the ledger's tracked attempts.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Tracked {
    /// Registered witnesses in record order, one per row, kind and record.
    pub witnesses: Vec<(u64, Witness)>,
    /// Rows whose confirmed turn closed after the confirmation; `true` marks an abort.
    pub closed: Vec<(u64, bool)>,
    /// Rows whose registered token came back around another body.
    pub altered: Vec<u64>,
    /// Complete frames naming no attempt of this ledger, or one from before its anchor.
    pub foreign: usize,
    /// The read reached the source's end with no partial line left.
    pub complete: bool,
}

/// Text a provider record offers as input, with the witness a frame in it would be.
pub(crate) fn carriers(provider: ShadowProvider, record: &Value) -> Vec<(WitnessKind, String)> {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let joined = |parts: &[Value]| {
        let texts: Vec<&str> = parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect();
        texts.join("\n")
    };
    let output = |value: &Value| match value {
        Value::Array(parts) => Some(joined(parts)),
        value => text(value),
    };
    let mut found = Vec::new();
    match (provider, record["type"].as_str()) {
        (ShadowProvider::Claude, Some("queue-operation")) => {
            let kind = match record["operation"].as_str() {
                Some("enqueue") => WitnessKind::Queued,
                Some("remove" | "popAll") => WitnessKind::Removed,
                _ => return found,
            };
            found.extend(text(&record["content"]).map(|content| (kind, content)));
        }
        (ShadowProvider::Claude, Some("attachment")) => {
            let item = &record["attachment"];
            if item["type"] == "queued_command" && item["commandMode"] == "prompt" {
                found.extend(text(&item["prompt"]).map(|prompt| (WitnessKind::Attachment, prompt)));
            }
        }
        (ShadowProvider::Claude, Some("user")) => {
            let typed = record["isMeta"] != true;
            match &record["message"]["content"] {
                Value::String(content) if typed => found.push((WitnessKind::User, content.clone())),
                Value::Array(items) => {
                    let (results, typed_items): (Vec<&Value>, Vec<&Value>) =
                        items.iter().partition(|item| item["type"] == "tool_result");
                    for result in results {
                        found
                            .extend(output(&result["content"]).map(|out| (WitnessKind::Tool, out)));
                    }
                    if typed && !typed_items.is_empty() {
                        let texts: Vec<&str> = typed_items
                            .iter()
                            .filter_map(|item| item["text"].as_str())
                            .collect();
                        found.push((WitnessKind::User, texts.join("\n")));
                    }
                }
                _ => {}
            }
        }
        (ShadowProvider::Codex, Some("response_item")) => {
            let item = &record["payload"];
            match (item["type"].as_str(), item["role"].as_str()) {
                (Some("message"), Some("user")) => {
                    found.extend(
                        (item["content"].as_array())
                            .map(|parts| (WitnessKind::User, joined(parts))),
                    );
                }
                (Some("function_call_output" | "custom_tool_call_output"), _) => {
                    found.extend(output(&item["output"]).map(|out| (WitnessKind::Tool, out)));
                }
                _ => {}
            }
        }
        _ => {}
    }
    found
}

/// Reads the parent source from the lowest anchor of an open tracked attempt. Restarting there
/// makes a re-read a no-op and finds whatever a failed append left unrecorded.
pub(crate) fn scan_tracked(binding: &SourceBinding, rows: &Rows) -> Result<Tracked, String> {
    let mut registered = HashMap::new();
    let mut anchor: Option<u64> = None;
    for (key, open, meta) in rows.attempts() {
        if meta.source != binding.source {
            continue;
        }
        registered.insert(meta.token.as_str(), (key, meta));
        if open {
            anchor = Some(anchor.map_or(meta.anchor, |low| low.min(meta.anchor)));
        }
    }
    let Some(anchor) = anchor else {
        return Ok(Tracked {
            complete: true,
            ..Tracked::default()
        });
    };
    let mut scan = Scan {
        provider: binding.provider,
        rows,
        registered,
        // A read from inside the file may start inside a turn whose opener it never sees.
        span: (anchor > 0).then_some(0),
        spans: 0,
        turn: None,
        confirmed: Vec::new(),
        seen: Tracked::default(),
    };
    let mut facts = InputFacts::open_at(binding.clone(), anchor)?;
    let mut through = anchor;
    for _ in 0..64 {
        let read = facts.poll(1024 * 1024)?.through;
        for event in facts.events() {
            scan.event(&binding.source, event);
        }
        if facts.caught_up()? {
            scan.seen.complete = true;
            break;
        }
        if read == through {
            break;
        }
        through = read;
    }
    Ok(scan.seen)
}

// Turns never nest in a parent source, so a close ends exactly the span open before it.
struct Scan<'a> {
    provider: ShadowProvider,
    rows: &'a Rows,
    registered: HashMap<&'a str, (u64, &'a AttemptMeta)>,
    span: Option<usize>,
    spans: usize,
    turn: Option<String>,
    confirmed: Vec<(u64, usize)>,
    seen: Tracked,
}

impl Scan<'_> {
    fn event(&mut self, source: &SourceId, event: &Ordered) {
        match event {
            Ordered::Opened { native_turn_id, .. } => {
                self.spans += 1;
                self.span = Some(self.spans);
                self.turn = native_turn_id.clone();
            }
            Ordered::Closed { aborted, .. } => {
                let span = self.span.take();
                self.turn = None;
                let closed = &mut self.seen.closed;
                self.confirmed.retain(|(key, of)| {
                    let ends = span == Some(*of);
                    if ends {
                        closed.push((*key, *aborted));
                    }
                    !ends
                });
            }
            Ordered::Input {
                range,
                record_key,
                record,
            } => {
                let range = SourceRange {
                    source: source.clone(),
                    start: range.0,
                    end: range.1,
                };
                for (kind, text) in carriers(self.provider, record) {
                    for frame in token::frames(token::profile(self.provider), &text) {
                        self.frame(kind, &frame, &range, record_key);
                    }
                }
            }
        }
    }

    // Only an exact registered frame after its anchor names a row, once per record and kind.
    fn frame(
        &mut self,
        kind: WitnessKind,
        frame: &token::Framed,
        range: &SourceRange,
        record_key: &Option<String>,
    ) {
        let Some(&(key, meta)) = self.registered.get(frame.token.as_str()) else {
            self.seen.foreign += 1;
            return;
        };
        if range.start < meta.anchor {
            self.seen.foreign += 1;
            return;
        }
        let profile = meta.frame_profile.as_deref();
        if frame.digest != meta.frame_digest
            || profile.is_some_and(|profile| profile != token::profile(self.provider))
        {
            if !self.seen.altered.contains(&key) {
                self.seen.altered.push(key);
            }
            return;
        }
        let mut witness = Witness {
            generation: meta.generation,
            token: meta.token.clone(),
            kind,
            range: Some(range.clone()),
            record_key: record_key.clone(),
            turn_ref: None,
        };
        if (self.seen.witnesses.iter()).any(|(_, seen)| seen.same_record(&witness)) {
            return;
        }
        if kind.confirms_input() {
            // A later read may start after the turn's opener; the recorded turn still names it.
            let recorded = (self.rows.row(key).into_iter())
                .flat_map(|row| row.witnesses.iter())
                .find(|seen| seen.witness.same_record(&witness))
                .and_then(|seen| seen.witness.turn_ref.clone());
            witness.turn_ref = recorded.or_else(|| self.turn.clone());
            if let Some(span) = self.span {
                self.confirmed.push((key, span));
            }
        }
        self.seen.witnesses.push((key, witness));
    }
}
