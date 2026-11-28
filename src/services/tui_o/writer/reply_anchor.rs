//! The Discord message a piece replies to: the native run holding the piece's record, then that
//! run's opener input, else its first joined input. Resolved right before `Prepared`.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::services::tui_input::human_frame::history::v2::native::Ticket;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::store::anchor_bindings::AnchorBinding;

/// A source not settled for this long stops holding pieces; they post without a reference.
const STALE: Duration = Duration::from_secs(10);

/// The stored source and record range a piece was derived from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coord {
    pub source: SourceId,
    pub range: Range<u64>,
}

/// Where a piece's reply comes from: resolved at its coordinate, or fixed by an earlier serial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyRef {
    At(Coord),
    Fixed(u64),
}

/// One native run and the anchors its membership names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunAnchor {
    pub(crate) start: u64,
    /// The closing record's end; `None` while the run is open.
    pub(crate) end: Option<u64>,
    pub(crate) opener: Option<u64>,
    pub(crate) joined: Vec<u64>,
    pub(crate) contested: bool,
}

impl RunAnchor {
    pub(crate) fn opened(start: u64) -> Self {
        Self {
            start,
            ..Self::default()
        }
    }
}

/// The Discord message an admitted ticket names.
pub(crate) fn anchor_of(ticket: &Ticket) -> Option<u64> {
    ticket.key["identity"]["primary"]
        .as_u64()
        .filter(|id| *id != 0)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    /// Native analysis has not covered the piece yet.
    Wait,
    /// Settled with no anchor for the piece's run, or no native view of its source.
    Unbound,
    /// `record` is set while `binding` is not yet durable; `marks` are joined inputs not yet marked.
    To {
        binding: AnchorBinding,
        record: bool,
        marks: Vec<u64>,
    },
}

struct View {
    settled: u64,
    at: Instant,
    runs: Vec<RunAnchor>,
    /// The analysed coverage and registrations the runs were computed from.
    key: Option<(u64, String)>,
}

#[derive(Default)]
struct State {
    views: HashMap<SourceId, View>,
    pins: HashMap<(SourceId, u64), AnchorBinding>,
}

/// Shared by the native source reader, which settles views, and the writer, which resolves.
#[derive(Clone, Default)]
pub(crate) struct Anchors(Arc<Mutex<State>>);

impl Anchors {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// An opened native source holds its pieces until its first settled analysis.
    pub(crate) fn track(&self, source: &SourceId) {
        let view = View {
            settled: 0,
            at: Instant::now(),
            runs: Vec::new(),
            key: None,
        };
        self.state().views.entry(source.clone()).or_insert(view);
    }

    /// Analysis through `key.0` with no receipt left to record; runs are recomputed only when
    /// the coverage or the registrations changed.
    pub(crate) fn settle(
        &self,
        source: &SourceId,
        key: (u64, String),
        runs: impl FnOnce() -> Result<Vec<RunAnchor>, String>,
    ) -> bool {
        if let Some(view) = self.state().views.get_mut(source)
            && view.key.as_ref() == Some(&key)
        {
            view.at = Instant::now();
            return true;
        }
        let Ok(runs) = runs() else { return false };
        let view = View {
            settled: key.0,
            at: Instant::now(),
            runs,
            key: Some(key),
        };
        self.state().views.insert(source.clone(), view);
        true
    }

    #[cfg(unix)]
    pub(crate) fn refresh_settled(&self, source: &SourceId, through: u64) {
        if let Some(view) = self.state().views.get_mut(source)
            && view.settled == through
            && view.key.is_some()
        {
            view.at = Instant::now();
        }
    }

    /// Durable bindings, read back at start; a later line for the same run wins.
    pub(crate) fn pinned(&self, bindings: impl IntoIterator<Item = AnchorBinding>) {
        let mut state = self.state();
        for binding in bindings {
            state
                .pins
                .insert((binding.source.clone(), binding.run_start), binding);
        }
    }

    pub(crate) fn rekey(&self, old: &SourceId, new: &SourceId, bindings: Vec<AnchorBinding>) {
        let mut state = self.state();
        state.views.remove(old);
        state
            .pins
            .retain(|(source, _), _| source != old && source != new);
        for binding in bindings
            .into_iter()
            .filter(|binding| &binding.source == new)
        {
            state.pins.insert((new.clone(), binding.run_start), binding);
        }
    }

    pub(crate) fn resolve(&self, coord: &Coord) -> Reply {
        let state = self.state();
        let Some(view) = state.views.get(&coord.source) else {
            return Reply::Unbound;
        };
        if view.settled < coord.range.end {
            return if view.at.elapsed() < STALE {
                Reply::Wait
            } else {
                Reply::Unbound
            };
        }
        let at = coord.range.start;
        let Some(run) = view
            .runs
            .iter()
            .rev()
            .find(|run| run.start <= at && run.end.is_none_or(|end| at < end))
        else {
            return Reply::Unbound;
        };
        let pin = state.pins.get(&(coord.source.clone(), run.start));
        let marked = pin.map_or(&[][..], |pin| pin.joined.as_slice());
        let marks: Vec<u64> = run
            .joined
            .iter()
            .copied()
            .filter(|j| !marked.contains(j))
            .collect();
        // A durable anchor outlives a later analysis that no longer names it.
        let anchor = pin
            .map(|pin| pin.anchor)
            .or(run.opener)
            .or(run.joined.first().copied());
        let Some(anchor) = anchor else {
            return Reply::Unbound;
        };
        let mut joined = marked.to_vec();
        joined.extend(&marks);
        Reply::To {
            binding: AnchorBinding {
                source: coord.source.clone(),
                run_start: run.start,
                anchor,
                joined,
            },
            record: pin.is_none() || !marks.is_empty(),
            marks,
        }
    }
}

#[cfg(all(test, unix))]
#[path = "reply_anchor_tests.rs"]
mod tests;
