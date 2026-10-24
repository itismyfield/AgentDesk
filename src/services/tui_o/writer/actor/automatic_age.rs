//! Age is known only from a complete, unchanged transcript at its captured cursor.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::capture::{FileMatch, Fingerprint, file_match};
use crate::services::tui_o::store::spool::Cursor;

const MAX_BYTES: u64 = 32 << 20;
const MIN_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Runs on the actor's blocking worker; unknown ages always retain the source.
#[cfg(all(test, unix))]
pub(super) fn last_dialogue(cursor: &Cursor) -> Option<SystemTime> {
    last_dialogue_for(cursor, ShadowProvider::Claude, None)
}

pub(super) fn last_dialogue_for(
    cursor: &Cursor,
    provider: ShadowProvider,
    provider_root: Option<&Path>,
) -> Option<SystemTime> {
    if cursor.captured_through > MAX_BYTES {
        return None;
    }
    let file = File::open(&cursor.source.path).ok()?;
    let before = Fingerprint::of(&file.metadata().ok()?).ok()?;
    if before.len != cursor.captured_through
        || file_match(&cursor.source, (before.dev, before.ino)) == FileMatch::Other
    {
        return None;
    }
    let mut bytes = Vec::new();
    (&file).take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 != cursor.captured_through
        || bytes.last() != Some(&b'\n')
        || hex::encode(Sha256::digest(&bytes)) != cursor.prefix_hash
        || Fingerprint::of(&file.metadata().ok()?).ok()? != before
        || Fingerprint::path(&cursor.source).ok()? != before
    {
        return None;
    }
    match provider {
        ShadowProvider::Claude => claude_dialogue(&bytes),
        // Native activity is an upper bound on dialogue; the audit field remains compatible.
        ShadowProvider::Codex => {
            super::codex_age::last_dialogue(&bytes, &cursor.source, provider_root?)
        }
    }
}

fn claude_dialogue(bytes: &[u8]) -> Option<SystemTime> {
    let mut last = None;
    // A bad row may hide newer dialogue, so it invalidates the entire age observation.
    for line in bytes.strip_suffix(b"\n")?.split(|byte| *byte == b'\n') {
        let row: serde_json::Value = serde_json::from_slice(line).ok()?;
        if !matches!(
            row.get("type").and_then(|kind| kind.as_str()),
            Some("user" | "assistant")
        ) {
            continue;
        }
        let timestamp = row.get("timestamp")?.as_str()?;
        let timestamp = DateTime::parse_from_rfc3339(timestamp)
            .ok()?
            .with_timezone(&Utc);
        let timestamp = SystemTime::from(timestamp);
        last = Some(last.map_or(timestamp, |previous: SystemTime| previous.max(timestamp)));
    }
    last
}

pub(super) fn old_enough(last: Option<SystemTime>, now: SystemTime) -> bool {
    last.is_some_and(|last| now.duration_since(last).is_ok_and(|age| age >= MIN_AGE))
}

#[cfg(all(test, unix))]
#[path = "automatic_age_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "codex_age_tests.rs"]
pub(super) mod codex_tests;
