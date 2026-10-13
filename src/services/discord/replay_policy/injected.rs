//! A busy input keeps its own attempt and source proof beside the existing output owner.

use super::live::{TerminalEvidence, TerminalKind};
use crate::db::replay_disposition::write::{self, ExactAttempt};
use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::cluster::stream_relay::{GenerationSourceIdentity, SourceFileIdentity};
use crate::services::discord::SharedData;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Marker {
    file: [u64; 2],
    generation: i64,
    spawn: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Source {
    path: String,
    file: [u64; 2],
    marker: Marker,
    reset: u64,
    frontier: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Binding {
    channel: u64,
    provider: String,
    pane: String,
    owner_nonce: String,
    session: String,
    injection_nonce: String,
    source: Source,
}

struct Entry {
    binding: Binding,
    hash: String,
    attempt: Mutex<Option<ExactAttempt>>,
    pending: Mutex<Option<Candidate>>,
}

type Key = (u64, i64);
static INPUTS: LazyLock<dashmap::DashMap<Key, Arc<Entry>>> = LazyLock::new(dashmap::DashMap::new);
static OWNERS: LazyLock<dashmap::DashMap<(u64, String), ()>> = LazyLock::new(dashmap::DashMap::new);

/// Cloneable source data; the execution attempt remains move-only in its live handle.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    channel: u64,
    receipt: i64,
    nonce: String,
    end: u64,
    kind: TerminalKind,
}

fn marker(pane: &str) -> Option<Marker> {
    let witness = crate::services::discord::delivery_lease_cell::source_epoch_observer::read_source_epoch_witness(pane);
    let GenerationSourceIdentity::Unix { mtime_ns, dev, ino } = witness.generation? else {
        return None;
    };
    Some(Marker {
        file: [dev, ino],
        generation: mtime_ns,
        spawn: witness.spawn_nonce_hash,
    })
}

fn pinned(shared: &SharedData, binding: &Binding, file: &std::fs::File) -> bool {
    let runtime = match binding.provider.as_str() {
        "claude" => RuntimeHandoffKind::ClaudeTui,
        "codex" => RuntimeHandoffKind::CodexTui,
        _ => return false,
    };
    let current = crate::services::tui_prompt_dedupe::try_peek_tmux_runtime_binding(&binding.pane);
    let Some(current) = current.ok().flatten() else {
        return false;
    };
    let identity = SourceFileIdentity::Unix {
        dev: binding.source.file[0],
        ino: binding.source.file[1],
    };
    shared.provider.as_str() == binding.provider
        && !binding.session.trim().is_empty()
        && !binding.owner_nonce.trim().is_empty()
        && !binding.injection_nonce.trim().is_empty()
        && current.runtime_kind == runtime
        && (current.output_path == binding.source.path
            || current.relay_output_path.as_deref() == Some(binding.source.path.as_str()))
        && current.session_id.as_deref() == Some(binding.session.as_str())
        && SourceFileIdentity::from_open_file(file) == identity
        && marker(&binding.pane).as_ref() == Some(&binding.source.marker)
        && shared
            .relay_frontier_token(serenity::all::ChannelId::new(binding.channel))
            .reset_incarnation
            == binding.source.reset
}

fn input_hash(text: &str) -> String {
    hex::encode(sha2::Sha256::digest(text.as_bytes()))
}

fn user_text(value: &serde_json::Value, provider: &str) -> Option<String> {
    let message = match (provider, value["type"].as_str()) {
        ("claude", Some("user")) if value["isMeta"] != true => &value["message"],
        ("codex", Some("response_item")) if value["payload"]["type"] == "message" => {
            &value["payload"]
        }
        ("codex", Some("event_msg")) if value["payload"]["type"] == "user_message" => {
            return value["payload"]["message"].as_str().map(str::to_owned);
        }
        _ => return None,
    };
    if message["role"] != "user" {
        return None;
    }
    if let Some(text) = message["content"].as_str() {
        return Some(text.to_owned());
    }
    message["content"]
        .as_array()?
        .iter()
        .map(|block| {
            matches!(block["type"].as_str(), Some("text" | "input_text"))
                .then(|| block["text"].as_str())
                .flatten()
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.concat())
}

fn terminal(value: &serde_json::Value, provider: &str) -> Option<TerminalKind> {
    match (provider, value["type"].as_str()) {
        ("claude", Some("result")) => Some(if value["is_error"] == true {
            TerminalKind::Error
        } else {
            TerminalKind::Done
        }),
        ("claude", Some("system")) if value["subtype"] == "stop_hook_summary" => {
            Some(TerminalKind::Done)
        }
        ("codex", Some("event_msg")) => match value["payload"]["type"].as_str() {
            Some("task_complete") => Some(TerminalKind::Done),
            Some("task_aborted") => Some(TerminalKind::Error),
            _ => None,
        },
        _ => None,
    }
}

fn included(shared: &SharedData, entry: &Entry, end: u64) -> Option<TerminalKind> {
    let binding = &entry.binding;
    let mut file = std::fs::File::open(&binding.source.path).ok()?;
    if !pinned(shared, binding, &file)
        || file.metadata().ok()?.len() < end
        || binding.source.frontier >= end
    {
        return None;
    }
    let mut aligned = true;
    if binding.source.frontier > 0 {
        file.seek(SeekFrom::Start(binding.source.frontier - 1))
            .ok()?;
        let mut previous = [0];
        file.read_exact(&mut previous).ok()?;
        aligned = previous[0] == b'\n';
    }
    file.seek(SeekFrom::Start(binding.source.frontier)).ok()?;
    let mut reader = std::io::BufReader::new(file.take(end - binding.source.frontier));
    let mut offset = binding.source.frontier;
    let mut matched = false;
    loop {
        let mut line = Vec::new();
        let bytes = reader.read_until(b'\n', &mut line).ok()?;
        if bytes == 0 || line.last() != Some(&b'\n') {
            return None;
        }
        offset += bytes as u64;
        if !aligned {
            aligned = true;
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
        let session = value["sessionId"]
            .as_str()
            .or_else(|| value["session_id"].as_str());
        if session.is_some_and(|session| session != binding.session) {
            return None;
        }
        if let Some(text) = user_text(&value, &binding.provider) {
            matched |= text.contains(&binding.injection_nonce) && input_hash(&text) == entry.hash;
        }
        if let Some(kind) = terminal(&value, &binding.provider) {
            if offset == end && matched {
                let file = reader.get_ref().get_ref();
                return pinned(shared, binding, file).then_some(kind);
            }
            matched = false;
        }
    }
}

pub(crate) fn protects(channel: u64, owner_nonce: Option<&str>) -> bool {
    owner_nonce.is_some_and(|nonce| OWNERS.contains_key(&(channel, nonce.into())))
        || INPUTS.iter().any(|entry| {
            entry.binding.channel == channel
                && Some(entry.binding.owner_nonce.as_str()) == owner_nonce
        })
}

fn candidates(
    shared: &SharedData,
    channel: u64,
    pane: &str,
    path: &str,
    end: u64,
) -> Vec<Candidate> {
    INPUTS
        .iter()
        .filter_map(|slot| {
            let entry = slot.value();
            if entry.binding.channel != channel
                || entry.binding.pane != pane
                || entry.binding.source.path != path
            {
                return None;
            }
            let attempt = entry.attempt.lock().ok()?;
            let attempt = attempt.as_ref()?;
            let kind = included(shared, entry, end)?;
            Some(Candidate {
                channel,
                receipt: attempt.receipt_id(),
                nonce: attempt.nonce().into(),
                end,
                kind,
            })
        })
        .collect()
}

/// Collects source candidates only; the existing consumer must grant the terminal boundary.
pub(crate) fn collect(
    shared: &SharedData,
    channel: u64,
    pane: &str,
    path: &str,
    end: u64,
) -> Vec<Candidate> {
    let candidates = candidates(shared, channel, pane, path, end);
    for candidate in &candidates {
        if let Some(entry) = INPUTS.get(&(channel, candidate.receipt)) {
            *entry
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(candidate.clone());
        }
    }
    candidates
}

pub(crate) fn pending(channel: u64, pane: &str, path: &str, end: u64) -> Vec<Candidate> {
    INPUTS
        .iter()
        .filter_map(|entry| {
            if entry.binding.channel != channel
                || entry.binding.pane != pane
                || entry.binding.source.path != path
            {
                return None;
            }
            entry
                .pending
                .lock()
                .ok()?
                .as_ref()
                .filter(|candidate| candidate.end == end)
                .cloned()
        })
        .collect()
}

pub(crate) fn native(shared: &SharedData, channel: u64, message: &StreamMessage) -> Vec<Candidate> {
    let (pane, path, nonce, session, end) = match message {
        StreamMessage::ClaudeTuiTerminalDone {
            tmux_session_name,
            transcript_path,
            turn_nonce,
            session_id,
            complete_record_end,
            ..
        } => (
            tmux_session_name,
            transcript_path,
            turn_nonce,
            session_id,
            *complete_record_end,
        ),
        StreamMessage::CodexTuiTerminalDone {
            tmux_session_name,
            rollout_path,
            turn_nonce,
            session_id,
            complete_record_end,
            ..
        } => (
            tmux_session_name,
            rollout_path,
            turn_nonce,
            session_id,
            *complete_record_end,
        ),
        _ => return Vec::new(),
    };
    candidates(shared, channel, pane, path, end)
        .into_iter()
        .filter(|candidate| {
            INPUTS
                .get(&(channel, candidate.receipt))
                .is_some_and(|entry| {
                    entry.binding.owner_nonce == *nonce
                        && session.as_deref() == Some(entry.binding.session.as_str())
                })
        })
        .collect()
}

/// Takes each exact attempt once; a missing commit ACK retains the owner's replay protection.
pub(crate) async fn consume(shared: &SharedData, channel: u64, candidates: Vec<Candidate>) {
    #[cfg(test)]
    test_support::note(channel, "consume_enter");
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    for candidate in candidates {
        let Some(entry) = INPUTS
            .get(&(channel, candidate.receipt))
            .map(|entry| entry.value().clone())
        else {
            continue;
        };
        if candidate.channel != channel
            || included(shared, &entry, candidate.end) != Some(candidate.kind)
        {
            continue;
        }
        #[cfg(test)]
        test_support::prepare(channel);
        let attempt = entry
            .attempt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let Some(attempt) = attempt else {
            continue;
        };
        let evidence =
            TerminalEvidence::from_injected(candidate.receipt, &candidate.nonce, candidate.kind);
        #[cfg(test)]
        test_support::note(channel, "classify_attempt");
        let result = write::classify_normal(pool, &attempt, &evidence).await;
        #[cfg(test)]
        let result = test_support::ack(channel, result);
        #[cfg(test)]
        test_support::note(channel, "classification_returned");
        if result.is_ok() {
            *entry
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            #[cfg(test)]
            test_support::note(channel, "normal_ack");
        }
    }
}

#[cfg(test)]
#[path = "injected/test_support_tests.rs"]
pub(crate) mod test_support;

#[cfg(test)]
pub(crate) fn attach_started(
    attempt: ExactAttempt,
    value: serde_json::Value,
    hash: String,
) -> Result<(), serde_json::Error> {
    let binding: Binding = serde_json::from_value(value)?;
    INPUTS.insert(
        (binding.channel, attempt.receipt_id()),
        Arc::new(Entry {
            binding,
            hash,
            attempt: Mutex::new(Some(attempt)),
            pending: Mutex::new(None),
        }),
    );
    Ok(())
}
