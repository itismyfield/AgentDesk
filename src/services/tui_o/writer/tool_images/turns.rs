//! The native turn an image belongs to. Pure: `derive.rs` feeds it each record's facts and
//! queues the spans it reports.

use std::collections::HashMap;

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
    Closed {
        turn: NativeTurn,
        end_offset: u64,
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
        }
    }

    /// Runs before any fact of the record at `offset`: a restored turn whose durable Closed ends
    /// at or before it is over, so that record's prompt can open the next turn.
    pub(crate) fn before_offset(&mut self, offset: u64) {
        if let State::Open(open) = &self.state
            && open.closes_at.is_some_and(|end| end <= offset)
        {
            self.state = State::Idle;
        }
    }

    /// Mirrors `TurnTracker::observe` (seal.rs:98-128); returns the turn a native idle closed.
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
            (RecordFact::Idle(_), _) => self.end(offset),
            _ => None,
        }
    }

    /// A verified physical end (`end_projection`); calls keep their turns for late results.
    pub(crate) fn physical_end(&mut self, end_offset: u64) -> Option<TurnEdge> {
        self.end(end_offset)
    }

    fn end(&mut self, end_offset: u64) -> Option<TurnEdge> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Open(open) => Some(TurnEdge::Closed {
                turn: open.turn,
                end_offset,
            }),
            State::Idle | State::Unknown => None,
        }
    }

    /// A tool call seen in the open turn; its result belongs there even after the turn ends.
    pub(crate) fn note_call(&mut self, call_id: &str) {
        if let State::Open(open) = &self.state {
            self.calls.insert(call_id.to_owned(), open.turn.clone());
        }
    }

    /// The turn of an image in `call_id`'s result at `offset`: the call's turn, the open turn,
    /// the stored span holding `offset`, else the edge turn; a body key is never a turn.
    pub(crate) fn turn_for_image(
        &mut self,
        call_id: &str,
        offset: u64,
        spans: &ImageState,
    ) -> ImageTurn {
        if self.state == State::Unknown
            && let Some(span) = spans.span_at(&self.source, offset)
        {
            self.state = State::Open(OpenTurn {
                turn: NativeTurn::new(span.turn.clone()),
                opener_offset: span.opener_offset,
                closes_at: span.end_offset,
            });
        }
        let open = match &self.state {
            State::Open(open) => Some(open),
            State::Idle | State::Unknown => None,
        };
        let turn = self
            .calls
            .get(call_id)
            .cloned()
            .or_else(|| open.map(|open| open.turn.clone()))
            .or_else(|| {
                spans
                    .span_at(&self.source, offset)
                    .map(|span| NativeTurn::new(span.turn.clone()))
            })
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

    pub(crate) fn source(&self) -> &SourceId {
        &self.source
    }

    /// The turn this source is in, observed or restored from its span.
    pub(crate) fn open_turn(&self) -> Option<&NativeTurn> {
        match &self.state {
            State::Open(open) => Some(&open.turn),
            State::Idle | State::Unknown => None,
        }
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
