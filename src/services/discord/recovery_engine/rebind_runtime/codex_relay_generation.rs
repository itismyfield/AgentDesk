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
) -> Result<Option<u64>, String> {
    use std::io::Write;

    let current_generation = relay_generation_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if *current_generation != relay_generation {
        return Err("Codex TUI rebind relay generation was superseded".to_string());
    }
    let Some(json) = super::codex_rebind_stream_message_json(message) else {
        return Ok(None);
    };
    if super::codex_rebind_should_skip_existing_normalized_event(
        &json,
        already_normalized_replay_events,
    ) {
        return Ok(None);
    }
    serde_json::to_writer(&mut *output, &json)
        .map_err(|error| format!("serialize normalized Codex rebind event: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .and_then(|_| output.sync_data())
        .map_err(|error| format!("write {}: {error}", relay_path.display()))?;
    output
        .metadata()
        .map(|metadata| Some(metadata.len()))
        .map_err(|error| format!("stat {} after write: {error}", relay_path.display()))
}

/// Synchronous raw-rollout -> canonical JSONL -> bridge adapter shared by
/// managed and external Codex-TUI turns.  The bridge never sees a message until
/// its normalized record is durable, and raw rollout offsets never cross this
/// boundary.
impl crate::services::discord::CodexCanonicalRelay {
    pub(crate) fn sender(
        &self,
    ) -> std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage> {
        self.sender
            .as_ref()
            .expect("canonical relay sender unavailable after finish")
            .clone()
    }

    pub(crate) fn close_input(&mut self) {
        self.sender.take();
    }

    pub(crate) fn finish(
        mut self,
    ) -> Result<crate::services::discord::CodexCanonicalRelayResult, String> {
        self.sender.take();
        self.join
            .take()
            .expect("canonical relay join unavailable after finish")
            .join()
            .map_err(|_| "Codex canonical relay panicked".to_string())?
    }

    fn close_and_join(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for crate::services::discord::CodexCanonicalRelay {
    fn drop(&mut self) {
        self.close_and_join();
    }
}

impl crate::services::discord::CodexCanonicalRelay {
    pub(crate) fn start(
        tmux_session_name: &str,
        downstream: std::sync::mpsc::Sender<crate::services::agent_protocol::StreamMessage>,
        truncate_output: bool,
        committed_start_offset: Option<u64>,
    ) -> Result<Self, String> {
        let output_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let (generation_gate, generation) = prepare(&output_path, truncate_output)
            .map_err(|error| format!("prepare Codex canonical relay: {error}"))?;
        let output = std::path::PathBuf::from(&output_path);
        super::codex_rebind_ensure_jsonl_append_boundary(&output)?;
        let physical_end = std::fs::metadata(&output)
            .map(|metadata| metadata.len())
            .map_err(|error| format!("stat Codex canonical relay {output_path}: {error}"))?;
        let start_offset = if truncate_output {
            0
        } else {
            committed_start_offset.unwrap_or(physical_end)
        };
        if start_offset > physical_end {
            return Err(format!(
                "Codex canonical committed offset {start_offset} exceeds EOF {physical_end}"
            ));
        }
        let mut replay_events = codex_canonical_replay_events(&output, start_offset)?;
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&output)
            .map_err(|error| format!("open Codex canonical relay {output_path}: {error}"))?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let join = std::thread::Builder::new()
            .name("codex_canonical_relay".to_string())
            .spawn(move || {
                let mut end_offset = start_offset;
                let mut terminal_records = 0_u64;
                let mut known_response = String::new();
                for message in receiver {
                    match message {
                        crate::services::agent_protocol::StreamMessage::OutputOffset { .. } => {
                            continue;
                        }
                        crate::services::agent_protocol::StreamMessage::RuntimeReady {
                            handoff:
                                crate::services::agent_protocol::RuntimeHandoff::CodexTui {
                                    thread_id,
                                    tmux_session_name,
                                    ..
                                },
                        } => {
                            if !replay_events.is_empty() {
                                return Err(
                                    "Codex canonical replay suffix remained before RuntimeReady"
                                        .to_string(),
                                );
                            }
                            downstream
                                .send(
                                    crate::services::agent_protocol::StreamMessage::OutputOffset {
                                        offset: end_offset,
                                    },
                                )
                                .map_err(|_| {
                                    "bridge receiver closed before canonical RuntimeReady"
                                        .to_string()
                                })?;
                            downstream
                            .send(
                                crate::services::agent_protocol::StreamMessage::RuntimeReady {
                                    handoff:
                                        crate::services::agent_protocol::RuntimeHandoff::CodexTui {
                                            rollout_path: output.display().to_string(),
                                            thread_id,
                                            tmux_session_name,
                                            last_offset: end_offset,
                                        },
                                },
                            )
                            .map_err(|_| {
                                "bridge receiver closed during canonical RuntimeReady".to_string()
                            })?;
                            continue;
                        }
                        message => {
                            if let crate::services::agent_protocol::StreamMessage::Done {
                                result,
                                ..
                            } = &message
                                && let Some(suffix) =
                                    super::codex_rebind_done_result_suffix(&known_response, result)
                            {
                                if let Some(offset) = write_or_replay_codex_canonical_message(
                                    &mut writer,
                                    &output,
                                    crate::services::agent_protocol::StreamMessage::Text {
                                        content: suffix.clone(),
                                    },
                                    &mut replay_events,
                                    &generation_gate,
                                    generation,
                                )? {
                                    end_offset = offset;
                                }
                                known_response.push_str(&suffix);
                            }
                            if let crate::services::agent_protocol::StreamMessage::Text {
                                content,
                            } = &message
                            {
                                known_response.push_str(content);
                            }
                            let is_terminal = matches!(
                                &message,
                                crate::services::agent_protocol::StreamMessage::Done { .. }
                                    | crate::services::agent_protocol::StreamMessage::Error { .. }
                            );
                            if let Some(offset) = write_or_replay_codex_canonical_message(
                                &mut writer,
                                &output,
                                message.clone(),
                                &mut replay_events,
                                &generation_gate,
                                generation,
                            )? {
                                end_offset = offset;
                                downstream
                                .send(
                                    crate::services::agent_protocol::StreamMessage::OutputOffset {
                                        offset: end_offset,
                                    },
                                )
                                .map_err(|_| {
                                    "bridge receiver closed before canonical message".to_string()
                                })?;
                                if is_terminal {
                                    terminal_records = terminal_records.saturating_add(1);
                                }
                            }
                            downstream.send(message).map_err(|_| {
                                "bridge receiver closed during canonical message".to_string()
                            })?;
                        }
                    }
                }
                if !replay_events.is_empty() {
                    return Err("Codex canonical replay suffix was not fully consumed".to_string());
                }
                Ok(crate::services::discord::CodexCanonicalRelayResult {
                    output_path: output.display().to_string(),
                    start_offset,
                    end_offset,
                    terminal_records,
                })
            })
            .map_err(|error| format!("spawn Codex canonical relay: {error}"))?;
        Ok(Self {
            sender: Some(sender),
            join: Some(join),
        })
    }
}

fn codex_canonical_replay_events(
    path: &std::path::Path,
    start_offset: u64,
) -> Result<std::collections::VecDeque<(serde_json::Value, u64)>, String> {
    use std::io::{BufRead, Seek};

    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("open canonical replay {}: {error}", path.display()))?;
    if start_offset > 0 {
        file.seek(std::io::SeekFrom::Start(start_offset - 1))
            .map_err(|error| format!("seek canonical replay {}: {error}", path.display()))?;
        let mut prior = [0_u8; 1];
        std::io::Read::read_exact(&mut file, &mut prior)
            .map_err(|error| format!("read canonical replay boundary: {error}"))?;
        if prior[0] != b'\n' {
            return Err("Codex canonical committed offset is not a JSONL boundary".to_string());
        }
    }
    file.seek(std::io::SeekFrom::Start(start_offset))
        .map_err(|error| format!("seek canonical replay {}: {error}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut cursor = start_offset;
    let mut replay = std::collections::VecDeque::new();
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("read canonical replay {}: {error}", path.display()))?;
        if bytes == 0 {
            break;
        }
        cursor = cursor.saturating_add(bytes as u64);
        if !line.ends_with('\n') {
            return Err("Codex canonical replay ends with a partial record".to_string());
        }
        let json = serde_json::from_str(line.trim_end_matches('\n'))
            .map_err(|error| format!("parse canonical replay {}: {error}", path.display()))?;
        replay.push_back((json, cursor));
    }
    Ok(replay)
}

fn write_or_replay_codex_canonical_message(
    output: &mut std::fs::File,
    path: &std::path::Path,
    message: crate::services::agent_protocol::StreamMessage,
    replay: &mut std::collections::VecDeque<(serde_json::Value, u64)>,
    generation_gate: &CodexRebindRelayGenerationGate,
    generation: u64,
) -> Result<Option<u64>, String> {
    use std::io::Write;

    let Some(json) = super::codex_rebind_stream_message_json(message) else {
        return Ok(None);
    };
    let current_generation = generation_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if *current_generation != generation {
        return Err("Codex canonical relay generation was superseded".to_string());
    }
    if let Some((expected, end)) = replay.front() {
        if expected != &json {
            return Err("Codex canonical replay diverged from raw rollout".to_string());
        }
        let end = *end;
        replay.pop_front();
        return Ok(Some(end));
    }
    serde_json::to_writer(&mut *output, &json)
        .map_err(|error| format!("serialize canonical Codex event: {error}"))?;
    output
        .write_all(b"\n")
        .and_then(|_| output.flush())
        .and_then(|_| output.sync_data())
        .map_err(|error| format!("write canonical {}: {error}", path.display()))?;
    output
        .metadata()
        .map(|metadata| Some(metadata.len()))
        .map_err(|error| format!("stat canonical {}: {error}", path.display()))
}
