use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct JsonlReadChunk {
    pub(in crate::services::discord) data: Vec<u8>,
    pub(in crate::services::discord) start_offset: u64,
    pub(in crate::services::discord) end_offset: u64,
    pub(in crate::services::discord) skipped_partial_record: bool,
}

fn read_jsonl_chunk(
    path: &str,
    offset: u64,
    align_persisted_offset: bool,
) -> Result<JsonlReadChunk, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let file_len = file.metadata().map_err(|e| format!("metadata: {e}"))?.len();
    let clamped_offset = offset.min(file_len);
    let on_record_boundary = if !align_persisted_offset || clamped_offset == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(clamped_offset - 1))
            .map_err(|e| format!("boundary seek: {e}"))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)
            .map_err(|e| format!("boundary read: {e}"))?;
        previous[0] == b'\n'
    };

    file.seek(SeekFrom::Start(clamped_offset))
        .map_err(|e| format!("seek: {e}"))?;
    let mut discarded = 0u64;
    if align_persisted_offset && !on_record_boundary {
        let mut byte = [0u8; 1];
        loop {
            let n = file
                .read(&mut byte)
                .map_err(|e| format!("align read: {e}"))?;
            if n == 0 {
                break;
            }
            discarded = discarded.saturating_add(1);
            if byte[0] == b'\n' {
                break;
            }
        }
    }

    let start_offset = clamped_offset.saturating_add(discarded);
    let mut data = vec![0u8; 16_384];
    let n = file.read(&mut data).map_err(|e| format!("read: {e}"))?;
    data.truncate(n);
    Ok(JsonlReadChunk {
        data,
        start_offset,
        end_offset: start_offset.saturating_add(n as u64),
        skipped_partial_record: align_persisted_offset && !on_record_boundary,
    })
}

pub(in crate::services::discord) fn read_jsonl_chunk_at_attach(
    path: &str,
    offset: u64,
) -> Result<JsonlReadChunk, String> {
    read_jsonl_chunk(path, offset, true)
}

pub(in crate::services::discord) fn read_jsonl_chunk_contiguous(
    path: &str,
    offset: u64,
) -> Result<JsonlReadChunk, String> {
    read_jsonl_chunk(path, offset, false)
}

#[cfg(test)]
mod tests {
    use super::super::forced_kill::watcher_session_is_main_orchestration;
    use super::*;
    use crate::services::discord::tmux::tmux_output_stream::{
        WatcherToolState, process_watcher_lines,
    };
    use crate::services::session_backend::StreamLineState;
    use poise::serenity_prelude::ChannelId;

    #[test]
    fn attach_offset_discards_only_the_persisted_partial_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("incident.jsonl");
        let partial = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "content": "review mentions rate limit as ordinary prose"
        })
        .to_string();
        let complete = serde_json::json!({
            "type": "result",
            "is_error": false,
            "result": "completed"
        })
        .to_string();
        std::fs::write(&path, format!("{partial}\n{complete}\n")).expect("fixture");
        let offset = (partial.find("rate limit").expect("phrase") + 2) as u64;

        let chunk = read_jsonl_chunk_at_attach(path.to_str().expect("path"), offset).expect("read");

        assert!(chunk.skipped_partial_record);
        assert_eq!(
            String::from_utf8(chunk.data).expect("utf8"),
            format!("{complete}\n")
        );
        assert_eq!(chunk.start_offset, partial.len() as u64 + 1);
    }

    #[test]
    fn contiguous_read_preserves_a_record_larger_than_the_read_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("large-record.jsonl");
        let record = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "content": format!("review discusses oauth and unauthorized handling: {}", "x".repeat(20_000))
        })
        .to_string();
        let result = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "completed"
        })
        .to_string();
        std::fs::write(&path, format!("{record}\n{result}\n")).expect("fixture");

        let first = read_jsonl_chunk_at_attach(path.to_str().expect("path"), 0).expect("first");
        assert_eq!(first.data.len(), 16_384);
        let second = read_jsonl_chunk_contiguous(path.to_str().expect("path"), first.end_offset)
            .expect("second");

        assert!(!second.skipped_partial_record);
        let joined = format!(
            "{}{}",
            String::from_utf8(first.data).expect("first utf8"),
            String::from_utf8(second.data).expect("second utf8")
        );
        assert_eq!(joined, format!("{record}\n{result}\n"));
    }

    #[test]
    fn incident_chain_preserves_terminal_result_without_false_abort() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("incident-chain.jsonl");
        let stale_partial = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "content": "stale persisted record with oauth and unauthorized review prose"
        })
        .to_string();
        let notification = serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "content": format!(
                "review of oauth unauthorized and rate limit paths: {}",
                "x".repeat(20_000)
            )
        })
        .to_string();
        let result = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "review complete"
        })
        .to_string();
        std::fs::write(
            &path,
            format!("{stale_partial}\n{notification}\n{result}\n"),
        )
        .expect("fixture");
        let persisted_offset = (stale_partial.find("oauth").expect("phrase") + 2) as u64;

        let mut buffer = String::new();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();
        let first = read_jsonl_chunk_at_attach(path.to_str().expect("path"), persisted_offset)
            .expect("first");
        assert!(first.skipped_partial_record);
        let offset = first.end_offset;
        buffer.push_str(&String::from_utf8(first.data).expect("first utf8"));
        let first_outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);
        assert!(!first_outcome.found_result);
        assert!(!buffer.is_empty(), "partial record must remain buffered");

        let second =
            read_jsonl_chunk_contiguous(path.to_str().expect("path"), offset).expect("second");
        buffer.push_str(&String::from_utf8(second.data).expect("second utf8"));
        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(
            outcome.found_result,
            "terminal result must survive chunking"
        );
        assert!(
            !outcome.is_auth_error,
            "review prose must not trigger auth abort"
        );
        assert!(
            !outcome.is_provider_overloaded,
            "review prose must not trigger overload abort"
        );
        assert!(
            watcher_session_is_main_orchestration("AgentDesk-claude-adk-cc", ChannelId::new(42)),
            "the incident chain must resolve the orchestration runtime as protected"
        );
    }
}
