//! Correlates sealed O units with Legacy bot messages; measurement only, never an effect input.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

use super::{
    DeriveOutput, DiffCause, DiffClass, DiffRecord, LegacyEdit, LegacyMsg, LegacyTapEvent,
    MATCH_WINDOW, PieceDigest, ShadowDiff, ShadowUnit, UnitKey, UnitKind,
};

/// Excluded reason for units below the attach extent; they are not diffed or counted.
pub(super) const HISTORICAL_REASON: &str = "historical";

struct LegacyState {
    msg: LegacyMsg,
    content: String,
    last_at: DateTime<Utc>,
    exact_for: Option<UnitKey>,
    duplicate_of: Option<UnitKey>,
    contained: bool,
    /// Containment matches: normalized range and the piece it serves; one occurrence per piece.
    claimed: Vec<(Range<usize>, PieceDigest)>,
    /// An edit removed a consumed occurrence, so no text of the message can be shown to be unused.
    claim_lost: bool,
}

/// A unit is decided one window after sealing; a Legacy message two windows after its
/// last activity, by which time every unit that could claim it has been decided.
pub(super) struct WindowDiff {
    window: Duration,
    pending: Vec<ShadowUnit>,
    ready: Vec<DiffRecord>,
    legacy: BTreeMap<u64, LegacyState>,
    last_matched: HashMap<u64, u64>,
    retired: Vec<LegacyMsg>,
}

impl Default for WindowDiff {
    fn default() -> Self {
        Self::new(Duration::seconds(MATCH_WINDOW.as_secs() as i64))
    }
}

impl WindowDiff {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: Vec::new(),
            ready: Vec::new(),
            legacy: BTreeMap::new(),
            last_matched: HashMap::new(),
            retired: Vec::new(),
        }
    }

    /// Legacy messages that left the window, with their edit history, for persistence.
    pub fn drain_retired(&mut self) -> Vec<LegacyMsg> {
        std::mem::take(&mut self.retired)
    }

    /// Messages live in `[since, until]`; a unit derived late still sees none of its later history.
    fn candidates(
        &self,
        channel: u64,
        (since, until): (DateTime<Utc>, DateTime<Utc>),
    ) -> impl Iterator<Item = (&u64, &LegacyState)> {
        self.legacy.iter().filter(move |(_, l)| {
            let settled = l.msg.created_at <= until && l.msg.edits.iter().all(|e| e.at <= until);
            l.msg.channel_id == channel && !l.msg.deleted && l.last_at >= since && settled
        })
    }

    fn claim_exact(&mut self, id: u64, key: &UnitKey, span: (DateTime<Utc>, DateTime<Utc>)) {
        let channel = key.channel_id;
        let Some(sha) = self.legacy.get(&id).map(|l| l.msg.content_sha256.clone()) else {
            return;
        };
        let twins: Vec<u64> = self
            .candidates(channel, span)
            .filter(|(other, l)| {
                **other != id && l.exact_for.is_none() && l.msg.content_sha256 == sha
            })
            .map(|(other, _)| *other)
            .collect();
        for twin in twins {
            if let Some(l) = self.legacy.get_mut(&twin) {
                l.duplicate_of.get_or_insert_with(|| key.clone());
            }
        }
        if let Some(l) = self.legacy.get_mut(&id) {
            l.exact_for = Some(key.clone());
            l.duplicate_of = None;
        }
    }

    /// Decides, in sealing order, the pending units whose window end satisfies `due`.
    fn decide_due(&mut self, due: impl Fn(DateTime<Utc>) -> bool) -> Vec<DiffRecord> {
        let window = self.window;
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|u| due(u.sealed_at + window));
        self.pending = waiting;
        ready.into_iter().map(|unit| self.decide(unit)).collect()
    }

    /// Exact payload match first; only then normalized containment in a Legacy message.
    fn decide(&mut self, unit: ShadowUnit) -> DiffRecord {
        let key = unit.unit_key;
        let channel = key.channel_id;
        let span = (unit.sealed_at - self.window, unit.sealed_at + self.window);
        let (mut ids, mut exact_all, mut missing) = (Vec::new(), true, false);
        for piece in &unit.pieces {
            let exact = self
                .candidates(channel, span)
                .find(|(_, l)| {
                    l.exact_for.is_none() && !l.contained && l.msg.content_sha256 == piece.sha256
                })
                .map(|(id, _)| *id);
            if let Some(id) = exact {
                self.claim_exact(id, &key, span);
                ids.push(id);
                continue;
            }
            exact_all = false;
            let contained = (self.candidates(channel, span))
                .filter(|(_, l)| l.exact_for.is_none() && !l.claim_lost)
                .find_map(|(id, l)| Some((*id, contains_piece(&l.content, piece, &l.claimed)?)));
            match contained.and_then(|(id, range)| self.legacy.get_mut(&id).map(|l| (id, l, range)))
            {
                Some((id, l, range)) => {
                    l.contained = true;
                    l.claimed.push((range, piece.clone()));
                    ids.push(id);
                }
                None => missing = true,
            }
        }
        let floor = self.last_matched.get(&channel).copied().unwrap_or(0);
        let ordered =
            ids.windows(2).all(|w| w[0] <= w[1]) && ids.first().is_none_or(|id| *id >= floor);
        if let Some(max) = ids.iter().max() {
            self.last_matched.insert(channel, floor.max(*max));
        }
        let class = match (missing, ordered, exact_all) {
            (true, _, _) => DiffClass::LegacyMissing,
            (false, false, _) => DiffClass::OrderDiff,
            (false, true, true) => DiffClass::Match,
            (false, true, false) => DiffClass::FormatOnly,
        };
        // Legacy never posts tool units, so only their absence is expected; other diffs stay open.
        let o_only = class == DiffClass::LegacyMissing && key.kind != UnitKind::Body;
        let mut row = record(channel, Some(key), class, ids);
        if o_only {
            row.cause = DiffCause::OOnlyTool;
        }
        row
    }
}

impl ShadowDiff for WindowDiff {
    fn observe_derived(&mut self, output: &DeriveOutput, _at: DateTime<Utc>) {
        let row = match output {
            DeriveOutput::Sealed(unit) => return self.pending.push(unit.clone()),
            DeriveOutput::Excluded { reason, .. } if reason == HISTORICAL_REASON => return,
            DeriveOutput::Excluded { unit_key, .. } => record(
                unit_key.channel_id,
                Some(unit_key.clone()),
                DiffClass::OExcluded,
                Vec::new(),
            ),
            DeriveOutput::SchemaBlocked { channel_id, .. } => {
                record(*channel_id, None, DiffClass::OSchemaBlocked, Vec::new())
            }
            DeriveOutput::TurnClosed(_) => return,
        };
        self.ready.push(row);
    }

    fn observe_legacy(&mut self, event: &LegacyTapEvent) {
        let (LegacyTapEvent::Created { at, .. }
        | LegacyTapEvent::Updated { at, .. }
        | LegacyTapEvent::Deleted { at, .. }) = event;
        // Units whose window closed before this event are judged on the state before it.
        let decided = self.decide_due(|deadline| deadline < *at);
        self.ready.extend(decided);
        match event {
            LegacyTapEvent::Created {
                channel_id,
                msg_id,
                at,
                content,
            } => {
                let msg = LegacyMsg {
                    msg_id: *msg_id,
                    channel_id: *channel_id,
                    created_at: *at,
                    edits: Vec::new(),
                    deleted: false,
                    content_sha256: sha256_hex(content),
                };
                self.legacy.entry(*msg_id).or_insert_with(|| LegacyState {
                    msg,
                    content: content.clone(),
                    last_at: *at,
                    exact_for: None,
                    duplicate_of: None,
                    contained: false,
                    claimed: Vec::new(),
                    claim_lost: false,
                });
            }
            LegacyTapEvent::Updated {
                msg_id,
                at,
                content,
                ..
            } => {
                let Some(l) = self.legacy.get_mut(msg_id) else {
                    return;
                };
                l.last_at = l.last_at.max(*at);
                if let Some(content) = content.as_ref().filter(|c| **c != l.content) {
                    l.msg.content_sha256 = sha256_hex(content);
                    let content_sha256 = l.msg.content_sha256.clone();
                    l.msg.edits.push(LegacyEdit {
                        at: *at,
                        content_sha256,
                    });
                    l.content = content.clone();
                    // An edit moves text; each consumed occurrence is found again and stays taken.
                    let mut moved = std::mem::take(&mut l.claimed);
                    moved.sort_by_key(|(range, _)| range.start);
                    for (_, piece) in moved {
                        match contains_piece(&l.content, &piece, &l.claimed) {
                            Some(range) => l.claimed.push((range, piece)),
                            None => l.claim_lost = true,
                        }
                    }
                }
            }
            LegacyTapEvent::Deleted { msg_id, at, .. } => {
                if let Some(l) = self.legacy.get_mut(msg_id) {
                    l.msg.deleted = true;
                    l.last_at = l.last_at.max(*at);
                }
            }
        }
    }

    /// The tap cannot tell which channel lost events, so the gap row carries channel 0.
    fn observe_tap_gap(&mut self, dropped: u64, _at: DateTime<Utc>) {
        if dropped > 0 {
            self.ready
                .push(record(0, None, DiffClass::TapGap, Vec::new()));
        }
    }

    fn drain_ready(&mut self, now: DateTime<Utc>) -> Vec<DiffRecord> {
        let mut out = std::mem::take(&mut self.ready);
        let window = self.window;
        out.extend(self.decide_due(|deadline| deadline <= now));
        let expired: Vec<u64> = self
            .legacy
            .iter()
            .filter(|(_, l)| l.last_at + window * 2 <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let Some(l) = self.legacy.remove(&id) else {
                continue;
            };
            if !l.msg.deleted && l.exact_for.is_none() && !l.contained {
                let class = match l.duplicate_of {
                    Some(_) => DiffClass::LegacyDuplicate,
                    None => DiffClass::LegacyExtra,
                };
                out.push(record(l.msg.channel_id, l.duplicate_of, class, vec![id]));
            }
            self.retired.push(l.msg);
        }
        out
    }
}

/// Initial cause before operator classification; only duplicates are attributable up front.
pub(super) fn default_cause(class: DiffClass) -> DiffCause {
    match class {
        DiffClass::Match | DiffClass::FormatOnly | DiffClass::OExcluded => DiffCause::Expected,
        DiffClass::LegacyDuplicate => DiffCause::LegacyDefect,
        _ => DiffCause::Unknown,
    }
}

fn record(
    channel_id: u64,
    unit_key: Option<UnitKey>,
    class: DiffClass,
    legacy_msg_ids: Vec<u64>,
) -> DiffRecord {
    DiffRecord {
        channel_id,
        unit_key,
        class,
        legacy_msg_ids,
        cause: default_cause(class),
    }
}

pub(super) fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Legacy-side normalization: CRLF and the zero-width spaces Legacy inserts.
fn normalize_legacy(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\u{200b}', "")
}

/// First unclaimed substring of the normalized Legacy text with the piece's UTF-16 length and sha256.
fn contains_piece(
    content: &str,
    piece: &PieceDigest,
    claimed: &[(Range<usize>, PieceDigest)],
) -> Option<Range<usize>> {
    let text = normalize_legacy(content);
    let target = piece.units as usize;
    let mut bounds = vec![(0usize, 0usize)];
    let mut units = 0;
    for (at, ch) in text.char_indices() {
        units += ch.len_utf16();
        bounds.push((at + ch.len_utf8(), units));
    }
    let mut end = 0;
    for start in 0..bounds.len() {
        let want = bounds[start].1 + target;
        while end < bounds.len() && bounds[end].1 < want {
            end += 1;
        }
        if end == bounds.len() {
            return None;
        }
        let range = bounds[start].0..bounds[end].0;
        let free = claimed
            .iter()
            .all(|(c, _)| c.end <= range.start || range.end <= c.start);
        if bounds[end].1 == want && free && sha256_hex(&text[range.clone()]) == piece.sha256 {
            return Some(range);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_o::shadow::{ShadowProvider, SourceId, SourceRange};
    use DiffClass::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn key(id: &str) -> UnitKey {
        let native_key = id.to_string();
        UnitKey {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            native_key,
            kind: UnitKind::Body,
        }
    }

    fn seal(diff: &mut WindowDiff, id: &str, at: i64, pieces: &[&str]) {
        seal_as(diff, UnitKind::Body, id, at, pieces);
    }

    fn seal_as(diff: &mut WindowDiff, kind: UnitKind, id: &str, at: i64, pieces: &[&str]) {
        let digest = |(i, p): (usize, &&str)| PieceDigest {
            index: i as u32,
            units: p.encode_utf16().count() as u32,
            sha256: sha256_hex(p),
        };
        let source = SourceId {
            session_id: "s".into(),
            path: "/t".into(),
            dev: 1,
            ino: 1,
        };
        let unit = ShadowUnit {
            unit_key: UnitKey { kind, ..key(id) },
            kind,
            source_range: SourceRange {
                source,
                start: 0,
                end: 1,
            },
            sealed_at: t(at),
            pieces: pieces.iter().enumerate().map(digest).collect(),
        };
        diff.observe_derived(&DeriveOutput::Sealed(unit), t(at));
    }

    fn post(diff: &mut WindowDiff, msg_id: u64, at: i64, content: &str) {
        let (channel_id, at, content) = (7, t(at), content.to_string());
        diff.observe_legacy(&LegacyTapEvent::Created {
            channel_id,
            msg_id,
            at,
            content,
        });
    }

    fn rows(diff: &mut WindowDiff, at: i64) -> Vec<(DiffClass, Vec<u64>, DiffCause)> {
        let row = |r: DiffRecord| (r.class, r.legacy_msg_ids, r.cause);
        diff.drain_ready(t(at)).into_iter().map(row).collect()
    }

    #[test]
    fn exact_match_is_tried_before_an_earlier_containing_message() {
        let mut diff = WindowDiff::default();
        post(&mut diff, 1, 0, "⏳ working\nfinal answer");
        post(&mut diff, 2, 1, "final answer");
        seal(&mut diff, "a", 2, &["final answer"]);
        assert!(
            rows(&mut diff, 301).is_empty(),
            "undecided inside the window"
        );
        assert_eq!(
            rows(&mut diff, 302),
            vec![(Match, vec![2], DiffCause::Expected)]
        );
    }

    #[test]
    fn normalized_containment_is_format_only_and_an_unposted_piece_is_missing() {
        let mut diff = WindowDiff::default();
        seal(&mut diff, "a", 0, &["first\nsecond"]);
        seal(&mut diff, "b", 0, &["never posted"]);
        post(&mut diff, 1, 200, "fir\u{200b}st\r\nsecond\n\n✅ done");
        let decided = rows(&mut diff, 300);
        assert_eq!(decided[0], (FormatOnly, vec![1], DiffCause::Expected));
        assert_eq!(decided[1], (LegacyMissing, vec![], DiffCause::Unknown));
    }

    #[test]
    fn legacy_rows_are_judged_two_windows_after_their_last_activity() {
        let mut diff = WindowDiff::default();
        seal(&mut diff, "a", 0, &["same"]);
        seal(&mut diff, "b", 5, &["twice"]);
        seal(&mut diff, "c", 5, &["twice"]);
        post(&mut diff, 1, 10, "same");
        post(&mut diff, 2, 20, "same");
        post(&mut diff, 3, 30, "unrelated");
        post(&mut diff, 4, 6, "twice");
        post(&mut diff, 5, 7, "twice");
        let decided = rows(&mut diff, 305);
        assert_eq!(
            decided.iter().filter(|r| r.0 == Match).count(),
            3,
            "{decided:?}"
        );
        assert!(rows(&mut diff, 619).is_empty());
        let late = rows(&mut diff, 630);
        assert_eq!(late[0], (LegacyDuplicate, vec![2], DiffCause::LegacyDefect));
        assert_eq!(late[1], (LegacyExtra, vec![3], DiffCause::Unknown));
        assert_eq!(diff.drain_retired().len(), 5);
    }

    #[test]
    fn an_unposted_tool_unit_is_o_only_while_other_misses_stay_unknown() {
        let mut diff = WindowDiff::default();
        seal_as(&mut diff, UnitKind::Tool, "t", 0, &["Bash: ls"]);
        seal_as(&mut diff, UnitKind::ToolResult, "r", 0, &["boom"]);
        seal(&mut diff, "b", 0, &["never posted"]);
        seal(&mut diff, "one", 0, &["one"]);
        seal_as(&mut diff, UnitKind::Tool, "two", 0, &["two"]);
        post(&mut diff, 1, 0, "two");
        post(&mut diff, 2, 0, "one");
        let o_only = (LegacyMissing, vec![], DiffCause::OOnlyTool);
        let expected = vec![
            o_only.clone(),
            o_only,
            (LegacyMissing, vec![], DiffCause::Unknown),
            (Match, vec![2], DiffCause::Expected),
            (OrderDiff, vec![1], DiffCause::Unknown),
        ];
        assert_eq!(rows(&mut diff, 300), expected);
    }

    #[test]
    fn one_legacy_occurrence_is_consumed_by_one_unit_only() {
        let mut diff = WindowDiff::default();
        seal(&mut diff, "a", 0, &["same answer"]);
        seal(&mut diff, "b", 1, &["same answer"]);
        post(&mut diff, 1, 0, "same answer");
        let missing = (LegacyMissing, vec![], DiffCause::Unknown);
        let exact = vec![(Match, vec![1], DiffCause::Expected), missing.clone()];
        assert_eq!(rows(&mut diff, 301), exact);
        let mut diff = WindowDiff::default();
        seal(&mut diff, "c", 0, &["one"]);
        seal(&mut diff, "d", 1, &["one"]);
        seal(&mut diff, "e", 1, &["two"]);
        post(&mut diff, 2, 0, "one\ntwo");
        let format_only = |ids| (FormatOnly, ids, DiffCause::Expected);
        let contained = vec![format_only(vec![2]), missing, format_only(vec![2])];
        assert_eq!(rows(&mut diff, 301), contained);
    }

    #[test]
    fn an_edit_that_moves_a_consumed_occurrence_does_not_free_it() {
        let mut diff = WindowDiff::default();
        seal(&mut diff, "a", 0, &["one"]);
        post(&mut diff, 1, 0, "one\nfooter");
        assert_eq!(rows(&mut diff, 300)[0].0, FormatOnly);
        let edit = |diff: &mut WindowDiff, text: &str| {
            let (channel_id, msg_id, at) = (7, 1, t(301));
            let content = Some(text.to_string());
            diff.observe_legacy(&LegacyTapEvent::Updated {
                channel_id,
                msg_id,
                at,
                content,
            });
        };
        edit(&mut diff, "prefix\none\nfooter");
        seal(&mut diff, "b", 301, &["one"]);
        let missing = (LegacyMissing, vec![], DiffCause::Unknown);
        assert_eq!(rows(&mut diff, 601), vec![missing.clone()]);
        // The consumed text is gone after the edit, so none of the message is handed out again.
        let mut diff = WindowDiff::default();
        seal(&mut diff, "c", 0, &["one two"]);
        post(&mut diff, 1, 0, "one two\nfooter");
        rows(&mut diff, 300);
        edit(&mut diff, "one\ntwo\nfooter");
        seal(&mut diff, "d", 301, &["one"]);
        seal(&mut diff, "e", 301, &["one\ntwo\nfooter"]);
        assert_eq!(rows(&mut diff, 601), [missing.clone(), missing]);
    }

    #[test]
    fn a_post_after_the_unit_window_misses_even_when_the_observer_drains_late() {
        let mut diff = WindowDiff::default();
        seal(&mut diff, "late", 0, &["answer"]);
        seal(&mut diff, "edge", 0, &["edge"]);
        post(&mut diff, 1, 300, "edge");
        post(&mut diff, 2, 301, "answer");
        let decided = rows(&mut diff, 301);
        assert_eq!(decided[0], (LegacyMissing, vec![], DiffCause::Unknown));
        assert_eq!(decided[1], (Match, vec![1], DiffCause::Expected));
    }

    #[test]
    fn a_unit_derived_after_its_window_is_judged_on_the_legacy_state_of_that_window() {
        let mut diff = WindowDiff::default();
        post(&mut diff, 1, 10, "kept");
        post(&mut diff, 2, 20, "draft");
        post(&mut diff, 3, 350, "late");
        let (channel_id, msg_id, at, content) = (7, 2, t(350), Some("changed".to_string()));
        let edit = LegacyTapEvent::Updated {
            channel_id,
            msg_id,
            at,
            content,
        };
        diff.observe_legacy(&edit);
        // Read at 0 but derived only at 400, after a held capture reached the matcher.
        for (id, text) in [("k", "kept"), ("l", "late"), ("c", "changed")] {
            seal(&mut diff, id, 0, &[text]);
        }
        let missing = (LegacyMissing, vec![], DiffCause::Unknown);
        let matched = (Match, vec![1], DiffCause::Expected);
        assert_eq!(rows(&mut diff, 400), [matched, missing.clone(), missing]);
    }

    #[test]
    fn legacy_posting_a_later_unit_first_is_an_order_diff() {
        let mut diff = WindowDiff::default();
        seal(&mut diff, "a", 0, &["one"]);
        seal(&mut diff, "b", 0, &["two"]);
        post(&mut diff, 1, 0, "two");
        post(&mut diff, 2, 0, "one");
        let classes: Vec<_> = rows(&mut diff, 300).into_iter().map(|r| r.0).collect();
        assert_eq!(classes, vec![Match, OrderDiff]);
    }

    #[test]
    fn historical_units_are_not_diffed_while_other_o_outcomes_are_immediate() {
        let mut diff = WindowDiff::default();
        let excluded = |reason: &str| DeriveOutput::Excluded {
            unit_key: key("x"),
            reason: reason.into(),
        };
        diff.observe_derived(&excluded(HISTORICAL_REASON), t(0));
        diff.observe_derived(&excluded("normal_tool_result"), t(0));
        diff.observe_tap_gap(3, t(0));
        let decided = rows(&mut diff, 0);
        assert_eq!(
            decided,
            vec![
                (OExcluded, vec![], DiffCause::Expected),
                (TapGap, vec![], DiffCause::Unknown)
            ]
        );
    }
}
