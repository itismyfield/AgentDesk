//! Complete, bounded transcript evidence; shared turn-state classification stays unchanged.
use serde_json::Value;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub file: (u64, u64),
    pub len: u64,
    modified: Option<SystemTime>,
}
impl Stamp {
    pub(super) fn read(path: &Path) -> Option<Self> {
        Self::of(&std::fs::metadata(path).ok()?)
    }
    fn of(meta: &std::fs::Metadata) -> Option<Self> {
        if !meta.is_file() {
            return None;
        }
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let file = (0, 0);
        Some(Self {
            file,
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    None,
    Hard,
    Summary,
}
pub(super) struct Tail {
    pub stamp: Stamp,
    pub end: End,
    pub terminal: u64,
    pub activity: u64,
    pub activity_start: u64,
    pub assistant: Option<String>,
    pub activity_at: Option<chrono::DateTime<chrono::Utc>>,
    pub empty: bool,
    pub native: Option<String>,
}
pub(super) fn read(path: &Path) -> Option<Tail> {
    let mut file = std::fs::File::open(path).ok()?;
    let stamp = Stamp::of(&file.metadata().ok()?)?;
    let start = stamp.len.saturating_sub(256 * 1024);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut offset = start;
    if start > 0 {
        offset += reader.read_until(b'\n', &mut line).ok()? as u64;
        line.clear();
    }
    let mut tail = Tail {
        stamp,
        end: End::None,
        terminal: 0,
        activity: 0,
        activity_start: 0,
        assistant: None,
        activity_at: None,
        empty: start == 0,
        native: None,
    };
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        let record_start = offset;
        offset += line.len() as u64;
        if !line.ends_with(b"\n") {
            return None;
        }
        let record: Value = serde_json::from_slice(&line).ok()?;
        if let Some(native) = record["sessionId"]
            .as_str()
            .or_else(|| record["session_id"].as_str())
        {
            if tail.native.as_deref().is_some_and(|seen| seen != native) {
                return None;
            }
            tail.native = Some(native.into());
        }
        match record["type"].as_str()? {
            "result" => {
                tail.end = End::Hard;
                tail.terminal = offset;
                tail.empty = false;
            }
            "system" => match record["subtype"].as_str() {
                Some("turn_duration") => {
                    tail.end = End::Hard;
                    tail.terminal = offset;
                    tail.empty = false;
                }
                Some("stop_hook_summary") => {
                    tail.end = End::Summary;
                    tail.terminal = offset;
                    tail.empty = false;
                }
                Some("init") => {
                    tail.end = End::None;
                }
                _ => {
                    tail.end = End::None;
                    tail.empty = false;
                }
            },
            "user" | "assistant" => {
                tail.empty = false;
                tail.activity = offset;
                tail.activity_start = record_start;
                tail.activity_at = timestamp(&record);
                let content = &record["message"]["content"];
                let text = content.as_str().map(str::to_owned).or_else(|| {
                    let blocks = content.as_array()?;
                    let mut text = String::new();
                    for block in blocks {
                        if block["type"] != "text" {
                            return None;
                        }
                        text.push_str(block["text"].as_str()?);
                    }
                    Some(text)
                });
                tail.end = if record["type"] == "user"
                    && text.as_deref().is_some_and(|text| {
                        matches!(
                            text,
                            "[Request interrupted by user]"
                                | "[Request interrupted by user for tool use]"
                        )
                    }) {
                    tail.terminal = offset;
                    End::Hard
                } else {
                    End::None
                };
                if record["type"] == "assistant" {
                    tail.assistant = text;
                }
            }
            "permission-mode" => return None,
            "progress" => {
                tail.empty = false;
                tail.activity = offset;
                tail.activity_start = record_start;
                tail.activity_at = timestamp(&record);
                tail.end = End::None;
            }
            "file-history-snapshot" | "queue-operation" | "last-prompt" => {}
            _ => {
                tail.end = End::None;
                tail.empty = false;
            }
        }
        line.clear();
    }
    if Stamp::read(path).as_ref() != Some(&tail.stamp) {
        return None;
    }
    Some(tail)
}

type Activity = (u64, u64, Option<chrono::DateTime<chrono::Utc>>);

/// A large assistant row may displace the input from the tail; inspect the turn's prefix too.
pub(super) fn activity_after(
    path: &Path,
    floor: u64,
    terminal: u64,
    native: &str,
    after: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<Activity> {
    use std::io::{Read, Take};
    let mut file = std::fs::File::open(path).ok()?;
    if floor > 0 {
        file.seek(SeekFrom::Start(floor - 1)).ok()?;
        let mut previous = [0];
        file.read_exact(&mut previous).ok()?;
        if previous != [b'\n'] {
            return None;
        }
    }
    file.seek(SeekFrom::Start(floor)).ok()?;
    let bounded: Take<_> = file.take(terminal.checked_sub(floor)?.min(4 * 1024 * 1024));
    let mut reader = BufReader::new(bounded);
    let mut line = Vec::new();
    let mut offset = floor;
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        let record_start = offset;
        offset += line.len() as u64;
        if !line.ends_with(b"\n") {
            return None;
        }
        let record: Value = serde_json::from_slice(&line).ok()?;
        if record["sessionId"]
            .as_str()
            .or_else(|| record["session_id"].as_str())
            .is_some_and(|seen| seen != native)
        {
            return None;
        }
        if matches!(
            record["type"].as_str(),
            Some("user" | "assistant" | "progress")
        ) && after.is_none_or(|at| timestamp(&record).is_some_and(|seen| seen > at))
        {
            return Some((record_start, offset, timestamp(&record)));
        }
        line.clear();
    }
    None
}

fn timestamp(record: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    record["timestamp"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.to_utc())
}
