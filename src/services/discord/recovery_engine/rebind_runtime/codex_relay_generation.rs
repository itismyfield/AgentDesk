use super::RebindError;

pub(super) type CodexRebindRelayGenerationGate = std::sync::Arc<std::sync::Mutex<u64>>;

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

pub(super) fn prepare(
    relay_output_path: &str,
    truncate_relay_output: bool,
) -> Result<(CodexRebindRelayGenerationGate, u64), RebindError> {
    let gate = generation_gate(relay_output_path);
    let generation = {
        let mut generation = gate.lock().unwrap_or_else(|poison| poison.into_inner());
        *generation = generation.saturating_add(1).max(1);
        if truncate_relay_output {
            std::fs::File::create(relay_output_path).map_err(|error| {
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
fn publish_generation(session: &str, previous: i64) -> Result<i64, String> {
    use crate::services::discord::outbound::delivery_record;
    use std::io::Write;
    if delivery_record::current_generation_mtime_ns(session) != previous {
        return Err("generation changed before publication".into());
    }
    let path = crate::services::tmux_common::session_temp_path(session, "generation");
    let temporary = format!("{path}.replay.{}", uuid::Uuid::new_v4());
    let publish = || -> std::io::Result<i64> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(
            crate::services::discord::runtime_store::process_generation()
                .to_string()
                .as_bytes(),
        )?;
        // A new inode must publish a distinct key even on a coarse timestamp filesystem.
        if previous > 0 {
            let time = std::time::UNIX_EPOCH
                + std::time::Duration::from_nanos(previous as u64 + 1_000_000_000);
            file.set_times(std::fs::FileTimes::new().set_modified(time))?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        let observed = delivery_record::current_generation_mtime_ns(session);
        if observed == 0 || observed == previous {
            return Err(std::io::Error::other("new generation readback failed"));
        }
        Ok(observed)
    };
    let result = publish().map_err(|e| e.to_string());
    let _ = std::fs::remove_file(temporary);
    result
}

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) fn publish_reconstruction(
    session: &str,
    temporary: &std::path::Path,
    output: &str,
    previous: i64,
    checkpoint: &mut impl FnMut(
        super::super::legacy_restart_replay::install::InstallCheckpoint,
    ) -> Result<(), String>,
    replacement_authority: impl FnOnce() -> Result<
        crate::services::discord::tmux_watcher_registry::TmuxWatcherRegistryGuard,
        String,
    >,
) -> Result<(CodexRebindRelayGenerationGate, u64, (u64, u64), i64), String> {
    use super::super::legacy_restart_replay::install::InstallCheckpoint;
    use crate::services::cluster::stream_relay::SourceFileIdentity;
    use crate::services::discord::outbound::delivery_record;
    crate::services::tmux_common::with_tmux_source_authority(session, |_| {
        let _writer_authority = replacement_authority()?;
        if delivery_record::current_generation_mtime_ns(session) != previous {
            return Err("generation changed during proof".to_string());
        }
        let generation = prepare(output, false).map_err(|e| format!("{e:?}"))?;
        let posted = publish_generation(session, previous)?;
        checkpoint(InstallCheckpoint::GenerationPublished)?;
        if delivery_record::current_generation_mtime_ns(session) != posted {
            return Err("generation changed before file install".to_string());
        }
        std::fs::rename(temporary, output).map_err(|e| e.to_string())?;
        checkpoint(InstallCheckpoint::FileInstalled)?;
        let file = std::fs::File::open(output).map_err(|e| e.to_string())?;
        let file_identity = match SourceFileIdentity::from_open_file(&file) {
            #[cfg(unix)]
            SourceFileIdentity::Unix { dev, ino } => (dev, ino),
            SourceFileIdentity::Unavailable => {
                return Err("installed source identity unavailable".to_string());
            }
        };
        if delivery_record::current_generation_mtime_ns(session) != posted {
            return Err("generation changed after file install".to_string());
        }
        Ok((generation.0, generation.1, file_identity, posted))
    })
}
