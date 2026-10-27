//! Bounded, read-only snapshots for delivery observers; never recover or repair the writer store.

use super::{CURSOR_DIR, LEDGER_FILE, OStore, ledger, rotation, spool};
use crate::services::tui_o::shadow::SourceId;
use serde::de::DeserializeOwned;
use std::{fs::File, io::Read, path::Path};

pub(crate) fn json<T: DeserializeOwned>(path: &Path, cap: u64) -> Option<T> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= cap)
        .then(|| serde_json::from_slice(&bytes).ok())
        .flatten()
}

pub(crate) fn source(
    root: &Path,
    channel: u64,
    path: &Path,
    file: (u64, u64),
) -> Option<spool::Cursor> {
    let store = OStore::existing(root)?;
    let init = store.read_init(channel).ok()??;
    let dir = store.channel_dir(channel);
    let mut sources: Vec<SourceId> = init.sources.into_iter().map(|s| s.source_id).collect();
    let boundary = dir.join(rotation::BOUNDARY_FILE);
    if boundary.exists() {
        let rotation: rotation::Rotation = json(&boundary, 1024 * 1024)?;
        sources.extend(rotation.links.into_values().map(|link| link.source));
    }
    let source = sources
        .into_iter()
        .find(|s| s.path == path && (s.dev, s.ino) == file)?;
    let cursor: spool::Cursor = json(
        &dir.join(CURSOR_DIR)
            .join(format!("{}.json", spool::source_key(&source))),
        64 * 1024,
    )?;
    (cursor.source == source).then_some(cursor)
}

pub(crate) fn local(root: &Path, channel: u64) -> Option<ledger::LedgerState> {
    let store = OStore::existing(root)?;
    let init = store.read_init(channel).ok()??;
    let mut file = File::open(store.channel_dir(channel).join(LEDGER_FILE)).ok()?;
    if file.metadata().ok()?.len() > 16 * 1024 * 1024 {
        return None;
    }
    let state = ledger::read_only(&mut file, init.initial_anchor).ok()?;
    state.violation().is_none().then_some(state)
}
