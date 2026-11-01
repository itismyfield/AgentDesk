//! Whether a Claude transcript still sits before any output: metadata records of its own session,
//! the `/clear` echo a cleared session starts with, and at most one user prompt.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::codex::prefix::{Prefix, Verdict, read};
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};

/// Records Claude writes without a `sessionId` before any output.
const SESSIONLESS: &[&str] = &["file-history-snapshot"];
/// The user row a cleared session opens with; it echoes a local command, not a prompt.
const CLEAR_ECHO: &str = "<command-name>/clear</command-name>";

pub(in crate::services::tui_o::writer::late_adopt) fn judge(source: &SourceId) -> Verdict {
    let bytes = match read(source) {
        Ok(bytes) => bytes,
        Err(verdict) => return verdict,
    };
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1);
    let mut prompt_end = None;
    let mut proven = false;
    let mut start = 0;
    for line in bytes[..complete].split_inclusive(|b| *b == b'\n') {
        let end = start + line.len() as u64;
        match record(line, start, source) {
            Ok(Line::Blank) => {}
            Ok(Line::Sessionless) => {}
            Ok(Line::Meta) => proven = true,
            Ok(Line::Prompt) if prompt_end.is_none() => (proven, prompt_end) = (true, Some(end)),
            Ok(Line::Prompt) => return Verdict::Refuse(format!("a second user prompt at {start}")),
            Err(detail) => return Verdict::Refuse(detail),
        }
        start = end;
    }
    if !proven {
        return Verdict::Wait("no record of the bound session yet".into());
    }
    if complete != bytes.len() {
        return Verdict::Wait("the transcript ends inside a record".into());
    }
    Verdict::Eligible(Prefix {
        len: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(&bytes)),
        prompt_end,
    })
}

/// Whether the bytes from `from` to the end are complete metadata records of `source`'s session,
/// as Claude appends when its session ends or is cleared.
pub(in crate::services::tui_o::writer::late_adopt) fn metadata_since(
    source: &SourceId,
    from: u64,
) -> Result<(), String> {
    let bytes = read(source).map_err(|verdict| format!("{verdict:?}"))?;
    let tail = usize::try_from(from)
        .ok()
        .and_then(|from| bytes.get(from..))
        .ok_or_else(|| format!("the transcript is shorter than {from}"))?;
    if !tail.is_empty() && tail.last() != Some(&b'\n') {
        return Err("the transcript ends inside a record".into());
    }
    let mut start = from;
    for line in tail.split_inclusive(|b| *b == b'\n') {
        match record(line, start, source)? {
            Line::Blank | Line::Sessionless | Line::Meta => {}
            Line::Prompt => return Err(format!("a user prompt at {start}")),
        }
        start += line.len() as u64;
    }
    Ok(())
}

enum Line {
    Blank,
    /// Metadata that names no session, so it proves none.
    Sessionless,
    Meta,
    Prompt,
}

/// One complete line; an error names the record that is output, foreign or not judgeable.
fn record(line: &[u8], at: u64, source: &SourceId) -> Result<Line, String> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(Line::Blank);
    }
    let value = serde_json::from_slice::<Value>(line).ok();
    let value = value
        .filter(Value::is_object)
        .ok_or_else(|| format!("malformed record at {at}"))?;
    match value.get("sessionId").map(Value::as_str) {
        Some(Some(session)) if session == source.session_id => {}
        Some(_) => return Err(format!("record at {at} names another session")),
        None if SESSIONLESS.contains(&value["type"].as_str().unwrap_or("")) => {
            return Ok(Line::Sessionless);
        }
        None => return Err(format!("record at {at} names no session")),
    }
    let echo = value["message"]["content"]
        .as_str()
        .is_some_and(|text| text.trim_start().starts_with(CLEAR_ECHO));
    let mut prompt = false;
    for fact in classify(ShadowProvider::Claude, &value) {
        match fact {
            RecordFact::Prompt(..) if echo => {}
            RecordFact::Prompt(..) if !prompt => prompt = true,
            RecordFact::Prompt(..) => return Err(format!("a second user prompt at {at}")),
            RecordFact::Idle(_) => return Err(format!("a turn end at {at}")),
            _ => return Err(format!("output at {at}")),
        }
    }
    Ok(if prompt { Line::Prompt } else { Line::Meta })
}

#[cfg(test)]
#[path = "prefix_tests.rs"]
mod tests;
