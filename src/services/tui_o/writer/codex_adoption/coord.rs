//! Where adoption reads a Codex source: the pane and execution that bound it, and offsets that
//! carry the namespace they count in, so a bare number never crosses from one file to another.

use std::path::{Path, PathBuf};

use super::scan::Scanned;
use super::{Load, Proof, Suffix, WrapperEvidence};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::writer::binding::BindingEvent;

/// A tmux pane under one execution; a restart under a new nonce is another pane.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pane {
    pub tmux: String,
    pub nonce: String,
}

impl Pane {
    pub fn of(event: &BindingEvent) -> Self {
        let (tmux, nonce) = (event.tmux_session.clone(), event.execution_nonce.clone());
        Self { tmux, nonce }
    }
}

/// The file an offset counts in: a native rollout, or one generation of a wrapper spool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Namespace {
    Native {
        path: PathBuf,
        ino: u64,
    },
    Wrapper {
        path: PathBuf,
        ino: u64,
        generation: u64,
    },
}

impl Namespace {
    pub fn path(&self) -> &Path {
        match self {
            Self::Native { path, .. } | Self::Wrapper { path, .. } => path,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offset {
    pub namespace: Namespace,
    pub at: u64,
}

impl Offset {
    /// The offset if it counts in `namespace`; a number from another namespace is none here.
    pub fn within(&self, namespace: &Namespace) -> Option<u64> {
        (self.namespace == *namespace).then_some(self.at)
    }
}

/// Legacy's native checkpoint for a pane, as its marker and ownership proof name it.
#[derive(Clone, Debug)]
pub struct NativeCheckpoint {
    pub channel: u64,
    pub pane: Pane,
    /// The binding event the ownership proof was issued for.
    pub proof_seq: u64,
    pub session: String,
    pub path: PathBuf,
    pub ino: u64,
    pub cursor: Offset,
}

/// A pane's normalized wrapper spool, read in its own namespace.
#[derive(Clone, Debug)]
pub struct WrapperCheckpoint {
    pub channel: u64,
    pub pane: Pane,
    pub namespace: Namespace,
    pub eof: u64,
    /// Legacy's read cursor in the spool.
    pub cursor: Offset,
    /// The confirmed delivery floor in the spool.
    pub floor: Offset,
    pub backlog: Load,
}

/// One bind of a source: the event's pane, seq, provider and channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub seq: u64,
    pub pane: Pane,
    pub provider: ShadowProvider,
    pub channel: u64,
}

/// Whether every bind of `source` is a Codex bind of `channel` and `scanned` read its inode and
/// session; a reboot may renumber the dev, never the inode.
pub fn file_proof(channel: u64, binds: &[Bind], source: &SourceId, scanned: &Scanned) -> Proof {
    let ours = |bind: &Bind| bind.provider == ShadowProvider::Codex && bind.channel == channel;
    let linked = !binds.is_empty()
        && binds.iter().all(ours)
        && scanned.file.1 == source.ino
        && scanned.session.as_deref() == Some(source.session_id.as_str());
    if linked {
        Proof::Linked
    } else {
        Proof::Mismatch
    }
}

/// Legacy's cursor on `source` when the checkpoint names this channel, pane, a bind of the source
/// on it, and the source's file and session, counted in the source's own namespace.
pub fn native_cursor(
    channel: u64,
    (pane, binds): (&Pane, &[Bind]),
    source: &SourceId,
    checkpoint: Option<&NativeCheckpoint>,
) -> (Proof, Option<u64>) {
    let Some(checkpoint) = checkpoint else {
        return (Proof::Unlinked, None);
    };
    let namespace = Namespace::Native {
        path: source.path.clone(),
        ino: source.ino,
    };
    let proven = binds
        .iter()
        .any(|b| b.seq == checkpoint.proof_seq && b.pane == *pane);
    let linked = proven
        && checkpoint.channel == channel
        && checkpoint.pane == *pane
        && (&checkpoint.session, &checkpoint.path, checkpoint.ino)
            == (&source.session_id, &source.path, source.ino);
    match (linked, checkpoint.cursor.within(&namespace)) {
        (true, Some(at)) => (Proof::Linked, Some(at)),
        _ => (Proof::Mismatch, None),
    }
}

/// A pane's wrapper read in its own namespace: none only when neither its live binding nor a
/// checkpoint names one. Bytes past the confirmed floor are not read as quiet.
pub fn wrapper(
    channel: u64,
    pane: &Pane,
    relay: Option<&Path>,
    checkpoint: Option<&WrapperCheckpoint>,
) -> Option<WrapperEvidence> {
    let checkpoint = match (relay, checkpoint) {
        (None, None) => return None,
        (Some(relay), Some(checkpoint)) if checkpoint.namespace.path() == relay => checkpoint,
        _ => return Some(WrapperEvidence::default()),
    };
    let namespace = &checkpoint.namespace;
    let (cursor, floor) = (
        checkpoint.cursor.within(namespace),
        checkpoint.floor.within(namespace),
    );
    let wrapped = matches!(namespace, Namespace::Wrapper { .. });
    let ours = wrapped && checkpoint.channel == channel && checkpoint.pane == *pane;
    let linked = ours && cursor.is_some() && floor.is_some();
    Some(WrapperEvidence {
        proof: if linked {
            Proof::Linked
        } else {
            Proof::Mismatch
        },
        cursor,
        eof: Some(checkpoint.eof),
        floor,
        suffix: match floor {
            Some(floor) if floor == checkpoint.eof => Suffix::Quiet,
            Some(_) => Suffix::Unrecognized,
            None => Suffix::Unknown,
        },
        backlog: checkpoint.backlog,
    })
}
