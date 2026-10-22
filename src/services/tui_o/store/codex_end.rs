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
        Self {
            schema: context.schema,
            provider: context.provider.clone(),
            created_at: context.created_at,
            execution_nonce: context.execution_nonce.clone(),
            tmux_session: context.tmux_session.clone(),
            channel_id: context.channel_id,
            owner_runtime_root: context.owner_runtime_root.clone(),
            host: context.host.clone(),
            expected_native_session_id: context.expected_native_session_id.clone(),
            launch_mode: context.launch_mode.clone(),
            provider_root: context.provider_root.clone(),
            first_prompt_digest: context.first_prompt_digest.clone(),
            source_policy: context.source_policy.clone(),
        }
    }
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
        if existing != record {
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
    let dir = root.join(DIR_NAME);
    durable::ensure_dir(&dir).map_err(|error| error.to_string())?;
    durable::sweep_tmp(&dir).map_err(|error| error.to_string())?;
    let target = path(root, record.channel);
    match durable::replace(&target, &bytes) {
        Ok(()) => Ok(Appended::Durable),
        Err(error) if fs::read(&target).is_ok_and(|now| now == bytes) => {
            Ok(Appended::PublishedUnsynced(error.to_string()))
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
#[path = "codex_end_tests.rs"]
mod tests;
