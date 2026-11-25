//! Tool-image ledger entries: native turn spans and image obligations, replayed with the rest
//! of the ledger. No production path appends them yet.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::{LedgerState, PieceOutcome};
use crate::services::tui_o::shadow::{SourceId, UnitKey, UnitKind};

/// Image attempts that count; a proven-unsent attempt (`NotSent`) does not.
pub(crate) const IMAGE_ATTEMPTS: usize = 2;
/// Counted omission-notice attempts before the notice stays open under an alarm.
pub(crate) const NOTICE_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageEntry {
    /// A native turn's first image occurrence, recorded before any of its obligations.
    ImageTurnOpened {
        source: SourceId,
        opener_offset: u64,
        /// The native turn id (`NativeTurn` in the writer).
        turn: String,
    },
    /// The turn ended (native idle or verified physical end); records at `end_offset` and later
    /// belong to a later turn.
    ImageTurnClosed {
        source: SourceId,
        turn: String,
        end_offset: u64,
    },
    /// One image owed to Discord; the first record for a key stands.
    ImageObligation(Box<ImageObligation>),
    /// A call still waiting for its result when its turn ended: a late result keeps this turn.
    ImageCall {
        source: SourceId,
        call_id: String,
        turn: String,
    },
    /// The image ends without an attachment; its result's omission notice is owed instead.
    ImageOmitted {
        unit_key: UnitKey,
        /// The writer's `OmitReason` name, e.g. `not_confirmed`.
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageObligation {
    /// `kind: ToolImage`, `native_key: img:{turn}:{sha256}` or `ref:{turn}:{call}:{index}`.
    pub unit_key: UnitKey,
    pub turn: String,
    /// Claude `tool_use_id` or Codex `call_id`; with `turn` it names the omission-notice group.
    pub call_id: String,
    pub image_index: u32,
    /// The raw record holding the bytes; an open obligation keeps its segment from GC.
    pub source: SourceId,
    pub record_start: u64,
    pub record_end: u64,
    /// Decoded-content SHA-256; `None` only for an external reference.
    pub sha256: Option<String>,
    pub media_type: Option<String>,
    pub size: Option<u64>,
    /// Shown in the caption; never part of the identity.
    pub display_name: String,
    /// Settlement marker in the caption, unique per turn and content.
    pub marker: String,
}

/// One turn's span in a source: `opener_offset <= offset < end_offset`, open-ended until Closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnSpan {
    pub turn: String,
    pub opener_offset: u64,
    pub end_offset: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageState {
    /// Per source, ordered by opener offset and never overlapping.
    spans: HashMap<SourceId, Vec<TurnSpan>>,
    obligations: BTreeMap<UnitKey, ImageObligation>,
    omitted: BTreeMap<UnitKey, String>,
    /// `(source, call id)` to the call's turn; one turn per call.
    calls: HashMap<(SourceId, String), String>,
}

/// One omitted image of a notice's tool result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NoticeMember {
    pub(crate) key: UnitKey,
    pub(crate) reason: String,
    pub(crate) source: SourceId,
    pub(crate) record_start: u64,
    pub(crate) record_end: u64,
}

/// Replays one entry; an inconsistent entry is an ownership violation like any other.
pub(super) fn apply(ledger: &mut LedgerState, entry: ImageEntry) {
    let conflict = match &entry {
        ImageEntry::ImageOmitted { unit_key, .. } => omission_conflict(ledger, unit_key),
        _ => None,
    };
    if let Some(detail) = conflict.or_else(|| ledger.tool_images.apply(entry).err()) {
        ledger.violate(detail);
    }
}

/// An omission may end an image only before any attempt, after a proven-unsent attempt, after
/// the counted NotFound attempts, or after Discord refused the upload itself (400, 413).
fn omission_conflict(ledger: &LedgerState, key: &UnitKey) -> Option<String> {
    let attempts = attempts(ledger, key);
    let allowed = match attempts.last() {
        None | Some((_, Some(PieceOutcome::NotSent))) => true,
        Some((_, Some(PieceOutcome::NotFound))) => counted(&attempts) >= IMAGE_ATTEMPTS,
        Some((_, Some(PieceOutcome::Rejected(400 | 413)))) => true,
        Some(_) => false,
    };
    (!allowed).then(|| format!("image {} omitted against its attempts", key.native_key))
}

/// The omission notice's unit for one tool result: `omit:{turn}:{call}`.
pub(crate) fn notice_native_key(turn: &str, call_id: &str) -> String {
    format!("omit:{turn}:{call_id}")
}

impl ImageState {
    fn apply(&mut self, entry: ImageEntry) -> Result<(), String> {
        match entry {
            ImageEntry::ImageTurnOpened {
                source,
                opener_offset,
                turn,
            } => open_span(self.spans.entry(source).or_default(), turn, opener_offset),
            ImageEntry::ImageTurnClosed {
                source,
                turn,
                end_offset,
            } => {
                let spans = self.spans.get_mut(&source).map(Vec::as_mut_slice);
                close_span(spans.unwrap_or_default(), &turn, end_offset)
            }
            ImageEntry::ImageObligation(obligation) => {
                if obligation.unit_key.kind != UnitKind::ToolImage {
                    return Err(format!(
                        "image obligation {} is not a tool image",
                        obligation.unit_key.native_key
                    ));
                }
                // Same bytes under another file name: the first metadata stands.
                self.obligations
                    .entry(obligation.unit_key.clone())
                    .or_insert(*obligation);
                Ok(())
            }
            ImageEntry::ImageCall {
                source,
                call_id,
                turn,
            } => match self.calls.get(&(source.clone(), call_id.clone())) {
                Some(old) if *old != turn => Err(format!("call {call_id} moved to turn {turn}")),
                Some(_) => Ok(()),
                None => {
                    self.calls.insert((source, call_id), turn);
                    Ok(())
                }
            },
            ImageEntry::ImageOmitted { unit_key, reason } => {
                if !self.obligations.contains_key(&unit_key) {
                    return Err(format!("image {} omitted unowed", unit_key.native_key));
                }
                match self.omitted.get(&unit_key) {
                    Some(old) if *old != reason => Err(format!(
                        "image {} omitted again as {}",
                        unit_key.native_key, reason
                    )),
                    Some(_) => Ok(()),
                    None => {
                        self.omitted.insert(unit_key, reason);
                        Ok(())
                    }
                }
            }
        }
    }

    /// The span holding `offset` in `source`, closed or still open.
    pub(crate) fn span_at(&self, source: &SourceId, offset: u64) -> Option<&TurnSpan> {
        self.spans.get(source)?.iter().find(|span| {
            span.opener_offset <= offset && span.end_offset.is_none_or(|end| offset < end)
        })
    }

    pub(crate) fn span_of(&self, source: &SourceId, turn: &str) -> Option<&TurnSpan> {
        self.spans
            .get(source)?
            .iter()
            .find(|span| span.turn == turn)
    }

    /// The durable turn of a call whose result came after its turn ended.
    pub(crate) fn call_turn(&self, source: &SourceId, call_id: &str) -> Option<&str> {
        let key = (source.clone(), call_id.to_owned());
        self.calls.get(&key).map(String::as_str)
    }

    /// The omitted images a notice key reports, with their reasons and source records.
    pub(crate) fn notice_members(&self, notice: &UnitKey) -> Vec<NoticeMember> {
        let members = self.obligations.iter().filter_map(|(key, obligation)| {
            let reason = self.omitted.get(key)?;
            (notice_key(obligation) == *notice).then(|| NoticeMember {
                key: key.clone(),
                reason: reason.clone(),
                source: obligation.source.clone(),
                record_start: obligation.record_start,
                record_end: obligation.record_end,
            })
        });
        members.collect()
    }

    /// Source records GC must keep: every open image obligation and every open notice's members.
    pub(crate) fn protected_ranges(&self, ledger: &LedgerState) -> Vec<(SourceId, u64, u64)> {
        let mut ranges = Vec::new();
        for open in self.open_obligations(ledger) {
            match open {
                OpenObligation::Image { key, .. } => {
                    let record = self.obligations.get(&key);
                    ranges.extend(record.map(|o| (o.source.clone(), o.record_start, o.record_end)));
                }
                OpenObligation::Notice { key, .. } => {
                    let members = self.notice_members(&key).into_iter();
                    ranges.extend(members.map(|m| (m.source, m.record_start, m.record_end)));
                }
            }
        }
        ranges.sort_by(|a, b| (a.1, a.2).cmp(&(b.1, b.2)));
        ranges.dedup();
        ranges
    }

    pub(crate) fn obligation(&self, key: &UnitKey) -> Option<&ImageObligation> {
        self.obligations.get(key)
    }

    pub(crate) fn omitted(&self, key: &UnitKey) -> Option<&str> {
        self.omitted.get(key).map(String::as_str)
    }

    /// Image and notice units this ledger owes, so a piece of `key` is never settled by the
    /// ordinary disposition alone.
    pub(crate) fn owns(&self, key: &UnitKey) -> bool {
        self.obligations.contains_key(key) || self.notice_keys().any(|notice| notice == *key)
    }

    fn notice_keys(&self) -> impl Iterator<Item = UnitKey> + '_ {
        self.obligations
            .iter()
            .filter(|(key, _)| self.omitted.contains_key(*key))
            .map(|(_, obligation)| notice_key(obligation))
    }

    /// Everything not yet finished, each with the selector's next step; retire
    /// refuses and GC keeps the source records while any is listed.
    pub(crate) fn open_obligations(&self, ledger: &LedgerState) -> Vec<OpenObligation> {
        let mut open = Vec::new();
        let mut groups: BTreeMap<UnitKey, bool> = BTreeMap::new();
        for (key, obligation) in &self.obligations {
            let step = if self.omitted.contains_key(key) {
                None
            } else {
                image_step(&attempts(ledger, key))
            };
            let notice = notice_key(obligation);
            let finished = step.is_none();
            let group = groups.entry(notice).or_insert(true);
            *group &= finished;
            if let Some(step) = step {
                open.push(OpenObligation::Image {
                    key: key.clone(),
                    step,
                });
            }
        }
        // A result's notice is owed once every image of that result has ended and one is omitted.
        for (notice, finished) in groups {
            let omitted = self.obligations.iter().any(|(key, obligation)| {
                self.omitted.contains_key(key) && notice_key(obligation) == notice
            });
            if !(finished && omitted) {
                continue;
            }
            if let Some(step) = notice_step(&attempts(ledger, &notice)) {
                open.push(OpenObligation::Notice { key: notice, step });
            }
        }
        open
    }
}

fn open_span(spans: &mut Vec<TurnSpan>, turn: String, opener_offset: u64) -> Result<(), String> {
    if let Some(span) = spans.iter().find(|span| span.turn == turn) {
        if span.opener_offset == opener_offset {
            return Ok(());
        }
        return Err(format!("image turn {} reopened at {opener_offset}", turn));
    }
    // A new span starts after every recorded one and outside any of them.
    let blocked = spans.iter().any(|span| {
        span.opener_offset >= opener_offset || span.end_offset.is_none_or(|end| opener_offset < end)
    });
    if blocked {
        return Err(format!(
            "image turn {} opens at {opener_offset} inside or before another turn",
            turn
        ));
    }
    spans.push(TurnSpan {
        turn,
        opener_offset,
        end_offset: None,
    });
    Ok(())
}

fn close_span(spans: &mut [TurnSpan], turn: &str, end_offset: u64) -> Result<(), String> {
    let Some(position) = spans.iter().position(|span| span.turn == turn) else {
        return Err(format!("image turn {} closed unopened", turn));
    };
    let (opener, end) = (spans[position].opener_offset, spans[position].end_offset);
    // No later turn may start inside the closed range.
    let overlaps = spans
        .iter()
        .any(|span| span.opener_offset > opener && span.opener_offset < end_offset);
    match end {
        // A replayed or repeated close of the same boundary is the same fact.
        Some(end) if end == end_offset => Ok(()),
        Some(end) => Err(format!(
            "image turn {} closed at {end_offset} after {end}",
            turn
        )),
        None if end_offset <= opener || overlaps => {
            Err(format!("image turn {} cannot close at {end_offset}", turn))
        }
        None => {
            spans[position].end_offset = Some(end_offset);
            Ok(())
        }
    }
}

fn notice_key(obligation: &ImageObligation) -> UnitKey {
    UnitKey {
        native_key: notice_native_key(&obligation.turn, &obligation.call_id),
        ..obligation.unit_key.clone()
    }
}

/// `(serial, outcome)` of every attempt of `key`, oldest first.
fn attempts<'a>(ledger: &'a LedgerState, key: &UnitKey) -> Vec<(u64, Option<&'a PieceOutcome>)> {
    let pieces = ledger
        .pieces
        .iter()
        .filter(|(_, piece)| piece.unit_key == *key);
    pieces
        .map(|(&serial, piece)| (serial, piece.outcome.as_ref()))
        .collect()
}

fn counted(attempts: &[(u64, Option<&PieceOutcome>)]) -> usize {
    let unsent = |outcome: &Option<&PieceOutcome>| matches!(outcome, Some(PieceOutcome::NotSent));
    attempts
        .iter()
        .filter(|(_, outcome)| !unsent(outcome))
        .count()
}

/// What an attempt that did not post leaves behind; shared by images and notices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Waiting {
    /// Prepared without a result: the ordinary settlement owns it.
    Prepared { serial: u64 },
    /// Re-settled from its own stored payload and anchor, never posted again.
    Unresolved { serial: u64 },
    /// Kept open under an alarm; no operator path resolves it.
    Ambiguous { serial: u64 },
    /// Discord refused the request; kept open under the existing rejected alarm.
    Rejected { serial: u64, status: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageStep {
    /// No attempt yet, or the latest is proven unsent: attempt from the spool bytes.
    Attempt,
    /// The latest attempt is NotFound with a counted attempt left.
    RetryAfterNotFound {
        serial: u64,
    },
    /// The counted attempts are spent: record `ImageOmitted(not_confirmed)`.
    Omit {
        serial: u64,
    },
    Waiting(Waiting),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoticeStep {
    /// No notice attempt yet, or the latest is proven unsent.
    Attempt,
    RetryAfterNotFound {
        serial: u64,
    },
    /// NotFound after the last counted attempt; open under an alarm.
    Exhausted {
        serial: u64,
    },
    Waiting(Waiting),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenObligation {
    Image { key: UnitKey, step: ImageStep },
    Notice { key: UnitKey, step: NoticeStep },
}

fn waiting(serial: u64, outcome: Option<&PieceOutcome>) -> Option<Waiting> {
    match outcome {
        None => Some(Waiting::Prepared { serial }),
        Some(PieceOutcome::Unresolved(_)) => Some(Waiting::Unresolved { serial }),
        Some(PieceOutcome::Ambiguous(_)) => Some(Waiting::Ambiguous { serial }),
        Some(PieceOutcome::Rejected(status)) => Some(Waiting::Rejected {
            serial,
            status: *status,
        }),
        Some(PieceOutcome::Posted(_) | PieceOutcome::NotFound | PieceOutcome::NotSent) => None,
    }
}

/// `None` once the image is posted.
fn image_step(attempts: &[(u64, Option<&PieceOutcome>)]) -> Option<ImageStep> {
    let Some(&(serial, outcome)) = attempts.last() else {
        return Some(ImageStep::Attempt);
    };
    match outcome {
        Some(PieceOutcome::Posted(_)) => None,
        Some(PieceOutcome::NotSent) => Some(ImageStep::Attempt),
        Some(PieceOutcome::NotFound) if counted(attempts) < IMAGE_ATTEMPTS => {
            Some(ImageStep::RetryAfterNotFound { serial })
        }
        Some(PieceOutcome::NotFound) => Some(ImageStep::Omit { serial }),
        _ => waiting(serial, outcome).map(ImageStep::Waiting),
    }
}

/// `None` once the notice is posted.
fn notice_step(attempts: &[(u64, Option<&PieceOutcome>)]) -> Option<NoticeStep> {
    let Some(&(serial, outcome)) = attempts.last() else {
        return Some(NoticeStep::Attempt);
    };
    match outcome {
        Some(PieceOutcome::Posted(_)) => None,
        Some(PieceOutcome::NotSent) => Some(NoticeStep::Attempt),
        Some(PieceOutcome::NotFound) if counted(attempts) < NOTICE_ATTEMPTS => {
            Some(NoticeStep::RetryAfterNotFound { serial })
        }
        Some(PieceOutcome::NotFound) => Some(NoticeStep::Exhausted { serial }),
        _ => waiting(serial, outcome).map(NoticeStep::Waiting),
    }
}

/// How a `Posted` for `serial` relates to the anchor rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LateConfirm {
    /// Not a tool-image reconfirmation: the ordinary anchor and skew checks apply unchanged.
    Ordinary,
    /// A recorded Unresolved image or notice attempt found after newer posts; the anchor becomes
    /// `anchor`, never lower than it was.
    Accept {
        anchor: u64,
    },
    Reject(String),
}

/// Judges a late confirmation for the `Posted` reducer and replay.
pub(crate) fn late_confirm(ledger: &LedgerState, serial: u64, msg_id: u64) -> LateConfirm {
    let Some(piece) = ledger.pieces.get(&serial) else {
        return LateConfirm::Ordinary;
    };
    let unresolved = matches!(piece.outcome, Some(PieceOutcome::Unresolved(_)));
    if piece.unit_key.kind != UnitKind::ToolImage
        || !unresolved
        || !ledger.tool_images.owns(&piece.unit_key)
    {
        return LateConfirm::Ordinary;
    }
    if msg_id <= piece.anchor_id || msg_id <= ledger.floor {
        return LateConfirm::Reject(format!(
            "serial {serial} confirmed {msg_id} before its prepared anchor {}",
            piece.anchor_id
        ));
    }
    if ledger.posted.contains(&msg_id) {
        return LateConfirm::Reject(format!(
            "serial {serial} confirmed {msg_id}, which another serial owns"
        ));
    }
    LateConfirm::Accept {
        anchor: ledger.anchor.max(msg_id),
    }
}
