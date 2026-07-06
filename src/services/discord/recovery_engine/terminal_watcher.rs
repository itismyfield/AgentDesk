//! Terminal-success watcher / recovery start-offset helpers (#3479 item-2 split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: the helpers that
//! decide when a terminal-success tmux pane has finished draining its JSONL
//! output (so recovery may stop watching) and that compute the byte offset a
//! restart-recovery watcher should resume reading from. They depend only on
//! `std::fs`/`tokio::time` and the tmux `has_session` / `WRAPPER_TERMINAL_END_EVENT`
//! contracts, so they live in this leaf module. The async drain driver and the
//! offset helper are re-imported by the root module so existing call sites stay
//! byte-identical.

pub(super) fn output_has_bytes_after_offset(output_path: &str, start_offset: u64) -> bool {
    std::fs::metadata(output_path)
        .map(|meta| meta.len() > start_offset)
        .unwrap_or(false)
}

const TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

fn terminal_success_watcher_stop_allowed(
    confirmed_end: u64,
    tmux_tail_offset: u64,
    quiet_for: std::time::Duration,
) -> bool {
    confirmed_end >= tmux_tail_offset && quiet_for >= TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
}

pub(super) async fn terminal_success_output_drained_for_recovery(
    output_path: &str,
    confirmed_end: u64,
    tmux_session_name: Option<&str>,
) -> bool {
    let Ok(before_meta) = std::fs::metadata(output_path) else {
        return false;
    };
    let tmux_alive = tmux_session_name
        .map(crate::services::platform::tmux::has_session)
        .unwrap_or(false);

    if !tmux_alive {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    if !terminal_success_watcher_stop_allowed(
        confirmed_end,
        before_meta.len(),
        TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
    ) {
        return false;
    }

    // #2442 (H2) — fast-path: if the wrapper has already emitted the
    // `terminal_end` JSONL sentinel, the pane is *definitively* done
    // writing and we can graduate the 2s drain quiet-period immediately.
    // The wrapper writes the sentinel as one of its very last actions
    // before kill_child_tree/cleanup, so its presence is a strict superset
    // of the quiet-period heuristic. We still keep the legacy 2s sleep as
    // a fallback for SIGKILL paths that bypass the sentinel write.
    if jsonl_tail_contains_terminal_end_sentinel(output_path) {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    tokio::time::sleep(TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD).await;

    let tail_after = std::fs::metadata(output_path)
        .map(|meta| meta.len())
        .unwrap_or(confirmed_end.saturating_add(1));
    tail_after == confirmed_end
        && terminal_success_watcher_stop_allowed(
            confirmed_end,
            tail_after,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        )
}

/// #2442 — peek the JSONL tail (last ~4 KiB) for the wrapper's
/// `terminal_end` sentinel. Reading the tail rather than the entire file
/// keeps this O(1) regardless of jsonl size. False negatives (no sentinel
/// detected when one is present) just fall back to the legacy 2s
/// quiet-period sleep, so a partial-line edge case is harmless.
fn jsonl_tail_contains_terminal_end_sentinel(output_path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_WINDOW_BYTES: u64 = 4 * 1024;

    let Ok(mut file) = std::fs::File::open(output_path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity(TAIL_WINDOW_BYTES as usize);
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    // The sentinel is one JSONL line: {"type":"terminal_end",...}. We search the
    // literal `"type":"terminal_end"` token because the wrapper writes JSON via
    // `serde_json::Value::to_string()` (exact compact form); the contract lives in
    // `tmux_common::emit_wrapper_sentinel` (pretty-printing would need a rework).
    let needle = format!(
        "\"type\":\"{}\"",
        crate::services::tmux_common::WRAPPER_TERMINAL_END_EVENT
    );
    let haystack = String::from_utf8_lossy(&buf);
    haystack.contains(&needle)
}

pub(super) fn recovery_watcher_start_offset(
    output_path: &str,
    saved_last_offset: u64,
    turn_start_offset: Option<u64>,
) -> (u64, u64, bool) {
    let current_len = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    let resume_floor = turn_start_offset.unwrap_or(0);
    let desired_offset = saved_last_offset.max(resume_floor);
    if current_len >= desired_offset {
        (desired_offset, current_len, false)
    } else {
        // The output file was recreated or truncated while dcserver was down.
        // Resume from the beginning of the new file so we do not skip the
        // entire restarted session output.
        (0, current_len, true)
    }
}

pub(super) fn recovery_watcher_start_offset_for_state(
    output_path: &str,
    state: &crate::services::discord::inflight::InflightTurnState,
) -> (u64, u64, bool) {
    recovery_watcher_start_offset(output_path, state.last_offset, state.turn_start_offset)
}

pub(super) fn restart_report_watcher_start(
    tmux_session_name: &str,
    state: &crate::services::discord::inflight::InflightTurnState,
) -> Option<(String, u64, u64, bool)> {
    let output_path = state
        .output_path
        .clone()
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| {
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl")
        });
    if std::fs::metadata(&output_path).is_err() {
        return None;
    }
    let (initial_offset, current_len, truncated) =
        recovery_watcher_start_offset_for_state(&output_path, state);
    Some((output_path, initial_offset, current_len, truncated))
}

#[cfg(test)]
mod restart_report_tests {
    use super::{recovery_watcher_start_offset_for_state, restart_report_watcher_start};
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;

    #[test]
    fn restart_report_alive_watcher_uses_rebased_output_path_and_offset() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let transcript_path = temp.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, vec![b't'; 512_000]).expect("write transcript");
        let transcript_eof = std::fs::metadata(&transcript_path)
            .expect("transcript metadata")
            .len();
        let tmux_session_name = "AgentDesk-claude-adk-restart-report-rebased-transcript-44153003";
        let wrapper_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::create_dir_all(
            std::path::Path::new(&wrapper_path)
                .parent()
                .expect("wrapper parent"),
        )
        .expect("create wrapper parent");
        std::fs::write(&wrapper_path, vec![b'w'; 128]).expect("write stale wrapper");

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            44_153_003,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("88fdb7f3-0000-4000-8000-000000000000".to_string()),
            Some(tmux_session_name.to_string()),
            Some(transcript_path.display().to_string()),
            None,
            transcript_eof,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        state.turn_start_offset = Some(transcript_eof);
        state.last_watcher_relayed_offset = None;

        let stale_wrapper_start = recovery_watcher_start_offset_for_state(&wrapper_path, &state);
        assert_eq!(stale_wrapper_start, (0, 128, true));

        let (output_path, initial_offset, current_len, truncated) =
            restart_report_watcher_start(tmux_session_name, &state).expect("watcher start");

        assert_eq!(output_path, transcript_path.display().to_string());
        assert_ne!(output_path, wrapper_path);
        assert_eq!(initial_offset, transcript_eof);
        assert_eq!(current_len, transcript_eof);
        assert!(!truncated);

        let _ = std::fs::remove_file(wrapper_path);
    }
}

#[cfg(test)]
mod tests {
    use super::recovery_watcher_start_offset;
    use std::io::Write;

    fn temp_file_with_len(len: usize) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(&vec![b'x'; len]).expect("write temp file");
        file
    }

    #[test]
    fn recovery_start_offset_uses_turn_start_floor_when_last_offset_is_zero() {
        let file = temp_file_with_len(2_000);

        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), 0, Some(1_250));

        assert_eq!(offset, 1_250);
        assert_eq!(current_len, 2_000);
        assert!(!truncated);
    }

    #[test]
    fn recovery_start_offset_prefers_newer_saved_last_offset() {
        let file = temp_file_with_len(2_000);

        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), 1_600, Some(1_250));

        assert_eq!(offset, 1_600);
        assert_eq!(current_len, 2_000);
        assert!(!truncated);
    }

    #[test]
    fn recovery_start_offset_rewinds_only_when_output_truncated_below_floor() {
        let file = temp_file_with_len(900);

        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), 0, Some(1_250));

        assert_eq!(offset, 0);
        assert_eq!(current_len, 900);
        assert!(truncated);
    }

    #[test]
    fn recovery_start_offset_for_adopted_transcript_uses_persisted_eof_after_restart() {
        let wrapper_last_offset = 128;
        let transcript_eof = 512_000;
        let file = temp_file_with_len(transcript_eof);
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            crate::services::provider::ProviderKind::Claude,
            44_153_002,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("88fdb7f3-0000-4000-8000-000000000000".to_string()),
            Some("AgentDesk-claude-recovery-start-offset-adopted-transcript-44153002".to_string()),
            Some(file.path().display().to_string()),
            None,
            wrapper_last_offset,
        );
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        state.last_offset = transcript_eof as u64;
        state.turn_start_offset = Some(transcript_eof as u64);
        state.last_watcher_relayed_offset = None;

        let (offset, current_len, truncated) =
            super::recovery_watcher_start_offset_for_state(file.path().to_str().unwrap(), &state);

        assert_eq!(offset, transcript_eof as u64);
        assert_ne!(offset, wrapper_last_offset);
        assert_eq!(current_len, transcript_eof as u64);
        assert!(!truncated);
    }
}
