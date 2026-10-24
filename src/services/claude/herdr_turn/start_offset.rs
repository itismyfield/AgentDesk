//! The live reader and terminal replay's timestamp record boundary.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{CancelToken, SourceFileIdentity, cancel_requested};

pub(super) fn eligible(line: &str, submitted_at: &chrono::DateTime<chrono::Utc>) -> bool {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let Some(stamp) = json.get("timestamp").and_then(serde_json::Value::as_str) else {
        return false;
    };
    chrono::DateTime::parse_from_rfc3339(stamp).is_ok_and(|stamp| stamp >= *submitted_at)
}

/// No offset is committed until a whole stamped record exists; metadata and a growing record
/// retain their raw byte positions while the file catches up with the submitted input.
pub(super) fn wait(
    transcript: &str,
    fallback: u64,
    token: &Arc<CancelToken>,
    ended: &impl Fn((u64, SourceFileIdentity)) -> Option<String>,
) -> Result<Option<u64>, String> {
    if cancel_requested(Some(token)) {
        return Ok(None);
    }
    let submitted_at = token
        .herdr_interrupt_state()
        .and_then(|state| state.turn_start.get().and_then(|start| start.submitted_at));
    let Some(submitted_at) = submitted_at else {
        return Ok(Some(fallback));
    };
    let path = Path::new(transcript);
    if super::before_start::mutant("start_offset_fallback_zero")
        || super::before_start::mutant("start_offset_replay_only")
    {
        #[cfg(test)]
        test_hooks::poll(path);
        return Ok(Some(
            if super::before_start::mutant("start_offset_fallback_zero") {
                0
            } else {
                fallback
            },
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let file = loop {
        if cancel_requested(Some(token)) {
            return Ok(None);
        }
        match std::fs::File::open(path) {
            Ok(file) => break file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Failed to open turn transcript: {error}")),
        }
        #[cfg(test)]
        test_hooks::poll(path);
        pause(deadline)?;
    };
    if super::before_start::mutant("start_offset_file_exists_only") {
        #[cfg(test)]
        test_hooks::poll(path);
        return Ok(Some(fallback));
    }
    let identity = SourceFileIdentity::from_open_file(&file);
    let mut reader = BufReader::new(file);
    let (mut record, mut offset) = (Vec::new(), 0u64);
    loop {
        if cancel_requested(Some(token)) {
            return Ok(None);
        }
        reader
            .read_until(b'\n', &mut record)
            .map_err(|error| format!("Failed to read turn transcript: {error}"))?;
        if record.last() == Some(&b'\n') {
            if std::str::from_utf8(&record).is_ok_and(|line| eligible(line, &submitted_at)) {
                return Ok(Some(offset));
            }
            offset = offset.saturating_add(record.len() as u64);
            record.clear();
            continue;
        }
        if crate::services::tmux_common::rotation_target_was_swapped(reader.get_ref(), path)
            .map_err(|error| format!("Failed to verify turn transcript identity: {error}"))?
        {
            return Err("Output file rotated before a turn timestamp record".into());
        }
        #[cfg(test)]
        test_hooks::poll(path);
        if ended((offset, identity)).is_some() {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pause(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("Timeout waiting for turn transcript file".into());
    }
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

#[cfg(test)]
#[path = "start_offset_tests.rs"]
pub(crate) mod test_hooks;
