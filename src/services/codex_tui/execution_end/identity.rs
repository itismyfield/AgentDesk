//! Shared native-source evidence for fresh discovery and execution-end capture.

use std::io::{BufRead, BufReader, Read};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::session::source_observation::VerifiedCodexHookSource;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

const PROMPT_PREFIX_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(unix)]
#[derive(Debug)]
pub(crate) enum SourceFailure {
    Pending(String),
    Rejected(String),
}

#[cfg(unix)]
impl From<&str> for SourceFailure {
    fn from(reason: &str) -> Self {
        Self::Rejected(reason.into())
    }
}

#[cfg(unix)]
impl From<String> for SourceFailure {
    fn from(reason: String) -> Self {
        Self::Pending(reason)
    }
}

/// Verifies the exact old source; a resume may retain a source born before this execution.
#[cfg(unix)]
pub(crate) fn verify_source(
    context: &BindingContext,
    source: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
) -> Result<std::path::PathBuf, String> {
    verify_source_detail(context, source).map_err(|failure| match failure {
        SourceFailure::Pending(reason) | SourceFailure::Rejected(reason) => reason,
    })
}

#[cfg(unix)]
pub(crate) fn verify_source_detail(
    context: &BindingContext,
    source: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
) -> Result<std::path::PathBuf, SourceFailure> {
    use crate::services::codex_tui::session::source_observation::{
        CodexHookSourceClaim, CodexRolloutSource, verify_codex_hook_source,
    };
    let root = context
        .provider_root
        .as_deref()
        .ok_or("provider root absent")?;
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("provider root: {e}"))?;
    if super::mutant("root_raw_rejected") && canonical_root != root {
        return Err("provider root is not canonical".into());
    }
    if super::mutant("source_unchecked") {
        return Ok(canonical_root);
    }
    let canonical_source = source
        .path
        .canonicalize()
        .map_err(|_| SourceFailure::Pending("source is not canonical".into()))?;
    if canonical_source != source.path {
        return Err("source is not canonical".into());
    }
    let verified = verify_codex_hook_source(
        &canonical_root,
        &CodexHookSourceClaim {
            session_id: &source.session_id,
            transcript_path: Some(&source.path),
            expected_source: CodexRolloutSource::Cli,
        },
    )
    .map_err(|reason| {
        let message = format!("native source rejected: {reason:?}");
        if reason.may_resolve_later() {
            SourceFailure::Pending(message)
        } else {
            SourceFailure::Rejected(message)
        }
    })?;
    let exact_file = match verified.identity {
        #[cfg(unix)]
        SourceFileIdentity::Unix { dev, ino } => dev == source.dev && ino == source.ino,
        SourceFileIdentity::Unavailable => false,
    };
    if !exact_file
        || verified.rollout_path != source.path
        || verified.session_id != source.session_id
    {
        return Err("native source identity changed".into());
    }
    match context.launch_mode.as_str() {
        "resume"
            if context.expected_native_session_id.as_deref()
                == Some(source.session_id.as_str()) =>
        {
            Ok(canonical_root)
        }
        "fresh" => match fresh_source_evidence(context, &verified) {
            Ok(Some(())) => Ok(canonical_root),
            Ok(None) => Err(SourceFailure::Pending(
                "fresh source evidence pending".into(),
            )),
            Err(reason) => Err(SourceFailure::Rejected(if super::mutant("reason_merged") {
                "fresh source does not match execution context".into()
            } else {
                reason
            })),
        },
        _ => Err("source is not the execution's fresh or resumed source".into()),
    }
}

/// None means incomplete source evidence; it must never become an accepted fresh launch.
pub(crate) fn fresh_source_matches(
    context: &BindingContext,
    source: &VerifiedCodexHookSource,
) -> Option<bool> {
    match fresh_source_evidence(context, source) {
        Ok(evidence) => evidence.map(|()| true),
        Err(_) => Some(false),
    }
}

fn fresh_source_evidence(
    context: &BindingContext,
    source: &VerifiedCodexHookSource,
) -> Result<Option<()>, String> {
    if context.launch_mode != "fresh" || context.expected_native_session_id.is_some() {
        return Err("source is not a fresh execution".into());
    }
    let policy = context.source_policy.as_deref().unwrap_or("absent");
    if policy != "shadow" {
        return Err(format!("fresh policy {policy} outside shadow contract"));
    }
    if !source
        .created_at
        .is_some_and(|created| created >= context.created_at)
    {
        return Err("created_at before context".into());
    }
    match context
        .first_prompt_digest
        .as_deref()
        .map(|digest| prompt_matches(source, digest))
    {
        Some(Some(false)) => Err("first prompt mismatch".into()),
        Some(None) => Ok(None),
        _ => Ok(Some(())),
    }
}

fn prompt_matches(source: &VerifiedCodexHookSource, expected: &str) -> Option<bool> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(&source.rollout_path).ok()?;
    if !file.metadata().ok()?.is_file()
        || SourceFileIdentity::from_open_file(&file) != source.identity
    {
        return None;
    }
    let mut reader = BufReader::new(file.take(PROMPT_PREFIX_BYTES));
    let mut line = Vec::new();
    let matched = loop {
        line.clear();
        reader.read_until(b'\n', &mut line).ok()?;
        if line.last() != Some(&b'\n') {
            return (reader.get_ref().limit() == 0).then_some(false);
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            return Some(false);
        };
        if record["type"] != "response_item"
            || crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt(&record)
                .is_none()
        {
            continue;
        }
        // Preserve the untrimmed input used by the immutable launch-context digest.
        let raw = match &record["payload"]["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("input_text"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return Some(false),
        };
        break format!("sha256:{:x}", Sha256::digest(raw.as_bytes())) == expected;
    };
    let current = options.open(&source.rollout_path).ok()?;
    (SourceFileIdentity::from_open_file(&current) == source.identity).then_some(matched)
}
