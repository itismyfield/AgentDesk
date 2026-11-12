//! Complete root records only; the screen and file age never establish activity.
use super::source::{Source, Stamp};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
};

const READ_LIMIT: u64 = 4 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Turn {
    pub floor: u64,
    pub prompt: Option<String>,
    pub at: DateTime<Utc>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Invocation {
    pub background: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub stamp: Stamp,
    pub turn: Option<Turn>,
    pub uses: HashMap<String, Invocation>,
    pub results: HashSet<String>,
    pub terminal: u64,
    pub progress: Option<(u64, DateTime<Utc>)>,
    pub through: u64,
    pub partial: Vec<u8>,
}
impl Frame {
    pub(super) fn caught_up(&self) -> bool {
        self.through == self.stamp.len && self.partial.is_empty()
    }
}
fn time(row: &Value) -> Option<DateTime<Utc>> {
    row["timestamp"].as_str()?.parse().ok()
}
pub(super) fn read(source: &Source, previous: Option<Frame>) -> Option<Frame> {
    let stamp = source.stamp()?;
    let mut file = File::open(&source.path).ok()?;
    let opened = crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file);
    #[cfg(unix)]
    if opened
        != (crate::services::cluster::stream_relay::SourceFileIdentity::Unix {
            dev: source.file.0,
            ino: source.file.1,
        })
    {
        return None;
    }
    #[cfg(not(unix))]
    let _ = opened;
    let mut frame = previous.unwrap_or_else(|| Frame {
        stamp: stamp.clone(),
        turn: None,
        uses: HashMap::new(),
        results: HashSet::new(),
        terminal: 0,
        progress: None,
        through: 0,
        partial: Vec::new(),
    });
    if stamp.len < frame.stamp.len
        || (stamp.len == frame.stamp.len && stamp.modified != frame.stamp.modified)
    {
        return None;
    }
    frame.stamp = stamp;
    file.seek(SeekFrom::Start(frame.through)).ok()?;
    let mut reader = BufReader::new(file.take(READ_LIMIT));
    let mut line = std::mem::take(&mut frame.partial);
    loop {
        let floor = frame.through - line.len() as u64;
        let read = reader.read_until(b'\n', &mut line).ok()?;
        frame.through += read as u64;
        if line.len() > crate::services::tui_o::shadow::capture::MAX_PARTIAL_BYTES {
            return None;
        }
        if read == 0 || !line.ends_with(b"\n") {
            frame.partial = line;
            break;
        }
        let offset = frame.through;
        let row: Value = serde_json::from_slice(&line).ok()?;
        line.clear();
        if row["sessionId"]
            .as_str()
            .or_else(|| row["session_id"].as_str())
            .is_some_and(|id| id != source.native)
        {
            return None;
        }
        if row["isSidechain"] == true || row["agentId"].is_string() || row["agent_id"].is_string() {
            continue;
        }
        let content = &row["message"]["content"];
        let blocks = content.as_array();
        let invoking = blocks
            .into_iter()
            .flatten()
            .any(|block| block["type"] == "tool_use");
        let results: Vec<_> = blocks
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_result")
            .collect();
        let text = content.as_str().or_else(|| {
            let blocks = content.as_array()?;
            (blocks.len() == 1 && blocks[0]["type"] == "text")
                .then(|| blocks[0]["text"].as_str())
                .flatten()
        });
        let interrupted = text.is_some_and(|s| {
            matches!(
                s,
                "[Request interrupted by user]" | "[Request interrupted by user for tool use]"
            )
        });
        let terminal = row["type"] == "result"
            || (row["type"] == "system" && row["subtype"] == "turn_duration")
            || interrupted;
        if terminal {
            frame.terminal = offset;
            frame.uses.clear();
            frame.progress = None;
            continue;
        }
        if row["type"] == "user" && results.is_empty() {
            frame.turn = time(&row).map(|at| Turn {
                floor,
                prompt: row["uuid"].as_str().map(str::to_owned),
                at,
            });
            frame.uses.clear();
            frame.results.clear();
            frame.terminal = 0;
            frame.progress = None;
        }
        for block in results {
            let id = block["tool_use_id"].as_str()?;
            if frame.results.len() >= 4096 && !frame.results.contains(id) {
                return None;
            }
            frame.results.insert(id.into());
        }
        if row["type"] == "assistant" {
            frame.progress = None;
            if frame.terminal > 0 {
                if let Some(turn) = &mut frame.turn {
                    turn.floor = frame.terminal;
                    if let Some(at) = time(&row) {
                        turn.at = at;
                    } else {
                        frame.turn = None;
                    }
                }
                frame.terminal = 0;
                frame.results.clear();
            }
            for block in blocks
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
            {
                let id = block["id"].as_str()?;
                if frame.uses.len() >= 4096 && !frame.uses.contains_key(id) {
                    return None;
                }
                frame.uses.insert(
                    id.into(),
                    Invocation {
                        // Async/background classification is refined by the background observer.
                        background: matches!(block["name"].as_str(), Some("Task" | "Agent"))
                            || block["input"]["run_in_background"] == true,
                    },
                );
            }
            if row["message"]["stop_reason"].is_null() && !invoking {
                frame.progress = time(&row).map(|at| (offset, at));
            }
        }
    }
    (source.stamp().as_ref() == Some(&frame.stamp)).then_some(frame)
}

#[cfg(all(test, unix))]
#[path = "transcript_tests.rs"]
mod tests;
