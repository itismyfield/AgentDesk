//! The resume start a SessionStart hook was accepted for, kept on disk so a restarted daemon can
//! tell the composer this execution resumed into from a turn of this run.
use super::evidence::{Stamp, Tail};
use super::source::Source;
use crate::services::tui_prompt_dedupe::binding_context::{self, BindingContext};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    tmux_session: String,
    execution_nonce: String,
    native: String,
    path: PathBuf,
    file: (u64, u64),
    /// The transcript length at acceptance: anything after it is a turn of this run.
    len: u64,
    launched: DateTime<Utc>,
    published_at: DateTime<Utc>,
}
impl Record {
    pub(super) fn accepted(
        ctx: &BindingContext,
        source: &Source,
        stamp: &Stamp,
        published_at: DateTime<Utc>,
    ) -> Self {
        Self {
            tmux_session: ctx.tmux_session.clone(),
            execution_nonce: ctx.execution_nonce.clone(),
            native: source.native.clone(),
            path: source.path.clone(),
            file: stamp.file,
            len: stamp.len,
            launched: ctx.created_at,
            published_at,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Written {
    /// No byte of this record replaced the previous file.
    NotPublished,
    /// The whole record is in place; only its directory entry may not survive a crash.
    Published,
    Durable,
}

pub(super) fn record_path(nonce: &str) -> Option<PathBuf> {
    if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let root = crate::config::runtime_root()?;
    Some(
        root.join("runtime/claude_resume_starts")
            .join(format!("{nonce}.json")),
    )
}

/// Never refuses the hook: a start left unrecorded only keeps a later restart's floor.
pub(super) fn write(
    record: &Record,
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Written {
    let Some(path) = record_path(&record.execution_nonce) else {
        return Written::NotPublished;
    };
    let text = match serde_json::to_string(record) {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(%error, "Claude resume start not recorded");
            return Written::NotPublished;
        }
    };
    if let Err(failed) =
        crate::domain::atomic_write::prepare(&path, &text).and_then(|prepared| prepared.publish())
    {
        tracing::warn!(stage = failed.stage.prefix(), error = %failed.error,
            "Claude resume start not recorded; a restart before its first turn keeps the floor");
        return Written::NotPublished;
    }
    match sync_parent(&path) {
        Ok(()) => Written::Durable,
        Err(error) => {
            tracing::warn!(%error, "Claude resume start recorded without a directory sync");
            Written::Published
        }
    }
}

/// The current execution's recorded start while it still names this transcript and `tail`, the
/// same bytes the observation judges, shows nothing of this run after the recorded length.
pub(super) fn restored(source: &Source, tail: &Tail) -> Option<(u64, DateTime<Utc>)> {
    let nonce = source.nonce.as_deref()?;
    let record: Record = serde_json::from_slice(&std::fs::read(record_path(nonce)?).ok()?).ok()?;
    let ctx = binding_context::execution_context("claude", nonce).ok()?;
    (record.execution_nonce == nonce
        && record.tmux_session == source.session
        && ctx.tmux_session == source.session
        && record.native == source.native
        && record.path == source.path
        && record.file == tail.stamp.file
        && record.launched == ctx.created_at
        && record.published_at >= ctx.created_at
        && tail.stamp.len >= record.len
        && tail.quiet_from(record.len, ctx.created_at))
    .then_some((record.len, ctx.created_at))
}
