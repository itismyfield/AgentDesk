use super::RebindError;

pub(in crate::services::discord::recovery_engine) type CodexRebindRelayGenerationGate =
    std::sync::Arc<std::sync::Mutex<u64>>;

static CODEX_REBIND_RELAY_GENERATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Weak<std::sync::Mutex<u64>>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn generation_gate(relay_output_path: &str) -> CodexRebindRelayGenerationGate {
    let mut registry = CODEX_REBIND_RELAY_GENERATIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(gate) = registry
        .get(relay_output_path)
        .and_then(std::sync::Weak::upgrade)
    {
        return gate;
    }
    let gate = std::sync::Arc::new(std::sync::Mutex::new(0));
    registry.insert(
        relay_output_path.to_string(),
        std::sync::Arc::downgrade(&gate),
    );
    gate
}

/// Reserve an absent producer without invalidating an incumbent generation.
pub(in crate::services::discord::recovery_engine) fn ensure_absent(
    relay_output_path: &str,
) -> Option<(CodexRebindRelayGenerationGate, u64)> {
    let mut registry = CODEX_REBIND_RELAY_GENERATIONS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if registry
        .get(relay_output_path)
        .and_then(std::sync::Weak::upgrade)
        .is_some()
    {
        return None;
    }
    let gate = std::sync::Arc::new(std::sync::Mutex::new(1));
    registry.insert(
        relay_output_path.to_string(),
        std::sync::Arc::downgrade(&gate),
    );
    Some((gate, 1))
}

pub(in crate::services::discord::recovery_engine) fn present(relay_output_path: &str) -> bool {
    CODEX_REBIND_RELAY_GENERATIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(relay_output_path)
        .and_then(std::sync::Weak::upgrade)
        .is_some()
}

pub(super) fn prepare(
    relay_output_path: &str,
    truncate_relay_output: bool,
) -> Result<(CodexRebindRelayGenerationGate, u64), RebindError> {
    let gate = generation_gate(relay_output_path);
    let generation = {
        let mut generation = gate.lock().unwrap_or_else(|poison| poison.into_inner());
        *generation = generation.saturating_add(1).max(1);
        if truncate_relay_output {
            replace_relay_output(relay_output_path).map_err(|error| {
                RebindError::Internal(format!(
                    "create Codex TUI rebind relay output {relay_output_path}: {error}"
                ))
            })?;
        } else {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(relay_output_path)
                .map_err(|error| {
                    RebindError::Internal(format!(
                        "open Codex TUI rebind relay output {relay_output_path}: {error}"
                    ))
                })?;
        }
        *generation
    };
    Ok((gate, generation))
}

/// Restarting the relay's byte coordinates also gives it a new file, so a reader
/// keyed on the file identity never reads the new bytes as the old file's.
#[cfg(unix)]
fn replace_relay_output(relay_output_path: &str) -> std::io::Result<()> {
    let path = std::path::Path::new(relay_output_path);
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("relay output path has no directory"))?;
    let staging = tempfile::NamedTempFile::new_in(directory)?;
    // Noted before the rename so an idle tick can never see the new file first.
    crate::services::discord::recovery_engine::relay_output_replacements::note_relay_output_replaced(
        relay_output_path,
    );
    staging.persist(path).map(drop).map_err(|error| error.error)
}

#[cfg(not(unix))]
fn replace_relay_output(relay_output_path: &str) -> std::io::Result<()> {
    std::fs::File::create(relay_output_path).map(drop)
}

pub(super) fn write_message(
    output: &mut std::fs::File,
    relay_path: &std::path::Path,
    message: crate::services::agent_protocol::StreamMessage,
    already_normalized_replay_events: &mut std::collections::VecDeque<serde_json::Value>,
    relay_generation_gate: &CodexRebindRelayGenerationGate,
    relay_generation: u64,
) -> Result<bool, String> {
    use std::io::Write;

    let current_generation = relay_generation_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if *current_generation != relay_generation {
        return Err("Codex TUI rebind relay generation was superseded".to_string());
    }
    let Some(json) = super::codex_rebind_stream_message_json(message) else {
        return Ok(false);
    };
    if super::codex_rebind_should_skip_existing_normalized_event(
        &json,
        already_normalized_replay_events,
    ) {
        return Ok(false);
    }
    serde_json::to_writer(&mut *output, &json)
        .map_err(|error| format!("serialize normalized Codex rebind event: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .map_err(|error| format!("write {}: {error}", relay_path.display()))?;
    Ok(true)
}

#[cfg(all(test, unix))]
#[path = "codex_relay_generation_tests.rs"]
mod tests;
