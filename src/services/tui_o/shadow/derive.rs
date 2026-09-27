//! `ShadowDerive` over captured transcript records: identity, plan and seal for each record,
//! plus the native turns its Idle observations close.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use super::identity::{RecordFact, classify, native_time, row_key};
use super::seal::{SealOutcome, SealRegistry, TurnEvent, TurnSpan, TurnTracker};
use super::unit_plan::{UnitPlan, plan};
use super::{
    CaptureBatch, CapturedRecord, DeriveOutput, ShadowDerive, ShadowTurn, ShadowUnit,
    SourceBinding, SourceId, SourceRange, UnitKey,
};

/// A closer written this long before attach or window start is treated as replayed history.
const CLOSER_START_SKEW: TimeDelta = TimeDelta::seconds(60);

/// Idle observations that closed no countable turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnCounters {
    pub stray_e: u64,
    pub edge_turn_uncounted: u64,
}

pub struct TranscriptDerive {
    seals: SealRegistry,
    turns: HashMap<(u64, SourceId), TurnTracker>,
    /// Per source: start boundary (attach or window-start extent) and attach time.
    attached: HashMap<SourceId, (u64, DateTime<Utc>)>,
    window_t0: Option<DateTime<Utc>>,
    /// Source that first showed each opener or closer key; a key from another source is inherited.
    turn_key_sources: HashMap<(u64, String), SourceId>,
    counters: TurnCounters,
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
            turns: HashMap::new(),
            attached: HashMap::new(),
            window_t0: None,
            turn_key_sources: HashMap::new(),
            counters: TurnCounters::default(),
            clock,
        }
    }

    /// Records the file size seen when `source` was first attached; later calls are ignored.
    pub fn attach(&mut self, source: &SourceId, attach_extent: u64, attached_at: DateTime<Utc>) {
        let first = (attach_extent, attached_at);
        self.attached.entry(source.clone()).or_insert(first);
    }

    /// Moves a source's boundary to its size at window start `t0`; records below it are history.
    pub fn window_start(&mut self, t0: DateTime<Utc>, source: &SourceId, window_start_extent: u64) {
        self.window_t0 = Some(t0);
        let boundary = self.attached.entry(source.clone()).or_insert((0, t0));
        boundary.0 = window_start_extent;
    }

    pub fn turn_counters(&self) -> TurnCounters {
        self.counters
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
        let value = serde_json::from_slice::<Value>(&record.line);
        let facts = match &value {
            Ok(value) => classify(binding.provider, value),
            Err(error) => vec![RecordFact::Blocked(format!("unparseable record: {error}"))],
        };
        let value = value.unwrap_or(Value::Null);
        let (now, row) = ((self.clock)(), row_key(&value));
        let range = SourceRange {
            source: source.clone(),
            start: record.start,
            end: record.end,
        };
        let floor = |(extent, at): (u64, DateTime<Utc>)| {
            (extent, self.window_t0.map_or(at, |t0| t0.max(at)))
        };
        let attached = self.attached.get(source).copied().map(floor);
        let historical = attached.is_some_and(|(extent, _)| record.start < extent);
        let blocked = |reason| DeriveOutput::SchemaBlocked {
            channel_id: binding.channel_id,
            source_range: range.clone(),
            reason,
        };
        let turns = self.turns.entry((binding.channel_id, source.clone()));
        let turns = turns.or_insert_with(|| TurnTracker::starting_at(record.start));
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
                        SealOutcome::Repeat if !historical => {
                            turns.add_unit(&unit_key);
                            continue;
                        }
                        SealOutcome::Repeat => continue,
                        SealOutcome::Conflict => {
                            let reason =
                                format!("sealed unit {} reappeared changed", unit_key.native_key);
                            out.push(blocked(reason));
                            continue;
                        }
                    }
                    // History is sealed with its real plan so a later copy still compares.
                    out.push(match planned {
                        _ if historical => DeriveOutput::Excluded {
                            unit_key,
                            reason: "historical".into(),
                        },
                        UnitPlan::Excluded(reason) => {
                            turns.add_unit(&unit_key);
                            DeriveOutput::Excluded {
                                unit_key,
                                reason: reason.into(),
                            }
                        }
                        UnitPlan::Pieces(pieces) => {
                            turns.add_unit(&unit_key);
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
                    let unit_key = key(native_key, kind);
                    turns.add_unit(&unit_key);
                    self.seals.announce(unit_key);
                }
                fact => match turns.observe(&fact, row.as_ref(), (record.start, record.end), now) {
                    TurnEvent::None => {}
                    TurnEvent::StrayIdle => self.counters.stray_e += 1,
                    TurnEvent::EdgeTurn => self.counters.edge_turn_uncounted += 1,
                    TurnEvent::Opened(id) => {
                        let first = (binding.channel_id, id);
                        self.turn_key_sources
                            .entry(first)
                            .or_insert_with(|| source.clone());
                    }
                    TurnEvent::Closed(span) => {
                        // Each check only excludes, so a wrong input fails the window instead of passing it.
                        let keys = [span.native_turn_id.clone(), row.clone()]
                            .into_iter()
                            .flatten();
                        let inherited = keys.fold(false, |inherited, id| {
                            let entry = self.turn_key_sources.entry((binding.channel_id, id));
                            inherited | (entry.or_insert_with(|| source.clone()) != source)
                        });
                        let closer_at = native_time(&value);
                        let excluded_reason = match attached {
                            None => Some("unattached"),
                            Some((extent, _)) if record.start < extent => Some("historical"),
                            Some((_, floor))
                                if !closer_at.is_some_and(|at| at >= floor - CLOSER_START_SKEW) =>
                            {
                                Some("closer_before_start")
                            }
                            Some(_) if inherited => Some("inherited"),
                            Some(_) => None,
                        };
                        out.push(DeriveOutput::TurnClosed(shadow_turn(
                            binding,
                            source,
                            span,
                            now,
                            excluded_reason,
                        )));
                    }
                },
            }
        }
        out
    }
}

fn shadow_turn(
    binding: &SourceBinding,
    source: &SourceId,
    span: TurnSpan,
    closed_at: DateTime<Utc>,
    excluded_reason: Option<&str>,
) -> ShadowTurn {
    ShadowTurn {
        channel_id: binding.channel_id,
        provider: binding.provider,
        native_turn_id: span.native_turn_id.unwrap_or_default(),
        source_range: SourceRange {
            source: source.clone(),
            start: span.start,
            end: span.end,
        },
        opened_at: span.opened_at,
        closed_at,
        unit_keys: span.unit_keys,
        autonomous: span.autonomous,
        synthetic_tokens: span.synthetic_tokens,
        live: excluded_reason.is_none(),
        excluded_reason: excluded_reason.map(str::to_owned),
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

    fn at(time: &str) -> DateTime<Utc> {
        format!("2026-09-27T{time}Z").parse().expect("time")
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
            DeriveOutput::TurnClosed(turn) => {
                let keys: Vec<&str> = turn
                    .unit_keys
                    .iter()
                    .map(|key| key.native_key.as_str())
                    .collect();
                let liveness = turn.excluded_reason.as_deref().unwrap_or("live");
                let (id, auto, tokens) = (
                    &turn.native_turn_id,
                    turn.autonomous,
                    &turn.synthetic_tokens,
                );
                format!("turn {id} {keys:?} auto={auto} tokens={tokens:?} {liveness}")
            }
        }
    }

    fn units(outputs: Vec<String>) -> Vec<String> {
        outputs
            .into_iter()
            .filter(|out| !out.starts_with("turn"))
            .collect()
    }

    fn turns(outputs: Vec<String>) -> Vec<String> {
        outputs
            .into_iter()
            .filter(|out| out.starts_with("turn"))
            .collect()
    }

    fn attached(name: &str, extent: u64) -> (TranscriptDerive, SourceId) {
        let mut derive = TranscriptDerive::with_clock(|| at("12:20:00"));
        let source = source(name);
        derive.attach(&source, extent, at("12:05:30"));
        (derive, source)
    }

    /// Claude units key by (message.id, apiBlockIndex), each tool_result by tool_use_id with
    /// only errors posted, and synthetic error rows by uuid; unkeyed shapes block.
    #[test]
    fn claude_profile_units_follow_identity_rules() {
        let (mut derive, source) = attached("claude", 0);
        let outputs = run(&mut derive, Claude, &source, &records(CLAUDE));
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
        assert_eq!(units(outputs), expected);
        assert!(derive.unsealed().is_empty());
    }

    /// Codex units key by payload.id and tool outputs by call_id (excluded); an announced
    /// AgentMessage stays unsealed until its response_item, and replays add no Body.
    #[test]
    fn codex_profile_units_and_unsealed_announcements() {
        let (mut derive, source) = attached("codex", 0);
        let records = records(CODEX);
        assert!(units(run(&mut derive, Codex, &source, &records[..5])).is_empty());
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
        assert_eq!(
            units(run(&mut derive, Codex, &source, &records[5..])),
            expected
        );
        assert_eq!(unsealed(&derive), ["msg_p2"]);
    }

    /// A forked source copying sealed rows reseals nothing, while a changed payload under a
    /// sealed key is surfaced instead of silently replacing the unit.
    #[test]
    fn sealed_units_repeat_silently_and_block_when_changed() {
        let records = records(CLAUDE);
        let mut derive = TranscriptDerive::default();
        run(&mut derive, Claude, &source("parent"), &records[..10]);
        assert!(units(run(&mut derive, Claude, &source("child"), &records[..10])).is_empty());
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

    /// Strict E closes a turn and never opens one; a native completion closes Codex turns;
    /// tokens come only from native user input; repeated E is only counted.
    #[test]
    fn turns_open_on_native_input_and_close_on_idle_observation() {
        let (mut derive, source) = attached("claude", 0);
        let expected = [
            r#"turn u-1 ["msg_1:1", "msg_1:2", "msg_1:3", "msg_1:4", "toolu_a", "toolu_b", "toolu_c"] auto=false tokens=["[o-shadow-synth:e1]"] live"#,
            r#"turn u-2 ["a-err"] auto=false tokens=[] live"#,
            r#"turn a-auto ["msg_3:0", "toolu_d"] auto=true tokens=[] live"#,
        ];
        assert_eq!(
            turns(run(&mut derive, Claude, &source, &records(CLAUDE))),
            expected
        );
        let counters = TurnCounters {
            stray_e: 2,
            edge_turn_uncounted: 0,
        };
        assert_eq!(derive.turn_counters(), counters);

        let (mut derive, source) = attached("codex", 0);
        let expected = [
            r#"turn t-1 ["msg_c1", "ctc_1", "call_1", "fc_1", "call_2", "msg_f1"] auto=false tokens=["[o-shadow-synth:e2]"] live"#,
        ];
        assert_eq!(
            turns(run(&mut derive, Codex, &source, &records(CODEX))),
            expected
        );
    }

    /// Records below the start boundary (source size at window start, or attach extent for a
    /// source joining later) are history; turns closed there or before t0 are not live.
    #[test]
    fn history_below_start_boundary_or_before_window_is_not_live() {
        let records = records(CLAUDE);
        let (mut derive, source) = attached("claude", 0);
        derive.window_start(at("12:05:40"), &source, records[10].start);
        let outputs = run(&mut derive, Claude, &source, &records);
        assert_eq!(outputs[0], "excluded Body msg_1:1 historical");
        assert!(outputs[7].starts_with("turn u-1 [] ") && outputs[7].ends_with("historical"));
        assert_eq!(
            outputs[9],
            r#"turn u-2 ["a-err"] auto=false tokens=[] live"#
        );

        let (mut joined, source) = attached("joined", records[10].start);
        assert_eq!(run(&mut joined, Claude, &source, &records)[7], outputs[7]);

        let (mut late, source) = attached("claude", 0);
        late.window_start(at("12:30:00"), &source, 0);
        let outputs = turns(run(&mut late, Claude, &source, &records));
        assert!(
            outputs
                .iter()
                .all(|out| out.ends_with("closer_before_start"))
        );
    }

    /// A turn whose opener a parent source already showed (a fork taken mid-turn), a turn whose
    /// opener precedes capture, and turns of unattached sources never count.
    #[test]
    fn inherited_edge_and_unattached_turns_are_not_counted() {
        let records = records(CLAUDE);
        let (mut derive, parent) = attached("parent", 0);
        run(&mut derive, Claude, &parent, &records[..8]);
        let child = source("child");
        derive.attach(&child, 0, at("12:05:30"));
        let outputs = turns(run(&mut derive, Claude, &child, &records[..10]));
        assert!(outputs.len() == 1 && outputs[0].ends_with("inherited"));

        let mut derive = TranscriptDerive::with_clock(|| at("12:20:00"));
        let outputs = turns(run(&mut derive, Claude, &source("mid"), &records[3..14]));
        let counters = TurnCounters {
            stray_e: 1,
            edge_turn_uncounted: 1,
        };
        assert_eq!(derive.turn_counters(), counters);
        assert!(
            outputs.len() == 1
                && outputs[0].starts_with("turn u-2")
                && outputs[0].ends_with("unattached")
        );
    }
}
