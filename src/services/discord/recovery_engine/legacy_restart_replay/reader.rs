use std::io::{BufRead, BufReader};

use crate::services::agent_protocol::StreamMessage;
use crate::services::codex_tui::rollout_tail::RolloutRecordDecoder;

use super::{CoordinateHold, proof::ProofPlan};

/// Continues the exact opened source and decoder that established the durable prefix.
pub(in crate::services::discord::recovery_engine) struct VerifiedReader {
    reader: BufReader<std::fs::File>,
    decoder: RolloutRecordDecoder,
    native_turn_id: String,
    session_id: String,
    composed_prefix: String,
    pending: Vec<u8>,
    residual: Vec<StreamMessage>,
    cursor: u64,
    finished: bool,
}

impl VerifiedReader {
    pub(in crate::services::discord::recovery_engine) fn from_plan(plan: ProofPlan) -> Self {
        let composed_prefix = plan.decoder.response()[..plan.witness.prefix_len].to_owned();
        let mut residual = Vec::new();
        if !plan.residual_text.is_empty() {
            residual.push(StreamMessage::Text {
                content: plan.residual_text,
            });
        }
        residual.extend(plan.residual_messages);
        Self {
            reader: plan.reader,
            decoder: plan.decoder,
            native_turn_id: plan.witness.native_turn_id,
            session_id: plan.witness.session_id,
            composed_prefix,
            cursor: plan.witness.prefix_record_end,
            pending: Vec::new(),
            finished: false,
            residual,
        }
    }

    pub(in crate::services::discord::recovery_engine) fn cursor(&self) -> u64 {
        self.cursor
    }

    pub(in crate::services::discord::recovery_engine) fn composed_prefix(&self) -> &str {
        &self.composed_prefix
    }

    pub(in crate::services::discord::recovery_engine) fn finished(&self) -> bool {
        self.finished
    }

    pub(in crate::services::discord::recovery_engine) fn poll(
        &mut self,
    ) -> Result<Vec<StreamMessage>, CoordinateHold> {
        if !self.residual.is_empty() {
            let messages = std::mem::take(&mut self.residual);
            self.finished = messages
                .iter()
                .any(|m| matches!(m, StreamMessage::Done { .. }));
            return Ok(messages);
        }
        if self.finished {
            return Ok(Vec::new());
        }
        if self
            .reader
            .get_ref()
            .metadata()
            .map_err(|_| CoordinateHold::RetryInstall("source_stat"))?
            .len()
            < self.cursor + self.pending.len() as u64
        {
            return Err(CoordinateHold::RetryInstall("source_truncated"));
        }
        self.reader
            .read_until(b'\n', &mut self.pending)
            .map_err(|_| CoordinateHold::RetryInstall("source_read"))?;
        if self.pending.last() != Some(&b'\n') {
            return Ok(Vec::new());
        }
        let record: serde_json::Value = std::str::from_utf8(&self.pending)
            .ok()
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or(CoordinateHold::NeedsEvidence("malformed_record"))?;
        if record.get("type").and_then(serde_json::Value::as_str) == Some("session_meta")
            && record
                .pointer("/payload/id")
                .and_then(serde_json::Value::as_str)
                != Some(self.session_id.as_str())
        {
            return Err(CoordinateHold::NeedsEvidence("session_mismatch"));
        }
        let opens = record
            .pointer("/payload/type")
            .and_then(serde_json::Value::as_str)
            == Some("task_started")
            || record.get("type").and_then(serde_json::Value::as_str) == Some("turn_context");
        if opens {
            self.finished = true;
            return Ok(Vec::new());
        }
        if record
            .pointer("/payload/type")
            .and_then(serde_json::Value::as_str)
            == Some("task_complete")
            && record
                .pointer("/payload/turn_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|turn| turn != self.native_turn_id)
        {
            return Err(CoordinateHold::NeedsEvidence("terminal_turn_mismatch"));
        }
        let before = self.decoder.response().to_owned();
        let mut messages = self.decoder.decode(&record).unwrap_or_default();
        if messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. }))
        {
            let after = self.decoder.response();
            if !after.starts_with(&self.composed_prefix) {
                return Err(CoordinateHold::NeedsEvidence("terminal_prefix_mismatch"));
            }
            // Terminal fallback text is cumulative; continue the response without replaying its prefix.
            let suffix = after.strip_prefix(&before).map(str::to_owned);
            messages.retain(|m| !matches!(m, StreamMessage::Text { .. }));
            if let Some(content) = suffix.filter(|s| !s.is_empty()) {
                messages.insert(0, StreamMessage::Text { content });
            }
        }
        self.cursor += self.pending.len() as u64;
        self.pending.clear();
        self.finished = messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Done { .. }));
        Ok(messages)
    }
}
