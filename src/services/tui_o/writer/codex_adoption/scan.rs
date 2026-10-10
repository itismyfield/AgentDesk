//! A strict read of one Codex rollout: every byte to the end read once and hashed, every record
//! classified, and the end judged closed only by the last turn's own completion.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Closed, Parse, Suffix};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};

/// One record as adoption reads it; only `Quiet` may sit past Legacy's checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Class {
    Quiet,
    Prompt,
    Start(Option<String>),
    /// A turn's completion or named abort; it may carry a fallback body.
    Close(Option<String>),
    Announce(String),
    /// A body, a tool call or its output; a tool call opens `call_id` until its output.
    Output {
        seals: Option<String>,
        opens: Option<String>,
        closes: Option<String>,
    },
    /// A tool call with no `call_id`: no output can be shown to answer it.
    UnkeyedCall,
    Unrecognized,
}

/// A rollout as one open read it.
#[derive(Clone, Debug)]
pub struct Scanned {
    pub file: (u64, u64),
    pub len: u64,
    pub hash: String,
    /// The `session_meta` id the file names first.
    pub session: Option<String>,
    pub prefix: Parse,
    pub closed: Closed,
    /// Each complete record's end with its class, in file order.
    records: Vec<(u64, Class)>,
    /// The file ends inside a record.
    torn: bool,
}

/// Reads `path` to its end, refusing more than `budget` bytes.
pub fn scan(path: &Path, budget: u64) -> Result<Scanned, String> {
    let shown = path.display();
    let io = |error: std::io::Error| format!("source {shown}: {error}");
    let file = File::open(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    let len = meta.len();
    if len > budget {
        return Err(format!(
            "source {shown} holds {len} bytes past the read budget"
        ));
    }
    let (mut hasher, mut reader) = (Sha256::new(), BufReader::new(file.take(len)));
    let (mut at, mut line, mut records) = (0u64, Vec::new(), Vec::new());
    let (mut session, mut malformed, mut torn) = (None, false, false);
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&line);
        at += read as u64;
        if line.last() != Some(&b'\n') {
            torn = true;
            break;
        }
        let record = std::str::from_utf8(&line).ok().and_then(|text| {
            serde_json::from_str::<Value>(text)
                .ok()
                .filter(Value::is_object)
        });
        let Some(record) = record else {
            malformed = true;
            records.push((at, Class::Unrecognized));
            continue;
        };
        if session.is_none() && record["type"] == "session_meta" {
            session = record["payload"]["id"].as_str().map(str::to_owned);
        }
        records.push((at, class(&record)));
    }
    if at != len {
        return Err(format!("source {shown} ended at {at} while read to {len}"));
    }
    let closed = closed(&records);
    Ok(Scanned {
        file: file_identity(&meta),
        len,
        hash: hex::encode(hasher.finalize()),
        session,
        // Strict only when every byte read is a complete JSONL record.
        prefix: if malformed || torn {
            Parse::Malformed
        } else {
            Parse::Strict
        },
        closed,
        records,
        torn,
    })
}

impl Scanned {
    /// The records in `[from, len)`: quiet only when `from` ends a record and nothing past it
    /// posts, prompts, starts, closes or is unknown.
    pub fn suffix(&self, from: u64) -> Suffix {
        let on_record = from == 0 || self.records.iter().any(|(end, _)| *end == from);
        if self.torn || from > self.len || !on_record {
            return Suffix::Partial;
        }
        let mut start = 0;
        for (end, class) in &self.records {
            let past = start >= from;
            start = *end;
            match class {
                _ if !past => {}
                Class::Quiet => {}
                Class::Prompt => return Suffix::Prompt,
                Class::Start(_) => return Suffix::Start,
                Class::Unrecognized => return Suffix::Unrecognized,
                Class::Close(_)
                | Class::Announce(_)
                | Class::Output { .. }
                | Class::UnkeyedCall => {
                    return Suffix::Output;
                }
            }
        }
        Suffix::Quiet
    }
}

/// Session and turn metadata and token counts; everything else may post or start a turn.
fn quiet(record: &Value) -> bool {
    match record["type"].as_str() {
        Some("session_meta" | "turn_context") => true,
        Some("event_msg") => record["payload"]["type"] == "token_count",
        _ => false,
    }
}

fn class(record: &Value) -> Class {
    let item = record["payload"]["type"].as_str().unwrap_or("");
    let call = record["payload"]["call_id"]
        .as_str()
        .filter(|id| !id.is_empty());
    let response = record["type"] == "response_item";
    let opens = call
        .filter(|_| response && item.ends_with("_call"))
        .map(str::to_owned);
    let closes = call
        .filter(|_| response && item.ends_with("_output"))
        .map(str::to_owned);
    if response && item.ends_with("_call") && call.is_none() {
        return Class::UnkeyedCall;
    }
    let output = |seals: Option<&String>| Class::Output {
        seals: seals.cloned(),
        opens: opens.clone(),
        closes: closes.clone(),
    };
    match classify(ShadowProvider::Codex, record).as_slice() {
        [] if quiet(record) => Class::Quiet,
        [RecordFact::Prompt(..)] => Class::Prompt,
        [RecordFact::TurnStart(id)] => Class::Start(id.clone()),
        [RecordFact::Idle(id)] => Class::Close(id.clone()),
        [RecordFact::Announced(id, _)] => Class::Announce(id.clone()),
        [RecordFact::Unit(id, ..)] => output(Some(id)),
        [RecordFact::Blocked(_) | RecordFact::Assistant] => output(None),
        _ => Class::Unrecognized,
    }
}

/// `Own` when the last turn closed by its own named completion with no announcement unsealed, no
/// tool call unanswered or unkeyed, and nothing but quiet records after it; no turn is `Own` too.
fn closed(records: &[(u64, Class)]) -> Closed {
    let (mut turn, mut idle, mut unnamed) = (None::<Option<String>>, true, false);
    // A call with no key can never be answered; announcements and calls stay open across turns.
    let mut unkeyed = false;
    let (mut announced, mut calls) = (BTreeSet::new(), BTreeSet::new());
    for (_, class) in records {
        match class {
            Class::Quiet => continue,
            Class::Start(id) => {
                (turn, unnamed) = (Some(id.clone()), false);
            }
            Class::Close(id) => match &turn {
                Some(Some(open)) if id.as_deref() == Some(open.as_str()) => {
                    idle = announced.is_empty() && calls.is_empty();
                    turn = None;
                    continue;
                }
                Some(None) if id.is_none() => unnamed = true,
                // Another turn's completion, or one with no turn open, leaves the end open.
                _ => {}
            },
            Class::Announce(id) => {
                announced.insert(id.clone());
            }
            Class::Output {
                seals,
                opens,
                closes,
            } => {
                if let Some(id) = seals {
                    announced.remove(id);
                }
                calls.extend(opens.clone());
                if let Some(id) = closes {
                    calls.remove(id);
                }
            }
            Class::UnkeyedCall => unkeyed = true,
            Class::Prompt | Class::Unrecognized => {}
        }
        idle = false;
    }
    match turn {
        _ if unkeyed => Closed::Open,
        None if idle => Closed::Own,
        Some(None) if unnamed => Closed::Unknown,
        _ => Closed::Open,
    }
}
