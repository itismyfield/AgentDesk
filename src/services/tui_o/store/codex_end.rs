//! Confirmed Codex execution ends. An End alone never proves a successor or retires a reader.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::durable;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

pub(crate) const DIR_NAME: &str = "o_codex_ends";
const MAX_RECORDS: usize = 16;
const MAX_BYTES: u64 = 16 << 10;
const SCHEMA: u32 = 1;
static WRITE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndBasis {
    ExplicitClear,
    DirectReplacement,
}

/// The validated immutable context values; unrelated future launch fields never enter this format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextSnapshot {
    pub schema: u32,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub execution_nonce: String,
    pub tmux_session: String,
    pub channel_id: Option<u64>,
    pub owner_runtime_root: String,
    pub host: Option<String>,
    pub expected_native_session_id: Option<String>,
    pub launch_mode: String,
    pub provider_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_policy: Option<String>,
}

impl From<&BindingContext> for ContextSnapshot {
    fn from(context: &BindingContext) -> Self {
        let BindingContext {
            schema,
            provider,
            created_at,
            execution_nonce,
            tmux_session,
            channel_id,
            owner_runtime_root,
            host,
            expected_native_session_id,
            launch_mode,
            provider_root,
            first_prompt_digest,
            source_policy,
        } = context;
        Self {
            schema: *schema,
            provider: provider.clone(),
            created_at: *created_at,
            execution_nonce: execution_nonce.clone(),
            tmux_session: tmux_session.clone(),
            channel_id: *channel_id,
            owner_runtime_root: owner_runtime_root.clone(),
            host: host.clone(),
            expected_native_session_id: expected_native_session_id.clone(),
            launch_mode: launch_mode.clone(),
            provider_root: provider_root.clone(),
            first_prompt_digest: first_prompt_digest.clone(),
            source_policy: source_policy.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SuccessorSeal {
    pub execution_nonce: String,
    pub binding_seq: u64,
    pub source: SourceId,
    pub context: ContextSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexEnd {
    pub end_id: String,
    pub channel: u64,
    pub provider: String,
    pub tmux_session: String,
    pub execution_nonce: String,
    pub host: String,
    pub owner_runtime_root: String,
    pub provider_root: PathBuf,
    pub source: SourceId,
    pub binding_seq: u64,
    pub captured_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub basis: EndBasis,
    pub context: ContextSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor: Option<SuccessorSeal>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Records {
    schema: u32,
    records: Vec<CodexEnd>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Appended {
    Durable,
    PublishedUnsynced(String),
}

fn path(root: &Path, channel: u64) -> PathBuf {
    root.join(DIR_NAME).join(format!("{channel}.json"))
}

fn valid(record: &CodexEnd, channel: u64) -> bool {
    let context = &record.context;
    let whole = [
        &record.end_id,
        &record.tmux_session,
        &record.host,
        &record.owner_runtime_root,
    ]
    .iter()
    .all(|value| !value.trim().is_empty());
    whole
        && channel != 0
        && record.channel == channel
        && record.binding_seq != 0
        && record.provider == "codex"
        && record.execution_nonce.len() == 32
        && record
            .execution_nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && !record.source.session_id.trim().is_empty()
        && record.provider_root.is_absolute()
        && record.source.path.is_absolute()
        && record.source.path.starts_with(&record.provider_root)
        && context.schema == 1
        && context.provider == record.provider
        && context.channel_id == Some(channel)
        && context.tmux_session == record.tmux_session
        && context.execution_nonce == record.execution_nonce
        && context.host.as_deref() == Some(record.host.as_str())
        && context.owner_runtime_root == record.owner_runtime_root
        && context.provider_root.is_some()
        && context
            .expected_native_session_id
            .as_ref()
            .is_none_or(|id| id == &record.source.session_id)
        && record
            .successor
            .as_ref()
            .is_none_or(|successor| valid_seal(record, successor))
}

fn valid_seal(record: &CodexEnd, seal: &SuccessorSeal) -> bool {
    let context = &seal.context;
    seal.binding_seq > record.binding_seq
        && seal.execution_nonce != record.execution_nonce
        && seal.execution_nonce.len() == 32
        && seal
            .execution_nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && !seal.source.session_id.trim().is_empty()
        && seal.source.path.is_absolute()
        && seal.source.path.starts_with(&record.provider_root)
        && context.schema == 1
        && context.provider == record.provider
        && context.channel_id == Some(record.channel)
        && context.tmux_session == record.tmux_session
        && context.execution_nonce == seal.execution_nonce
        && context.host.as_deref() == Some(record.host.as_str())
        && context.owner_runtime_root == record.owner_runtime_root
        && context
            .provider_root
            .as_ref()
            .is_some_and(|root| root.is_absolute())
        && context.launch_mode == "fresh"
        && context.expected_native_session_id.is_none()
        && context.source_policy.as_deref() == Some("shadow")
}

fn known_fields(read: &serde_json::Value, model: &serde_json::Value) -> bool {
    use serde_json::Value::{Array, Object};
    match (read, model) {
        (Object(read), Object(model)) => read.iter().all(|(key, value)| {
            model
                .get(key)
                .is_some_and(|expected| known_fields(value, expected))
        }),
        (Array(read), Array(model)) => read.iter().zip(model).all(|(a, b)| known_fields(a, b)),
        _ => true,
    }
}

/// Missing is empty; malformed, foreign or oversized files are unavailable and remain untouched.
pub(crate) fn read(root: &Path, channel: u64) -> Result<Vec<CodexEnd>, String> {
    let file = match fs::File::open(path(root, channel)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("Codex ends exceed their size limit".into());
    }
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let records: Records =
        serde_json::from_value(raw.clone()).map_err(|error| error.to_string())?;
    if records.schema != SCHEMA
        || !known_fields(
            &raw,
            &serde_json::to_value(&records).map_err(|error| error.to_string())?,
        )
    {
        return Err("Codex ends schema or fields mismatch".into());
    }
    if records.records.len() > MAX_RECORDS
        || records.records.iter().enumerate().any(|(index, record)| {
            !valid(record, channel)
                || records.records[..index]
                    .iter()
                    .any(|old| old.end_id == record.end_id)
        })
    {
        return Err("Codex ends identity mismatch".into());
    }
    Ok(records.records)
}

fn encode(records: &[CodexEnd]) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&Records {
        schema: SCHEMA,
        records: records.to_vec(),
    })
    .map_err(|error| error.to_string())
}

/// Appends under one read/validate/replace lock. A retry cannot replace the identity of an End.
pub(crate) fn append(root: &Path, record: &CodexEnd) -> Result<Appended, String> {
    if !valid(record, record.channel)
        || record.successor.is_some()
        || encode(std::slice::from_ref(record))?.len() as u64 > MAX_BYTES
    {
        return Err("Codex end identity or size mismatch".into());
    }
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut records = read(root, record.channel)?;
    if let Some(existing) = records
        .iter()
        .find(|existing| existing.end_id == record.end_id)
    {
        let mut identity = existing.clone();
        identity.successor = None;
        if identity != *record {
            return Err("Codex end id already names another record".into());
        }
    } else {
        records.push(record.clone());
    }
    let excess = records.len().saturating_sub(MAX_RECORDS);
    records.drain(..excess);
    let bytes = loop {
        let bytes = encode(&records)?;
        if bytes.len() as u64 <= MAX_BYTES {
            break bytes;
        }
        records.remove(0);
    };
    publish(root, record.channel, &bytes)
}

/// Fills only an existing End's empty seal; identical retries re-establish directory durability.
pub(crate) fn seal(
    root: &Path,
    channel: u64,
    end_id: &str,
    successor: &SuccessorSeal,
) -> Result<Appended, String> {
    let _write = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let mut records = read(root, channel)?;
    let record = records
        .iter_mut()
        .find(|record| record.end_id == end_id)
        .ok_or("Codex end is absent or evicted")?;
    if !valid_seal(record, successor) {
        return Err("Codex successor identity mismatch".into());
    }
    match &record.successor {
        Some(existing) if existing != successor => {
            #[cfg(all(test, unix))]
            let overwrite = crate::services::codex_tui::execution_end::mutant("cas_overwrite");
            #[cfg(not(all(test, unix)))]
            let overwrite = false;
            if !overwrite {
                return Err("Codex end already names another successor".into());
            }
            record.successor = Some(successor.clone());
        }
        Some(_) => {}
        None => record.successor = Some(successor.clone()),
    }
    if encode(std::slice::from_ref(record))?.len() as u64 > MAX_BYTES {
        return Err("Codex successor seal exceeds its size limit".into());
    }
    let bytes = loop {
        let bytes = encode(&records)?;
        if bytes.len() as u64 <= MAX_BYTES {
            break bytes;
        }
        if records[0].end_id == end_id {
            return Err("Codex successor seal would evict its End".into());
        }
        records.remove(0);
    };
    publish(root, channel, &bytes)
}

fn publish(root: &Path, channel: u64, bytes: &[u8]) -> Result<Appended, String> {
    let dir = root.join(DIR_NAME);
    durable::ensure_dir(&dir).map_err(|error| error.to_string())?;
    durable::sweep_tmp(&dir).map_err(|error| error.to_string())?;
    let target = path(root, channel);
    match durable::replace(&target, bytes) {
        Ok(()) => Ok(Appended::Durable),
        Err(error) if fs::read(&target).is_ok_and(|now| now.as_slice() == bytes) => {
            Ok(Appended::PublishedUnsynced(error.to_string()))
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
#[path = "codex_end_tests.rs"]
mod tests;
