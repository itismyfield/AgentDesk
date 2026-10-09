//! A Codex channel's binding log folded per pane and execution: each live pane's current source,
//! and every other bound source retired only by a later bind naming it old or by its exit.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::Retirement;
use super::coord::{Bind, Pane};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::adoption::logged;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingRecord, BindingTarget};

/// A live runtime binding: the files its pane reads now.
#[derive(Clone, Debug)]
pub struct LivePane {
    pub pane: Pane,
    pub output_path: PathBuf,
    pub relay_output_path: Option<PathBuf>,
}

/// Every source the log names: bound ones with their binds, then those only named old or parent.
#[derive(Clone, Debug)]
pub struct Folded {
    pub seq: u64,
    /// Each live pane with the source it binds now.
    pub current: Vec<(Pane, SourceId, Vec<Bind>)>,
    pub retired: Vec<(SourceId, Retirement, Vec<Bind>)>,
    pub named: Vec<SourceId>,
}

fn bound(event: &BindingEvent) -> Option<&SourceId> {
    match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some(source),
        _ => None,
    }
}

fn old(event: &BindingEvent) -> Option<&SourceId> {
    match &event.record {
        BindingRecord::Bound { old, .. } => old.as_ref(),
        _ => None,
    }
}

/// Folds `events` against the panes live now; a pending bind, a live pane with no bound source or
/// reading another file refuses the whole channel.
pub fn fold(events: &[BindingEvent], live: &[LivePane]) -> Result<Folded, String> {
    let (bound_sources, named) = logged(events).map_err(String::from)?;
    let mut latest: BTreeMap<Pane, &SourceId> = BTreeMap::new();
    for event in events {
        if let Some(source) = bound(event) {
            latest.insert(Pane::of(event), source);
        }
    }
    let binds = |source: &SourceId| -> Vec<Bind> {
        let binding = events.iter().filter(|event| bound(event) == Some(source));
        let bind = |event: &BindingEvent| Bind {
            seq: event.seq,
            pane: Pane::of(event),
            provider: event.provider,
            channel: event.channel_id,
        };
        binding.map(bind).collect()
    };
    let mut current = Vec::new();
    for pane in live {
        let tmux = &pane.pane.tmux;
        let source = latest.get(&pane.pane);
        let source = source.ok_or_else(|| format!("live pane {tmux} binds no source"))?;
        if pane.output_path != source.path {
            return Err(format!("live pane {tmux} reads another file than it binds"));
        }
        current.push((pane.pane.clone(), (*source).clone(), binds(source)));
    }
    let mut retired: Vec<(SourceId, Retirement, Vec<Bind>)> = Vec::new();
    // Newest first, as a store reopens past sources.
    for source in bound_sources.into_iter().rev() {
        let known = current.iter().any(|(_, s, _)| s == source)
            || retired.iter().any(|(s, _, _)| s == source);
        if !known {
            let binds = binds(source);
            retired.push((
                source.clone(),
                retirement(events, live, source, &binds),
                binds,
            ));
        }
    }
    Ok(Folded {
        seq: events.last().map_or(0, |event| event.seq),
        current,
        retired,
        named: named.into_iter().cloned().collect(),
    })
}

/// Retired when no live binding reads the file, and either a later bind on a pane that bound it
/// named it old, or every execution that bound it has ended.
fn retirement(
    events: &[BindingEvent],
    live: &[LivePane],
    source: &SourceId,
    binds: &[Bind],
) -> Retirement {
    let reads = |path: &PathBuf| *path == source.path;
    let read = |pane: &LivePane| {
        reads(&pane.output_path) || pane.relay_output_path.as_ref().is_some_and(reads)
    };
    if live.iter().any(read) {
        return Retirement::Live;
    }
    let last = |pane: &Pane| {
        binds
            .iter()
            .filter(|b| b.pane == *pane)
            .map(|b| b.seq)
            .max()
    };
    let replaced = events.iter().any(|later| {
        old(later) == Some(source) && last(&Pane::of(later)).is_some_and(|seq| seq < later.seq)
    });
    if replaced {
        return Retirement::Replaced;
    }
    let ended = |bind: &Bind| {
        let nonce = &bind.pane.nonce;
        !nonce.is_empty() && live.iter().all(|pane| pane.pane.nonce != *nonce)
    };
    if !binds.is_empty() && binds.iter().all(ended) {
        return Retirement::Exited;
    }
    Retirement::Missing
}
