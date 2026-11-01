//! Whether a Codex rollout still sits before any output: its verified `session_meta`, records the
//! classifier ignores, one turn opener, and one user prompt with that prompt's own mirrors.

use std::io::Read;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::services::codex_tui::rollout_index::strict_parent_session;
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{MAX_READ_BYTES, ShadowProvider, SourceId};
use crate::services::tui_prompt_dedupe::is_codex_context_record;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::tui_o::writer::late_adopt) enum Verdict {
    /// Every byte is a complete record O may read from offset 0 with no Legacy body before it.
    Eligible(Prefix),
    /// The file ends inside a record or could not be read; the same rollout is judged again.
    Wait(String),
    /// The rollout holds output, another turn or input, or a record that cannot be judged.
    Refuse(String),
}

/// The judged bytes, so a later judgement can tell whether anything changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::tui_o::writer::late_adopt) struct Prefix {
    pub(in crate::services::tui_o::writer::late_adopt) len: u64,
    pub(in crate::services::tui_o::writer::late_adopt) sha256: String,
    /// End of the first user prompt's record, once one is written.
    pub(in crate::services::tui_o::writer::late_adopt) prompt_end: Option<u64>,
}

pub(super) fn judge(source: &SourceId) -> Verdict {
    judged(source).unwrap_or_else(|verdict| verdict)
}

fn judged(source: &SourceId) -> Result<Verdict, Verdict> {
    let bytes = read(source)?;
    let complete = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |at| at + 1);
    let mut turn = Turn::default();
    let mut start = 0;
    for line in bytes[..complete].split_inclusive(|b| *b == b'\n') {
        let end = start + line.len() as u64;
        turn.read(line, (start, end)).map_err(Verdict::Refuse)?;
        start = end;
    }
    if !turn.meta {
        return Err(Verdict::Wait("no session_meta yet".into()));
    }
    if complete != bytes.len() {
        return Err(Verdict::Wait("the rollout ends inside a record".into()));
    }
    // A failed read waits; a header that does not prove the bound session refuses the rollout.
    strict_parent_session(&source.path, &source.session_id).map_err(|error| {
        match error.kind() {
            std::io::ErrorKind::Other => Verdict::Refuse(error.to_string()),
            _ => Verdict::Wait(error.to_string()),
        }
    })?;
    Ok(Verdict::Eligible(Prefix {
        len: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(&bytes)),
        prompt_end: turn.prompt.map(|prompt| prompt.end),
    }))
}

pub(in crate::services::tui_o::writer::late_adopt) fn read(
    source: &SourceId,
) -> Result<Vec<u8>, Verdict> {
    let shown = source.path.display();
    let wait = |error: std::io::Error| Verdict::Wait(format!("source {shown}: {error}"));
    let file = std::fs::File::open(&source.path).map_err(wait)?;
    let meta = file.metadata().map_err(wait)?;
    if file_identity(&meta).1 != source.ino {
        return Err(Verdict::Refuse(format!("source {shown} was replaced")));
    }
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(wait)?;
    match bytes.len() as u64 {
        len if len > MAX_READ_BYTES => Err(Verdict::Refuse(format!(
            "source {shown} holds more than {MAX_READ_BYTES} bytes"
        ))),
        _ => Ok(bytes),
    }
}

#[derive(Default)]
struct Turn {
    meta: bool,
    /// The turn opener's native id, once one was read.
    opener: Option<Option<String>>,
    prompt: Option<Prompt>,
    /// Which mirror kinds already repeated the prompt.
    mirrored: [bool; 2],
}

struct Prompt {
    text: String,
    end: u64,
}

impl Turn {
    /// Reads one complete line; an error names why the rollout is refused.
    fn read(&mut self, line: &[u8], (at, end): (u64, u64)) -> Result<(), String> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value = serde_json::from_slice::<Value>(line).ok();
        let value = value
            .filter(Value::is_object)
            .ok_or_else(|| format!("malformed record at {at}"))?;
        let session = value["type"] == "session_meta";
        match (session, std::mem::replace(&mut self.meta, true)) {
            (true, false) => return Ok(()),
            (true, true) => return Err(format!("a second session_meta at {at}")),
            (false, false) => return Err(format!("record at {at} precedes the session_meta")),
            (false, true) => {}
        }
        if let Some(mirror) = Mirror::of(&value) {
            return self.mirror(mirror, at);
        }
        for fact in classify(ShadowProvider::Codex, &value) {
            match fact {
                RecordFact::TurnStart(id) if self.opener.is_none() => self.opener = Some(id),
                // Codex's context items before the prompt are not it; Legacy never relays them.
                RecordFact::Prompt(..)
                    if self.prompt.is_none() && is_codex_context_record(&value["payload"]) => {}
                RecordFact::Prompt(_, text) if self.prompt.is_none() => {
                    self.prompt = Some(Prompt { text, end });
                }
                RecordFact::TurnStart(_) => return Err(format!("a second turn opener at {at}")),
                RecordFact::Prompt(..) => return Err(format!("a second user prompt at {at}")),
                other => return Err(format!("{} at {at}", output(&other))),
            }
        }
        Ok(())
    }

    /// A mirror must follow the prompt it repeats, once per kind, with its text and its turn.
    fn mirror(&mut self, mirror: Mirror, at: u64) -> Result<(), String> {
        let Some(prompt) = &self.prompt else {
            return Err(format!("user input at {at} before any prompt"));
        };
        if std::mem::replace(&mut self.mirrored[mirror.kind], true) {
            return Err(format!("a second user input at {at}"));
        }
        if mirror.text.as_deref() != Some(prompt.text.as_str()) {
            return Err(format!("user input at {at} is not the first prompt"));
        }
        let opener = self.opener.as_ref().and_then(Option::as_deref);
        match (mirror.turn.as_deref(), opener) {
            (Some(turn), Some(open)) if turn == open => Ok(()),
            (None, _) if !mirror.named => Ok(()),
            _ => Err(format!("user input at {at} has no matching turn")),
        }
    }
}

/// A record that repeats a user prompt; the classifier yields nothing for it.
struct Mirror {
    kind: usize,
    text: Option<String>,
    turn: Option<String>,
    /// Whether this kind always names its turn.
    named: bool,
}

impl Mirror {
    fn of(value: &Value) -> Option<Self> {
        let payload = &value["payload"];
        let turn = payload["turn_id"].as_str().map(str::to_owned);
        match (value["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("user_message")) => Some(Self {
                kind: 0,
                text: payload["message"].as_str().map(str::to_owned),
                turn,
                named: false,
            }),
            (Some("event_msg"), Some("item_completed"))
                if payload["item"]["type"] == "UserMessage" =>
            {
                let parts = payload["item"]["content"].as_array();
                let texts = parts.map(|parts| {
                    let texts = parts.iter().filter_map(|part| part["text"].as_str());
                    texts.collect::<Vec<_>>().join("\n")
                });
                Some(Self {
                    kind: 1,
                    text: texts,
                    turn,
                    named: true,
                })
            }
            _ => None,
        }
    }
}

fn output(fact: &RecordFact) -> &'static str {
    match fact {
        RecordFact::Unit(..) => "an output unit",
        RecordFact::Announced(..) => "an announced output",
        RecordFact::Blocked(_) => "an output without identity",
        RecordFact::Idle(_) => "a turn end",
        _ => "an assistant row",
    }
}

#[cfg(test)]
#[path = "prefix_tests.rs"]
mod tests;
