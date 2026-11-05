//! Classification 2: comment-free matching inside the headless entry for the two scheduled-snapshot
//! isolation rules that no behavior test covers yet. A lexical tripwire only.

use crate::services::discord::router::source_guard_support::Lexed;

pub(crate) const HEADLESS_TURN: &str = include_str!("../../headless_turn.rs");

const ENTRY: &str = "async fn start_reserved_headless_turn_admitted(";
const SEVERANCE: &str = "let fresh_context_severance = goal_fresh || fresh_routine;";
const SNAPSHOT_SEVERANCE: &str = "goal_fresh || fresh_routine || scheduled_snapshot_turn";
const LOCAL_COLD_START: &str = "if scheduled_snapshot_turn {\n        session_id = None;";
const GUARDED_REFRESH: &str =
    "if !scheduled_snapshot_turn\n        && valid_routine_metadata(metadata.as_ref()).is_some()";

fn entry(source: &str) -> Result<Lexed, Vec<String>> {
    Lexed::new(source)
        .block(ENTRY)
        .ok_or_else(|| vec![format!("missing {ENTRY}")])
}

/// Scheduled snapshots stay out of channel-clearing severance and cold-start locally.
pub(crate) fn snapshot_severance_gaps(source: &str) -> Vec<String> {
    let src = match entry(source) {
        Ok(src) => src,
        Err(gaps) => return gaps,
    };
    let mut gaps = Vec::new();
    for kept in [SEVERANCE, LOCAL_COLD_START] {
        if !src.contains(kept) {
            gaps.push(format!("snapshot isolation lost: {kept}"));
        }
    }
    if src.text().contains(SNAPSHOT_SEVERANCE) {
        gaps.push("scheduled snapshot re-joined channel-clearing severance".to_string());
    }
    gaps
}

/// Snapshot metadata gates the legacy channel session-key refresh.
pub(crate) fn snapshot_refresh_gaps(source: &str) -> Vec<String> {
    entry(source).map_or_else(
        |gaps| gaps,
        |src| match src.contains(GUARDED_REFRESH) {
            true => Vec::new(),
            false => vec!["snapshot must not refresh the channel session key".to_string()],
        },
    )
}
