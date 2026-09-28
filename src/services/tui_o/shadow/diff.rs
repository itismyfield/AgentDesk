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
