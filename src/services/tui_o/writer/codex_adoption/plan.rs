//! A verified Codex init, built only by the shared evaluator. It starts every bound source at
//! the end it read, so Legacy's consumed prefix is never recorded as delivered.

use super::coord::{self, NativeCheckpoint, WrapperCheckpoint};
use super::fold::{LivePane, fold};
use super::judge::{BUDGET_BYTES, BUDGET_SOURCES, Judgment, Verdict, judge};
use super::scan::{Scanned, scan};
use super::{Anchor, Evidence, Proof, Retirement, SourceEvidence, SourceRole, Suffix};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::store::InitSource;
use crate::services::tui_o::writer::binding::BindingEvent;
use crate::services::tui_o::writer::renumbered::first_named;

/// What a verifier read for one channel besides its channel-wide evidence.
pub struct Reads<'a> {
    pub events: &'a [BindingEvent],
    pub live: &'a [LivePane],
    pub checkpoints: &'a [NativeCheckpoint],
    pub wrappers: &'a [WrapperCheckpoint],
}

/// An init the evaluator found eligible; nothing else can build one.
#[derive(Debug)]
pub struct VerifiedCodexInit {
    channel: u64,
    seq: u64,
    sources: Vec<InitSource>,
    anchor: u64,
}

impl VerifiedCodexInit {
    pub fn channel(&self) -> u64 {
        self.channel
    }

    /// The binding log's last seq the plan folded.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn sources(&self) -> &[InitSource] {
        &self.sources
    }

    pub fn anchor(&self) -> u64 {
        self.anchor
    }
}

#[derive(Debug)]
pub enum Refusal {
    /// The binding log or the live panes refuse a fold.
    Fold(String),
    /// A bound source could not be read within budget.
    Read(String),
    Judged(Box<Judgment>),
}

/// Judges `facts` with every source the log binds read now; `facts.sources` is replaced.
pub fn verify(mut facts: Evidence, reads: Reads<'_>) -> Result<VerifiedCodexInit, Refusal> {
    let channel = facts.channel;
    // Each file under the identity the log first gave it, as O's own copy names it.
    let events = first_named(reads.events.to_vec());
    let folded = fold(&events, reads.live).map_err(Refusal::Fold)?;
    let count = folded.current.len() + folded.retired.len() + folded.named.len();
    if count > BUDGET_SOURCES {
        return Err(Refusal::Read(format!(
            "{count} sources exceed the read budget"
        )));
    }
    let mut budget = BUDGET_BYTES;
    let (mut sources, mut pinned) = (Vec::new(), Vec::new());
    for (pane, source, binds) in &folded.current {
        let scanned = read(source, &mut budget)?;
        let at_pane = |c: &&NativeCheckpoint| c.pane == *pane;
        let checkpoint = reads.checkpoints.iter().find(at_pane);
        let (marker, cursor) =
            coord::native_cursor(channel, (pane, binds.as_slice()), source, checkpoint);
        let file = coord::file_proof(channel, binds, source, &scanned);
        let live = reads.live.iter().find(|live| live.pane == *pane);
        let relay = live.and_then(|live| live.relay_output_path.as_deref());
        let wrapper = reads.wrappers.iter().find(|w| w.pane == *pane);
        let wrapper = coord::wrapper(channel, pane, relay, wrapper);
        budget = budget.saturating_sub(wrapper.as_ref().and_then(|w| w.eof).unwrap_or(0));
        sources.push(SourceEvidence {
            role: SourceRole::Current,
            proof: if file == Proof::Linked { marker } else { file },
            native_cursor: cursor,
            suffix: cursor.map_or(Suffix::Unknown, |cursor| scanned.suffix(cursor)),
            closed: scanned.closed,
            wrapper,
            ..read_evidence(&scanned)
        });
        // Two panes on one file pin it once.
        if !pinned.iter().any(|(pinned, _)| *pinned == source) {
            pinned.push((source, scanned));
        }
    }
    for (source, retirement, binds) in &folded.retired {
        let scanned = read(source, &mut budget)?;
        sources.push(SourceEvidence {
            role: SourceRole::Retired,
            proof: coord::file_proof(channel, binds, source, &scanned),
            retirement: *retirement,
            ..read_evidence(&scanned)
        });
        pinned.push((source, scanned));
    }
    for source in &folded.named {
        let meta = std::fs::metadata(&source.path).ok();
        let same = meta.filter(|meta| file_identity(meta).1 == source.ino);
        let bytes = same.map(|meta| meta.len());
        let role = SourceRole::Named;
        sources.push(SourceEvidence {
            role,
            bytes,
            ..SourceEvidence::default()
        });
    }
    facts.sources = sources;
    let judgment = judge(&facts);
    let anchor = match facts.anchor {
        Anchor::Latest { id } => id,
        _ => 0,
    };
    if judgment.boundary != Verdict::Eligible {
        return Err(Refusal::Judged(Box::new(judgment)));
    }
    let init = |(source, scanned): (&SourceId, Scanned)| InitSource {
        source_id: source.clone(),
        delivery_start: scanned.len,
        prefix_hash: scanned.hash,
    };
    Ok(VerifiedCodexInit {
        channel,
        seq: folded.seq,
        sources: pinned.into_iter().map(init).collect(),
        anchor,
    })
}

/// Reads `source` within what is left of the channel's budget.
fn read(source: &SourceId, budget: &mut u64) -> Result<Scanned, Refusal> {
    let scanned = scan(&source.path, *budget).map_err(Refusal::Read)?;
    *budget -= scanned.len;
    Ok(scanned)
}

fn read_evidence(scanned: &Scanned) -> SourceEvidence {
    SourceEvidence {
        bytes: Some(scanned.len),
        prefix: scanned.prefix,
        retirement: Retirement::Unknown,
        ..SourceEvidence::default()
    }
}
