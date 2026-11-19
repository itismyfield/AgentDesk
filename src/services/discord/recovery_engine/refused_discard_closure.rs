//! The native turn a discarded body belongs to: its raw rollout source and the opener of the turn
//! its raw anchor falls in. The row's replay coordinates are read, never moved.

use super::*;
use crate::db::tui_output_disposition::closure::Closure;
use crate::services::tui_o::shadow::SourceId;

/// `row`'s raw source and a raw offset inside its turn. Offsets that name a normalized replay
/// namespace count only through the raw turn their proof verified.
fn raw_anchor(row: &inflight::InflightTurnState) -> Option<(String, SourceId, u64)> {
    let (session_id, path, (dev, ino), anchor) = match &row.legacy_replay_coordinate {
        Some(marker) => {
            let raw = marker.raw.as_ref()?;
            let (session_id, path) = (raw.session_id.clone(), raw.path.clone());
            (session_id, path, raw.file_identity, raw.turn_start)
        }
        None => (
            row.session_id.clone()?,
            row.output_path.clone()?,
            row.tui_terminal_source_file_identity?,
            row.turn_start_offset?,
        ),
    };
    let source = SourceId {
        session_id,
        path: path.clone().into(),
        dev,
        ino,
    };
    Some((path, source, anchor))
}

/// The closure of a Codex row's turn; none when the row names no raw turn of a readable rollout.
pub(super) async fn of(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
) -> Option<Closure> {
    if !matches!(provider, ProviderKind::Codex) {
        return None;
    }
    let (path, source, anchor) = raw_anchor(row)?;
    let read = source.clone();
    let native = crate::services::tui_o::writer::late_adopt::native_turn;
    let blocking = tokio::task::spawn_blocking(move || native(&read, anchor));
    let (from, until) = blocking.await.ok()??;
    Some(Closure {
        session_id: source.session_id,
        path,
        dev: source.dev,
        ino: source.ino,
        from,
        until,
    })
}
