//! `ShadowDerive` over captured transcript records: identity, plan and seal for each record,
//! plus the native turns its Idle observations close.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use super::identity::{RecordFact, classify, native_time, row_key};
use super::seal::{SealOutcome, SealRegistry, TurnEvent, TurnSpan, TurnTracker};
use super::unit_plan::{UnitPlan, plan};
use super::{
    CaptureBatch, CapturedRecord, DeriveOutput, ShadowDerive, ShadowProvider, ShadowTurn,
    ShadowUnit, SourceBinding, SourceId, SourceRange, UnitKey,
};

/// A closer written this long before attach or window start is treated as replayed history.
const CLOSER_START_SKEW: TimeDelta = TimeDelta::seconds(60);

/// Turn identity across sources: channel, provider and an opener or closer native key.
type TurnKey = (u64, ShadowProvider, String);

/// Idle observations that closed no countable turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnCounters {
    pub stray_e: u64,
    pub edge_turn_uncounted: u64,
}

pub struct TranscriptDerive {
    seals: SealRegistry,
    turns: HashMap<(u64, SourceId), TurnTracker>,
    /// Per source: file size and time at first attach.
    attached: HashMap<SourceId, (u64, DateTime<Utc>)>,
    /// Window start `t0` and the size at t0 of each source it listed.
    window: Option<(DateTime<Utc>, HashMap<SourceId, u64>)>,
    /// Source that first opened each opener key; a turn reopened elsewhere is inherited.
    opener_sources: HashMap<TurnKey, SourceId>,
    /// Opener and closer keys of every closed turn; a later turn showing one is inherited.
    closed_turn_keys: HashSet<TurnKey>,
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
            window: None,
            opener_sources: HashMap::new(),
            closed_turn_keys: HashSet::new(),
            counters: TurnCounters::default(),
            clock,
        }
    }

    /// Records the file size seen when `source` was first attached; later calls are ignored.
    pub fn attach(&mut self, source: &SourceId, attach_extent: u64, attached_at: DateTime<Utc>) {
        let first = (attach_extent, attached_at);
        self.attached.entry(source.clone()).or_insert(first);
    }

    /// Opens the window at `t0` even when it lists no source; the first `t0` holds for the window.
    pub fn window_open(&mut self, t0: DateTime<Utc>) {
        self.window.get_or_insert_with(|| (t0, HashMap::new()));
    }

    /// Records one source of the window start at `t0`; the first `t0` holds for the window.
    pub fn window_start(&mut self, t0: DateTime<Utc>, source: &SourceId, window_start_extent: u64) {
        let (_, extents) = self.window.get_or_insert_with(|| (t0, HashMap::new()));
        extents.insert(source.clone(), window_start_extent);
    }

    /// Where live output of `source` starts and the earliest closer time, or why none can count.
    fn start_boundary(&self, source: &SourceId) -> Result<(u64, DateTime<Utc>), &'static str> {
        let &(attach_extent, attached_at) = self.attached.get(source).ok_or("unattached")?;
        let (t0, extents) = self.window.as_ref().ok_or("no_window")?;
        if attached_at >= *t0 {
            return Ok((attach_extent, attached_at));
        }
        // A source attached before t0 needs its size at t0; its attach size would admit backlog.
        let extent = extents.get(source).ok_or("window_missing_source")?;
        Ok((*extent, *t0))
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
        let boundary = self.start_boundary(source);
        // Backlog below the attach size stays history even before the window evidence is complete.
        let history_extent = match boundary {
            Ok((extent, _)) => Some(extent),
            Err(_) => self.attached.get(source).map(|&(extent, _)| extent),
        };
        let historical = history_extent.is_some_and(|extent| record.start < extent);
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
                        let opener = (binding.channel_id, binding.provider, id);
                        let first = self.opener_sources.entry(opener);
                        first.or_insert_with(|| source.clone());
                    }
                    TurnEvent::Closed(span) => {
                        // Each check only excludes, so a wrong input fails the window instead of passing it.
                        let turn_key =
                            |id: &String| (binding.channel_id, binding.provider, id.clone());
                        let reopened = span.native_turn_id.as_ref().is_some_and(|id| {
                            let first = self.opener_sources.get(&turn_key(id));
                            first.is_some_and(|first| first != source)
                        });
                        let keys: Vec<TurnKey> = [&span.native_turn_id, &row]
                            .into_iter()
                            .flatten()
                            .map(turn_key)
                            .collect();
                        let replayed = keys.iter().any(|key| self.closed_turn_keys.contains(key));
                        self.closed_turn_keys.extend(keys);
                        let closer_at = native_time(&value);
                        let excluded_reason = match boundary {
                            Err(reason) => Some(reason),
                            Ok((extent, _)) if record.start < extent => Some("historical"),
                            Ok((_, floor))
                                if !closer_at.is_some_and(|at| at >= floor - CLOSER_START_SKEW) =>
                            {
                                Some("closer_before_start")
                            }
                            Ok(_) if reopened || replayed => Some("inherited"),
                            Ok(_) => None,
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

    fn clock() -> DateTime<Utc> {
        at("12:20:00")
    }

    /// A source attached before a window that started while the source was still empty.
    fn live_source(name: &str) -> (TranscriptDerive, SourceId) {
        let (mut derive, source) = (TranscriptDerive::with_clock(clock), source(name));
        derive.attach(&source, 0, at("12:05:30"));
        derive.window_start(at("12:05:40"), &source, 0);
        (derive, source)
    }

    /// Claude units key by (message.id, apiBlockIndex), each tool_result by tool_use_id with
    /// only errors posted, and synthetic error rows by uuid; unkeyed shapes block.
    #[test]
    fn claude_profile_units_follow_identity_rules() {
        let (mut derive, source) = live_source("claude");
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
        let (mut derive, source) = live_source("codex");
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
    /// tokens come only from native user text, even beside non-text items; repeated E is counted.
    #[test]
    fn turns_open_on_native_input_and_close_on_idle_observation() {
        let (mut derive, source) = live_source("claude");
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

        let (mut derive, source) = live_source("codex");
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
        let (mut early, claude) = (TranscriptDerive::with_clock(clock), source("claude"));
        early.attach(&claude, 0, at("12:05:30"));
        early.window_start(at("12:05:40"), &claude, records[10].start);
        let outputs = run(&mut early, Claude, &claude, &records);
        assert_eq!(outputs[0], "excluded Body msg_1:1 historical");
        assert!(outputs[7].starts_with("turn u-1 [] ") && outputs[7].ends_with("historical"));
        assert_eq!(
            outputs[9],
            r#"turn u-2 ["a-err"] auto=false tokens=[] live"#
        );

        let (mut joined, other) = (TranscriptDerive::with_clock(clock), source("other"));
        joined.window_start(at("12:05:40"), &other, 0);
        joined.attach(&claude, records[10].start, at("12:05:50"));
        assert_eq!(run(&mut joined, Claude, &claude, &records)[7], outputs[7]);

        let mut late = TranscriptDerive::with_clock(clock);
        late.attach(&claude, 0, at("12:05:30"));
        late.window_start(at("12:30:00"), &claude, 0);
        let outputs = turns(run(&mut late, Claude, &claude, &records));
        assert!(
            outputs
                .iter()
                .all(|out| out.ends_with("closer_before_start"))
        );
    }

    /// No turn is live without both an attach and a window start, and a source attached before
    /// t0 but missing from the window start must not fall back to its attach size.
    #[test]
    fn live_requires_attach_and_a_window_boundary_for_the_source() {
        let records = records(CLAUDE);
        let (a, b) = (source("a"), source("b"));
        let reasons = |derive: &mut TranscriptDerive, source: &SourceId| -> Vec<String> {
            let outputs = turns(run(derive, Claude, source, &records));
            outputs
                .iter()
                .map(|out| out.rsplit(' ').next().unwrap_or("").to_owned())
                .collect()
        };
        let mut only_attach = TranscriptDerive::with_clock(clock);
        only_attach.attach(&a, 0, at("12:05:30"));
        assert_eq!(reasons(&mut only_attach, &a), ["no_window"; 3]);

        let mut only_window = TranscriptDerive::with_clock(clock);
        only_window.window_start(at("12:05:40"), &a, 0);
        assert_eq!(reasons(&mut only_window, &a), ["unattached"; 3]);

        let mut partial = TranscriptDerive::with_clock(clock);
        partial.attach(&a, 0, at("12:05:30"));
        partial.attach(&b, 0, at("12:05:30"));
        partial.window_start(at("12:05:40"), &a, 0);
        assert_eq!(reasons(&mut partial, &b), ["window_missing_source"; 3]);
    }

    /// A window start listing no source still sets t0, so a source attached after it counts;
    /// a later open keeps the first t0.
    #[test]
    fn empty_window_start_then_new_attach_counts_live() {
        let (records, claude) = (records(CLAUDE), source("claude"));
        let mut derive = TranscriptDerive::with_clock(clock);
        derive.window_open(at("12:05:40"));
        derive.window_open(at("12:30:00"));
        derive.attach(&claude, 0, at("12:05:50"));
        let outputs = turns(run(&mut derive, Claude, &claude, &records));
        let live = outputs.iter().filter(|out| out.ends_with(" live")).count();
        assert_eq!((outputs.len(), live), (3, 3), "{outputs:?}");
    }

    /// Forked-mid-turn, same-source replayed, pre-capture-opener and unattached-source turns
    /// never count.
    #[test]
    fn inherited_replayed_edge_and_unattached_turns_are_not_counted() {
        let records = records(CLAUDE);
        let (mut derive, parent) = live_source("parent");
        run(&mut derive, Claude, &parent, &records[..8]);
        let child = source("child");
        derive.attach(&child, 0, at("12:05:50"));
        let outputs = turns(run(&mut derive, Claude, &child, &records[..10]));
        assert!(outputs.len() == 1 && outputs[0].ends_with("inherited"));

        let (mut derive, claude) = live_source("claude");
        let first = turns(run(&mut derive, Claude, &claude, &records[..10]));
        let eof = records[9].end;
        let shift = |record: &CapturedRecord| CapturedRecord {
            start: record.start + eof,
            end: record.end + eof,
            line: record.line.clone(),
        };
        let replay: Vec<CapturedRecord> = records[1..10].iter().map(shift).collect();
        let second = turns(run(&mut derive, Claude, &claude, &replay));
        assert!(first.len() == 1 && first[0].ends_with("live"));
        assert!(second.len() == 1 && second[0].ends_with("inherited"));

        let mut derive = TranscriptDerive::with_clock(clock);
        let outputs = turns(run(&mut derive, Claude, &source("mid"), &records[3..14]));
        let counters = TurnCounters {
            stray_e: 1,
            edge_turn_uncounted: 1,
        };
        assert_eq!(derive.turn_counters(), counters);
        let unattached = outputs[0].starts_with("turn u-2") && outputs[0].ends_with("unattached");
        assert!(outputs.len() == 1 && unattached);
    }
}
