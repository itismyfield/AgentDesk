//! `ShadowDerive` over captured transcript records: identity, plan and seal for each record.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::identity::{RecordFact, classify};
use super::seal::{SealOutcome, SealRegistry};
use super::unit_plan::{UnitPlan, plan};
use super::{
    CaptureBatch, CapturedRecord, DeriveOutput, ShadowDerive, ShadowUnit, SourceBinding, SourceId,
    SourceRange, UnitKey,
};

pub struct TranscriptDerive {
    seals: SealRegistry,
    clock: fn() -> DateTime<Utc>,
}

impl Default for TranscriptDerive {
    fn default() -> Self {
        Self::with_clock(Utc::now)
    }
}

impl TranscriptDerive {
    pub fn with_clock(clock: fn() -> DateTime<Utc>) -> Self {
        Self {
            seals: SealRegistry::default(),
            clock,
        }
    }

    fn derive_record(
        &mut self,
        binding: &SourceBinding,
        source: &SourceId,
        record: &CapturedRecord,
    ) -> Vec<DeriveOutput> {
        let mut out = Vec::new();
        if record.line.iter().all(u8::is_ascii_whitespace) {
            return out;
        }
        let facts = match serde_json::from_slice::<Value>(&record.line) {
            Ok(value) => classify(binding.provider, &value),
            Err(error) => vec![RecordFact::Blocked(format!("unparseable record: {error}"))],
        };
        let now = (self.clock)();
        let range = SourceRange {
            source: source.clone(),
            start: record.start,
            end: record.end,
        };
        let blocked = |reason| DeriveOutput::SchemaBlocked {
            channel_id: binding.channel_id,
            source_range: range.clone(),
            reason,
        };
        let key = |native_key, kind| UnitKey {
            channel_id: binding.channel_id,
            provider: binding.provider,
            native_key,
            kind,
        };
        for fact in facts {
            match fact {
                RecordFact::Unit(native_key, kind, content) => {
                    let unit_key = key(native_key, kind);
                    let planned = match plan(&content) {
                        Ok(planned) => planned,
                        Err(reason) => {
                            out.push(blocked(reason));
                            continue;
                        }
                    };
                    match self.seals.seal(&unit_key, &planned) {
                        SealOutcome::First => {}
                        SealOutcome::Repeat => continue,
                        SealOutcome::Conflict => {
                            let reason =
                                format!("sealed unit {} reappeared changed", unit_key.native_key);
                            out.push(blocked(reason));
                            continue;
                        }
                    }
                    out.push(match planned {
                        UnitPlan::Excluded(reason) => DeriveOutput::Excluded {
                            unit_key,
                            reason: reason.into(),
                        },
                        UnitPlan::Pieces(pieces) => {
                            let source_range = range.clone();
                            DeriveOutput::Sealed(ShadowUnit {
                                unit_key,
                                kind,
                                source_range,
                                sealed_at: now,
                                pieces,
                            })
                        }
                    });
                }
                RecordFact::Blocked(reason) => out.push(blocked(reason)),
                RecordFact::Announced(native_key, kind) => {
                    self.seals.announce(key(native_key, kind))
                }
                // Turn facts carry no unit.
                _ => {}
            }
        }
        out
    }
}

impl ShadowDerive for TranscriptDerive {
    fn derive(&mut self, binding: &SourceBinding, batch: &CaptureBatch) -> Vec<DeriveOutput> {
        let records = batch.records.iter();
        records
            .flat_map(|record| self.derive_record(binding, &batch.source, record))
            .collect()
    }

    fn unsealed(&self) -> Vec<UnitKey> {
        self.seals.unsealed()
    }
}

#[cfg(test)]
mod tests {
    use super::super::ShadowProvider::{self, Claude, Codex};
    use super::super::identity::UnitContent;
    use super::*;

    const CLAUDE: &str = "derive_claude_tui.jsonl";
    const CODEX: &str = "derive_codex_tui.jsonl";

    fn records(fixture: &str) -> Vec<CapturedRecord> {
        let root = env!("CARGO_MANIFEST_DIR");
        let text = std::fs::read_to_string(format!("{root}/tests/fixtures/tui_o_shadow/{fixture}"));
        let mut start = 0;
        let to_record = |line: &str| {
            let end = start + line.len() as u64 + 1;
            let line = line.as_bytes().to_vec();
            let record = CapturedRecord { start, end, line };
            start = end;
            record
        };
        text.expect("fixture").lines().map(to_record).collect()
    }

    fn source(name: &str) -> SourceId {
        let (session_id, path) = (name.into(), name.into());
        SourceId {
            session_id,
            path,
            dev: 1,
            ino: name.len() as u64,
        }
    }

    fn run(
        derive: &mut TranscriptDerive,
        provider: ShadowProvider,
        source: &SourceId,
        records: &[CapturedRecord],
    ) -> Vec<String> {
        let binding = SourceBinding {
            channel_id: 7,
            provider,
            source: source.clone(),
        };
        let captured_through = records.last().map_or(0, |record| record.end);
        let batch = CaptureBatch {
            source: source.clone(),
            records: records.to_vec(),
            captured_through,
        };
        derive
            .derive(&binding, &batch)
            .iter()
            .map(summary)
            .collect()
    }

    fn summary(output: &DeriveOutput) -> String {
        match output {
            DeriveOutput::Sealed(unit) => {
                let pieces = unit.pieces.len();
                format!(
                    "sealed {:?} {} pieces={pieces}",
                    unit.kind, unit.unit_key.native_key
                )
            }
            DeriveOutput::Excluded { unit_key, reason } => {
                format!(
                    "excluded {:?} {} {reason}",
                    unit_key.kind, unit_key.native_key
                )
            }
            DeriveOutput::SchemaBlocked { reason, .. } => format!("blocked {reason}"),
        }
    }

    /// Claude units key by (message.id, apiBlockIndex), each tool_result by tool_use_id with
    /// only errors posted, and synthetic error rows by uuid; unkeyed shapes block.
    #[test]
    fn claude_profile_units_follow_identity_rules() {
        let mut derive = TranscriptDerive::default();
        let outputs = run(&mut derive, Claude, &source("claude"), &records(CLAUDE));
        let expected = [
            "sealed Body msg_1:1 pieces=1",
            "sealed Tool msg_1:2 pieces=1",
            "sealed Tool msg_1:3 pieces=1",
            "sealed Tool msg_1:4 pieces=1",
            "excluded ToolResult toolu_a normal_tool_result",
            "excluded ToolResult toolu_b normal_tool_result",
            "sealed ToolResult toolu_c pieces=1",
            "sealed Body a-err pieces=1",
            "sealed Body msg_3:0 pieces=1",
            "blocked assistant row without a supported identity",
            "blocked tool_result without tool_use_id",
            "excluded ToolResult toolu_d normal_tool_result",
        ];
        assert_eq!(outputs, expected);
        assert!(derive.unsealed().is_empty());
    }

    /// Codex units key by payload.id and tool outputs by call_id (excluded); an announced
    /// AgentMessage stays unsealed until its response_item, and replays add no Body.
    #[test]
    fn codex_profile_units_and_unsealed_announcements() {
        let (mut derive, source) = (TranscriptDerive::default(), source("codex"));
        let records = records(CODEX);
        assert!(run(&mut derive, Codex, &source, &records[..5]).is_empty());
        let unsealed = |derive: &TranscriptDerive| -> Vec<String> {
            derive
                .unsealed()
                .into_iter()
                .map(|key| key.native_key)
                .collect()
        };
        assert_eq!(unsealed(&derive), ["msg_c1"]);
        let expected = [
            "sealed Body msg_c1 pieces=1",
            "sealed Tool ctc_1 pieces=1",
            "excluded ToolResult call_1 codex_tool_output",
            "sealed Tool fc_1 pieces=1",
            "excluded ToolResult call_2 codex_tool_output",
            "sealed Body msg_f1 pieces=1",
            "blocked response_item message without payload.id",
        ];
        assert_eq!(run(&mut derive, Codex, &source, &records[5..]), expected);
        assert_eq!(unsealed(&derive), ["msg_p2"]);
    }

    /// A forked source copying sealed rows reseals nothing, while a changed payload under a
    /// sealed key is surfaced instead of silently replacing the unit.
    #[test]
    fn sealed_units_repeat_silently_and_block_when_changed() {
        let records = records(CLAUDE);
        let mut derive = TranscriptDerive::default();
        run(&mut derive, Claude, &source("parent"), &records[..10]);
        assert!(run(&mut derive, Claude, &source("child"), &records[..10]).is_empty());
        let line = String::from_utf8(records[3].line.clone()).expect("utf8");
        let line = line.replace("세 번", "네 번").into_bytes();
        let changed = CapturedRecord {
            line,
            ..records[3].clone()
        };
        let outputs = run(&mut derive, Claude, &source("other"), &[changed]);
        assert_eq!(outputs, ["blocked sealed unit msg_1:1 reappeared changed"]);
    }

    /// Long bodies split exactly like Legacy, counting UTF-16 units; an over-limit piece blocks.
    #[test]
    fn split_pieces_follow_legacy_split_in_utf16_units() {
        let text = "한글 본문과 이모지 🎉 섞인 문장. ".repeat(160);
        let Ok(UnitPlan::Pieces(pieces)) = plan(&UnitContent::Payload(text.clone())) else {
            panic!("long body must split into pieces");
        };
        let legacy = crate::services::discord::formatting::split_for_shadow(text.trim());
        assert!(pieces.len() >= 2 && pieces.len() == legacy.len());
        for (piece, (legacy_text, _)) in pieces.iter().zip(&legacy) {
            assert_eq!(piece.units as usize, legacy_text.encode_utf16().count());
            assert!(piece.units <= 2000);
            let sha256 = <sha2::Sha256 as sha2::Digest>::digest(legacy_text);
            assert_eq!(piece.sha256, hex::encode(sha256));
        }
        let over = super::super::unit_plan::digest_pieces(vec![("x".repeat(2001), 2001)]);
        assert!(over.is_err());
    }
}
