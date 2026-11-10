//! A verified Codex init, built only by the shared evaluator: each bound source starts at the end
//! it read, or the current one at Legacy's delivered end while a turn is held for O.

use std::io::Read;

use sha2::{Digest, Sha256};

use super::coord::{self, NativeCheckpoint, WrapperCheckpoint};
use super::fold::{LivePane, fold};
use super::judge::{BUDGET_BYTES, BUDGET_SOURCES, Judgment, Verdict, judge};
use super::scan::{Scanned, scan};
use super::{
    Anchor, Evidence, Load, Proof, Retirement, SourceEvidence, SourceRole, Suffix, Wrapper,
};
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
    let held = facts.obligations.values().any(|load| *load == Load::Held);
    let (mut sources, mut pinned) = (Vec::new(), Vec::new());
    for (pane, source, binds) in &folded.current {
        let scanned = read(source, &mut budget)?;
        let at_pane = |c: &&NativeCheckpoint| c.pane == *pane;
        let checkpoint = reads.checkpoints.iter().find(at_pane);
        let (marker, cursor) =
            coord::native_cursor(channel, (pane, binds.as_slice()), source, checkpoint);
        // Only a linked checkpoint's delivered end on a record boundary is a frontier.
        let frontier = (checkpoint.filter(|_| marker == Proof::Linked))
            .and_then(|checkpoint| coord::native_delivered(source, checkpoint))
            .filter(|at| scanned.suffix(*at) != Suffix::Partial);
        let file = coord::file_proof(channel, binds, source, &scanned);
        let live = reads.live.iter().find(|live| live.pane == *pane);
        let relay = live.and_then(|live| live.relay_output_path.as_deref());
        let wrapper = reads.wrappers.iter().find(|w| w.pane == *pane);
        // No live relay path and no checkpoint is the confirmed absence of a spool.
        let wrapper = coord::wrapper(channel, pane, relay, wrapper);
        let wrapper = wrapper.map_or(Wrapper::Absent, Wrapper::Present);
        budget = budget.saturating_sub(wrapper.present().and_then(|w| w.eof).unwrap_or(0));
        sources.push(SourceEvidence {
            role: SourceRole::Current,
            proof: if file == Proof::Linked { marker } else { file },
            native_cursor: cursor,
            frontier,
            suffix: cursor.map_or(Suffix::Unknown, |cursor| scanned.suffix(cursor)),
            closed: scanned.closed,
            wrapper,
            ..read_evidence(&scanned)
        });
        // Two panes on one file pin it once.
        if !pinned.iter().any(|(pinned, _, _)| *pinned == source) {
            pinned.push((source, scanned, frontier.filter(|_| held)));
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
        pinned.push((source, scanned, None));
    }
    for source in &folded.named {
        // Opened, not only stat'ed: a file that cannot be read is not a proven empty source.
        let meta = std::fs::File::open(&source.path).and_then(|file| file.metadata());
        let same = meta.ok().filter(|meta| file_identity(meta).1 == source.ino);
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
    let init = |(source, scanned, frontier): (&SourceId, Scanned, Option<u64>)| {
        let (delivery_start, prefix_hash) = match frontier {
            Some(at) if at != scanned.len => (at, hash_before(source, at)?),
            _ => (scanned.len, scanned.hash),
        };
        Ok(InitSource {
            source_id: source.clone(),
            delivery_start,
            prefix_hash,
        })
    };
    let sources = pinned.into_iter().map(init).collect::<Result<_, _>>();
    Ok(VerifiedCodexInit {
        channel,
        seq: folded.seq,
        sources: sources.map_err(Refusal::Read)?,
        anchor,
    })
}

/// The SHA-256 of the first `at` bytes of the same file the scan read.
fn hash_before(source: &SourceId, at: u64) -> Result<String, String> {
    let shown = source.path.display();
    let io = |error: std::io::Error| format!("source {shown}: {error}");
    let file = std::fs::File::open(&source.path).map_err(io)?;
    if file_identity(&file.metadata().map_err(io)?).1 != source.ino {
        return Err(format!("source {shown} was replaced"));
    }
    let mut bytes = Vec::new();
    file.take(at).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 != at {
        return Err(format!("source {shown} holds fewer than {at} bytes"));
    }
    Ok(hex::encode(Sha256::digest(&bytes)))
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
