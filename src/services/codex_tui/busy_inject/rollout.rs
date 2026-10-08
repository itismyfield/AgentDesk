//! Reads a Codex rollout's tail: which native turn is open and whether Enter would steer it, and
//! which turn recorded an injected nonce as user input.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// How far back the turn verdict reads; a turn opened before this window reads as Unknown.
const TAIL_WINDOW: u64 = 4 * 1024 * 1024;

/// The open native turn, as Enter in the composer would meet it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnVerdict {
    /// A model turn: Enter steers into it.
    KnownModelTurn(String),
    /// A shell, compact or review turn, or a sandbox other than full access: Enter queues or the
    /// turn is out of scope.
    NonSteerable(String),
    /// Not proven either way.
    Unknown,
    /// The last turn has ended, or none ever started.
    NotBusy,
}

#[derive(Default)]
struct Open {
    turn: String,
    root: String,
    context: bool,
    full_access: bool,
    user: bool,
    compaction: bool,
    closed: bool,
    unreadable: bool,
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The verdict for the latest `task_started` in the tail window; None when the file is unreadable.
pub(crate) fn read_turn(path: &Path) -> Option<TurnVerdict> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes).ok()?;
    let mut lines = bytes.split_inclusive(|byte| *byte == b'\n');
    if start > 0 {
        // The window may begin mid-record.
        lines.next();
    }
    let mut open: Option<Open> = None;
    let mut review: Option<String> = None;
    // A line without its newline may still be mid-write.
    for line in lines.filter(|line| line.ends_with(b"\n")) {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            if let Some(open) = open.as_mut() {
                open.unreadable = true;
            }
            continue;
        };
        apply(&record, &mut open, &mut review);
    }
    let Some(open) = open else {
        return Some(if start == 0 {
            TurnVerdict::NotBusy
        } else {
            TurnVerdict::Unknown
        });
    };
    let turn = open.turn.clone();
    Some(if open.unreadable {
        TurnVerdict::Unknown
    } else if open.closed {
        TurnVerdict::NotBusy
    } else if open.turn != open.root || review.is_some() || open.compaction {
        TurnVerdict::NonSteerable(turn)
    } else if !open.context || !open.full_access {
        TurnVerdict::NonSteerable(turn)
    } else if !open.user {
        TurnVerdict::Unknown
    } else {
        TurnVerdict::KnownModelTurn(turn)
    })
}

fn apply(record: &Value, open: &mut Option<Open>, review: &mut Option<String>) {
    let Some(payload) = record.get("payload") else {
        return;
    };
    let turn_id = crate::services::agent_protocol::codex_payload_turn_id(payload);
    match (text(record, "type"), text(payload, "type")) {
        (Some("event_msg"), Some("task_started")) => {
            let next = turn_id.map(|turn| Open {
                turn: turn.to_string(),
                root: text(payload, "root_turn_id").unwrap_or(turn).to_string(),
                ..Open::default()
            });
            *open = Some(next.unwrap_or(Open {
                unreadable: true,
                ..Open::default()
            }));
        }
        (Some("turn_context"), _) => {
            if let Some(open) = open.as_mut().filter(|open| turn_id == Some(&open.turn)) {
                open.context = true;
                let sandbox = payload.get("sandbox_policy");
                open.full_access =
                    sandbox.and_then(|policy| text(policy, "type")) == Some("danger-full-access");
            }
        }
        (Some("event_msg"), Some("item_completed")) => {
            let item = payload.get("item").and_then(|item| text(item, "type"));
            match item {
                Some("EnteredReviewMode") => *review = turn_id.map(str::to_string),
                Some("ExitedReviewMode") => *review = None,
                _ => {}
            }
            let Some(open) = open.as_mut().filter(|open| turn_id == Some(&open.turn)) else {
                return;
            };
            match item {
                Some("UserMessage") => open.user = true,
                Some("ContextCompaction") => open.compaction = true,
                _ => {}
            }
        }
        (Some("event_msg"), Some("task_complete" | "turn_aborted")) => {
            if review.is_some() && review.as_deref() == turn_id {
                *review = None;
            }
            // An unnamed end may be this turn's, so the turn reads as Unknown.
            if let Some(open) = open.as_mut() {
                match turn_id {
                    Some(turn) if turn == open.turn || turn == open.root => open.closed = true,
                    Some(_) => {}
                    None => open.unreadable = true,
                }
            }
        }
        _ => {}
    }
}

/// The turn a complete user record after `offset` carrying the nonce names, the first one found.
/// The outer Some is the sighting; the inner value is the turn id when the record names one.
pub(crate) fn submitted(path: &Path, offset: u64, nonce: &str) -> Option<Option<String>> {
    let mut file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() < offset {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let marker = format!(" · {nonce}]");
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    while reader
        .read_until(b'\n', &mut line)
        .is_ok_and(|read| read > 0)
    {
        if line.ends_with(b"\n")
            && let Ok(record) = serde_json::from_slice::<Value>(&line)
            && let Some((input, turn)) = user_input(&record)
            && input.contains(&marker)
        {
            return Some(turn);
        }
        line.clear();
    }
    None
}

/// The text and turn of a user message record; other records never count.
fn user_input(record: &Value) -> Option<(String, Option<String>)> {
    let payload = record.get("payload")?;
    let joined = |content: &Value| -> String {
        let blocks = content.as_array().into_iter().flatten();
        let texts = blocks.filter_map(|block| text(block, "text"));
        texts.collect::<Vec<_>>().join("\n")
    };
    match (text(record, "type")?, text(payload, "type")?) {
        ("response_item", "message") if text(payload, "role") == Some("user") => {
            let meta = payload.get("internal_chat_message_metadata_passthrough");
            let turn = meta.and_then(|meta| text(meta, "turn_id"));
            Some((joined(payload.get("content")?), turn.map(str::to_string)))
        }
        ("event_msg", "item_completed") => {
            let item = payload.get("item")?;
            if text(item, "type")? != "UserMessage" {
                return None;
            }
            let turn = text(payload, "turn_id").map(str::to_string);
            Some((joined(item.get("content")?), turn))
        }
        _ => None,
    }
}
