//! The native turn an image belongs to. Pure: the writer wiring feeds it each record's facts
//! and appends the spans it reports.

use std::collections::{BTreeSet, HashMap};

use super::NativeTurn;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::identity::RecordFact;
use crate::services::tui_o::store::ledger::tool_images::ImageState;
use crate::services::tui_o::store::spool::source_key;

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Reading began mid-file, or the open turn had no identity.
    Unknown,
    Open(OpenTurn),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenTurn {
    turn: NativeTurn,
    opener_offset: u64,
    /// The durable Closed boundary of a turn restored from its span.
    closes_at: Option<u64>,
}

/// A turn boundary the wiring records: `Opened` only when the turn has no span yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnEdge {
    Opened {
        turn: NativeTurn,
        opener_offset: u64,
    },
    /// `pending_calls` are this turn's calls still waiting for a result: the wiring records
    /// each as `ImageCall` with the Closed, before GC can collect the call's evidence.
    Closed {
        turn: NativeTurn,
        end_offset: u64,
        pending_calls: Vec<String>,
    },
}

/// The turn of one image occurrence, and the span to open first when it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageTurn {
    pub(crate) turn: NativeTurn,
    pub(crate) open_span: Option<TurnEdge>,
}

/// Image turn tracking for one source; turns never span sources.
#[derive(Clone, Debug)]
pub(crate) struct ImageTurns {
    source: SourceId,
    /// The source's durable delivery start: the only edge-turn identity.
    origin: u64,
    state: State,
    /// Call id to the turn of its call, kept past the turn's end for late results.
    calls: HashMap<String, NativeTurn>,
    /// Calls without a result yet; reported when their turn ends.
    pending: BTreeSet<String>,
}

impl ImageTurns {
    /// `first_offset` is where this run reads `source` from: mid-file starts unknown.
    pub(crate) fn starting_at(source: SourceId, first_offset: u64, origin: u64) -> Self {
        let state = match first_offset {
            0 => State::Idle,
            _ => State::Unknown,
        };
        Self {
            source,
            origin,
            state,
            calls: HashMap::new(),
            pending: BTreeSet::new(),
        }
    }

    /// Prepares the record at `offset` before any of its facts or calls: an unknown tracker
    /// restores the durable span holding `offset`, and a restored turn ends at its Closed.
    pub(crate) fn before_offset(&mut self, offset: u64, spans: &ImageState) {
        if self.state == State::Unknown
            && let Some(span) = spans.span_at(&self.source, offset)
        {
            self.state = State::Open(OpenTurn {
                turn: NativeTurn::new(span.turn.clone()),
                opener_offset: span.opener_offset,
                closes_at: span.end_offset,
            });
        }
        if let State::Open(open) = &self.state
            && open.closes_at.is_some_and(|end| end <= offset)
        {
            self.state = State::Idle;
        }
    }

    /// Follows `TurnTracker::observe`; an idle naming another turn leaves the open turn, as the
    /// input facts and the restart projection do. Returns the turn an idle closed.
    pub(crate) fn observe(
        &mut self,
        fact: &RecordFact,
        row_key: Option<&str>,
        offset: u64,
    ) -> Option<TurnEdge> {
        let opened = |id: Option<&str>| match id {
            Some(id) => State::Open(OpenTurn {
                turn: NativeTurn::new(id),
                opener_offset: offset,
                closes_at: None,
            }),
            None => State::Unknown,
        };
        match (fact, &self.state) {
            // A prompt inside an open turn never reopens it.
            (RecordFact::Prompt(..), State::Open(_)) => None,
            (RecordFact::Prompt(true, _), _) if row_key.is_some() => {
                self.state = opened(row_key);
                None
            }
            (RecordFact::Assistant, State::Idle) => {
                self.state = opened(row_key);
                None
            }
            (RecordFact::TurnStart(turn_id), _) => {
                self.state = opened(turn_id.as_deref());
                None
            }
            (RecordFact::Idle(Some(close)), State::Open(open)) if *close != open.turn.as_str() => {
                None
            }
            (RecordFact::Idle(_), _) => self.end(offset),
            _ => None,
        }
    }

    /// A verified physical end (`end_projection`); calls keep their turns for late results.
    pub(crate) fn physical_end(&mut self, end_offset: u64) -> Option<TurnEdge> {
        self.end(end_offset)
    }

    fn end(&mut self, end_offset: u64) -> Option<TurnEdge> {
        let State::Open(open) = std::mem::replace(&mut self.state, State::Idle) else {
            return None;
        };
        let of_turn = |call: &String| self.calls.get(call) == Some(&open.turn);
        let pending_calls: Vec<String> = self
            .pending
            .iter()
            .filter(|c| of_turn(c))
            .cloned()
            .collect();
        for call in &pending_calls {
            self.pending.remove(call);
        }
        Some(TurnEdge::Closed {
            turn: open.turn,
            end_offset,
            pending_calls,
        })
    }

    /// A tool call seen in the open turn; its result belongs there even after the turn ends.
    pub(crate) fn note_call(&mut self, call_id: &str) {
        if let State::Open(open) = &self.state {
            self.calls.insert(call_id.to_owned(), open.turn.clone());
            self.pending.insert(call_id.to_owned());
        }
    }

    /// A tool result arrived (with or without images); its call no longer needs attribution.
    pub(crate) fn note_result(&mut self, call_id: &str) {
        self.pending.remove(call_id);
    }

    /// The turn of an image in `call_id`'s result: the call's turn (memory, then the durable
    /// attribution), the open turn, else the edge turn; a body key is never a turn.
    pub(crate) fn turn_for_image(&self, call_id: &str, spans: &ImageState) -> ImageTurn {
        let open = match &self.state {
            State::Open(open) => Some(open),
            State::Idle | State::Unknown => None,
        };
        let durable = || spans.call_turn(&self.source, call_id).map(NativeTurn::new);
        let turn = self
            .calls
            .get(call_id)
            .cloned()
            .or_else(durable)
            .or_else(|| open.map(|open| open.turn.clone()))
            .unwrap_or_else(|| self.edge_turn());
        let open_span = open
            .filter(|open| {
                open.turn == turn && spans.span_of(&self.source, turn.as_str()).is_none()
            })
            .map(|open| TurnEdge::Opened {
                turn: turn.clone(),
                opener_offset: open.opener_offset,
            });
        ImageTurn { turn, open_span }
    }

    fn edge_turn(&self) -> NativeTurn {
        NativeTurn::new(format!("edge:{}:{}", source_key(&self.source), self.origin))
    }

    /// Whether GC may collect the segment holding `turn`'s opener: an open turn or an unclosed
    /// span keeps it until its Closed is durable, even while that append fails.
    pub(crate) fn opener_collectable(&self, spans: &ImageState, turn: &NativeTurn) -> bool {
        let live = |open: &OpenTurn| open.turn == *turn && open.closes_at.is_none();
        if matches!(&self.state, State::Open(open) if live(open)) {
            return false;
        }
        spans
            .span_of(&self.source, turn.as_str())
            .is_none_or(|span| span.end_offset.is_some())
    }
}
