use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::session::source_observation::{
    CodexHookSourceClaim, CodexRolloutSource, VerifiedCodexHookSource, verify_codex_hook_source,
};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

// Bound discovery work; a complete prefix beyond this budget remains merely observed.
const PROMPT_PREFIX_BYTES: u64 = 4 * 1024 * 1024;

pub(super) fn cause(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    channel: u64,
    binding: &TuiRuntimeBinding,
) -> Option<CauseSource> {
    let observed = Some(CauseSource::Observed);
    let tmux = authority.session();
    let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(tmux) else {
        return observed;
    };
    let Some(context) = binding_context::input_context("codex", &nonce) else {
        return observed;
    };
    if context.schema != 1
        || context.provider != "codex"
        || context.execution_nonce != nonce
        || context.tmux_session != tmux
        || context.launch_mode != "fresh"
        || context.expected_native_session_id.is_some()
        || context.source_policy.as_deref() != Some("shadow")
        || context.channel_id != Some(channel)
        || context.owner_runtime_root != crate::services::tmux_common::current_tmux_owner_marker()
    {
        return observed;
    }
    let (Some(root), Some(id)) = (
        context.provider_root.as_deref(),
        binding.session_id.as_deref(),
    ) else {
        return observed;
    };
    let path = Path::new(&binding.output_path);
    let source = match verify_codex_hook_source(
        root,
        &CodexHookSourceClaim {
            session_id: id,
            transcript_path: Some(path),
            expected_source: CodexRolloutSource::Cli,
        },
    ) {
        Ok(source) => source,
        Err(error) if error.may_resolve_later() => return None,
        Err(_) => return observed,
    };
    if source.session_id != id || path.canonicalize().ok().as_ref() != Some(&source.rollout_path) {
        return observed;
    }
    if !source
        .created_at
        .is_some_and(|created| created >= context.created_at)
    {
        return observed;
    }
    if let Some(expected) = context.first_prompt_digest.as_deref() {
        return prompt_matches(&source, expected).map(|matches| {
            if matches {
                CauseSource::Launch
            } else {
                CauseSource::Observed
            }
        });
    }
    Some(CauseSource::Launch)
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
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
            return Some(false);
        };
        if record["type"] != "response_item"
            || crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt(&record)
                .is_none()
        {
            continue;
        }
        // The shared extractor selects real input, but its trimming must not alter the digest.
        let content = &record["payload"]["content"];
        let raw = match content {
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
