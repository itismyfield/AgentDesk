//! Pure file-offset JSONL readers for recovery (#3479 r8 split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: these helpers read
//! a provider JSONL transcript from a byte offset and reconstruct turn outcome
//! (terminal `result` end offset, accumulated assistant response). They depend
//! only on `std::fs` + `serde_json`, so they live in this leaf module. Async
//! drain helpers and analytics/transcript persistence stay in the root module.

/// Check whether a **successful** result record exists after the given offset.
/// Error results are not considered completion — they should not trigger the
/// recovery completed-turn path (✅ reaction, idle dispatch, etc.).
pub(in crate::services::discord) fn success_result_end_offset_after_offset(
    output_path: &str,
    start_offset: u64,
) -> Option<u64> {
    let Ok(bytes) = std::fs::read(output_path) else {
        return None;
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    let mut absolute_end = start as u64;
    for segment in bytes[start..].split_inclusive(|byte| *byte == b'\n') {
        absolute_end = absolute_end.saturating_add(segment.len() as u64);
        let line = String::from_utf8_lossy(segment);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let is_result = value.get("type").and_then(|v| v.as_str()) == Some("result");
        let is_error = value
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_result && !is_error {
            return Some(absolute_end);
        }
    }

    None
}

/// Extract accumulated assistant text from output JSONL after the given offset.
pub(super) fn extract_response_from_output(output_path: &str, start_offset: u64) -> String {
    extract_response_from_output_pub(output_path, start_offset)
}

/// Public wrapper for turn_bridge fallback recovery.
///
/// Mirrors the `resolve_done_response` logic from `turn_bridge.rs`:
/// when tool_use was seen and no post-tool assistant text followed,
/// prefer the `result` record over stale pre-tool narration.
pub fn extract_response_from_output_pub(output_path: &str, start_offset: u64) -> String {
    let Ok(bytes) = std::fs::read(output_path) else {
        return String::new();
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    extract_response_from_jsonl_bytes(&bytes[start..])
}

/// Recover only the captured episode's bytes; a successor appended after `end`
/// is outside the caller's authority and must not be attributed to this answer.
pub(in crate::services::discord) fn extract_response_from_output_range(
    output_path: &std::path::Path,
    start: u64,
    end: u64,
    native_codex: bool,
) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(output_path).map_err(|e| e.to_string())?;
    if end < start || end > file.metadata().map_err(|e| e.to_string())?.len() {
        return Err("captured recovery range is outside the source file".into());
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(end - start)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 != end - start {
        return Err("captured recovery source changed while reading".into());
    }
    if native_codex {
        return crate::services::codex_tui::rollout_tail::recover_captured_rollout_response(&bytes);
    }
    // A malformed or incomplete captured frame cannot prove an empty result.
    let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
    let mut state = crate::services::session_backend::StreamLineState::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).map_err(|e| e.to_string())?;
        // Share the live reader's terminal semantics, including native Claude
        // stop_hook_summary with no session ID. Drain metadata after each line.
        crate::services::session_backend::process_stream_line(line, &sender, &mut state);
        receiver.try_iter().for_each(drop);
    }
    if state.final_result.is_none() || state.stdout_error.is_some() {
        return Err("captured recovery source has no successful terminal result".into());
    }
    Ok(extract_response_from_jsonl_bytes(&bytes))
}

fn extract_response_from_jsonl_bytes(bytes: &[u8]) -> String {
    let mut response = String::new();
    let mut any_tool_used = false;
    let mut has_post_tool_text = false;
    let mut result_text = String::new();

    for line in String::from_utf8_lossy(bytes).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "assistant" => {
                if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
                    if let Some(arr) = content.as_array() {
                        let mut block_has_tool = false;
                        let mut block_has_text = false;
                        for block in arr {
                            match block.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        if !text.is_empty() {
                                            response.push_str(text);
                                            block_has_text = true;
                                        }
                                    }
                                }
                                Some("tool_use") => {
                                    block_has_tool = true;
                                }
                                _ => {}
                            }
                        }
                        if block_has_tool {
                            any_tool_used = true;
                            // Reset: text in a block that also has tool_use is pre-tool narration
                            has_post_tool_text = false;
                        } else if block_has_text && any_tool_used {
                            has_post_tool_text = true;
                        }
                    }
                }
            }
            "result" => {
                let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
                if subtype == "success" {
                    if let Some(r) = value.get("result").and_then(|v| v.as_str()) {
                        result_text = r.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    // Apply resolve_done_response logic: if tool was used and no post-tool
    // assistant text followed, the accumulated response is stale narration —
    // prefer the authoritative result record.
    if !result_text.is_empty() {
        if response.trim().is_empty() {
            return result_text;
        }
        if any_tool_used && !has_post_tool_text {
            return result_text;
        }
    }
    response
}

#[cfg(test)]
mod captured_claude_recovery_tests {
    use super::extract_response_from_output_range;
    use std::io::Write;

    fn recover(captured: &str) -> Result<String, String> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        // Neither a predecessor nor a successor may prove completion or supply
        // text for the exact captured range, even when both are successful.
        let foreign =
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"foreign answer\"}\n";
        write!(file, "{foreign}{captured}{foreign}").unwrap();
        extract_response_from_output_range(
            file.path(),
            foreign.len() as u64,
            (foreign.len() + captured.len()) as u64,
            false,
        )
    }

    #[test]
    fn bounded_claude_recovery_accepts_native_stop_hook_with_nullable_session_5071() {
        for session in [serde_json::Value::Null, serde_json::json!("native-session")] {
            for body in ["", "captured answer"] {
                let assistant = serde_json::json!({
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": body}]}
                });
                let terminal = serde_json::json!({
                    "type": "system", "subtype": "stop_hook_summary", "session_id": session
                });
                let captured = format!("{assistant}\n{terminal}\n");
                assert_eq!(recover(&captured).unwrap(), body);
            }
        }
    }

    #[test]
    fn bounded_claude_recovery_keeps_normalized_result_and_tool_precedence_5071() {
        let captured = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"stale narration\"},{\"type\":\"tool_use\",\"name\":\"Read\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"final answer\"}\n"
        );
        assert_eq!(recover(captured).unwrap(), "final answer");
    }

    #[test]
    fn bounded_claude_recovery_rejects_unproven_or_malformed_captured_terminal_5071() {
        for captured in [
            "",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"unfinished\"}]}}\n",
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            "{\"type\":\"unknown_record\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":true}\n",
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}\n{malformed\n",
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\"}\n{\"type\":\"result\",\"is_error\":true}\n",
        ] {
            assert!(recover(captured).is_err(), "captured: {captured}");
        }
    }
}
