use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight;

pub(crate) struct PendingCodexTuiRebindRelay {
    pub(crate) rollout_path: String,
    pub(crate) raw_start_offset: u64,
    pub(crate) truncate_relay_output: bool,
    pub(crate) session_id: Option<String>,
    pub(crate) already_relayed_response: String,
    pub(crate) already_normalized_replay_events: Vec<serde_json::Value>,
}
pub(crate) fn codex_tui_existing_normalized_relay_resume_path(
    tmux_session_name: &str,
    existing_inflight: Option<&inflight::InflightTurnState>,
) -> Option<String> {
    let existing = existing_inflight?;
    if existing.runtime_kind != Some(RuntimeHandoffKind::CodexTui) {
        return None;
    }
    let relay_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    if existing
        .output_path
        .as_deref()
        .is_none_or(|path| std::path::Path::new(path) != std::path::Path::new(&relay_path))
    {
        return None;
    }
    let relay_len = std::fs::metadata(&relay_path).ok()?.len();
    (relay_len > 0).then_some(relay_path)
}
pub(crate) fn codex_tui_rebind_raw_start_offset(
    tmux_session_name: &str,
    rollout_path: &str,
    codex_rollout_resume_offset: Option<u64>,
    codex_rollout_resume_offset_from_marker: bool,
    existing_inflight: Option<&inflight::InflightTurnState>,
    synthetic_initial_offset: u64,
    normalized_relay_prompt_replay_start_offset: Option<u64>,
) -> u64 {
    if let Some(existing) = existing_inflight {
        let existing_raw_cursor =
            codex_tui_existing_inflight_raw_cursor(tmux_session_name, rollout_path, existing);
        if codex_rollout_resume_offset_from_marker {
            let marker_offset = codex_rollout_resume_offset
                .or(existing_raw_cursor)
                .unwrap_or(0);
            if let Some(existing_raw_cursor) = existing_raw_cursor {
                return marker_offset.max(existing_raw_cursor);
            }
            return normalized_relay_prompt_replay_start_offset
                .map(|prompt_offset| marker_offset.max(prompt_offset))
                .unwrap_or(marker_offset);
        }
        if let Some(existing_raw_cursor) = existing_raw_cursor {
            return existing_raw_cursor;
        }
        if let Some(resume_offset) = codex_rollout_resume_offset {
            return normalized_relay_prompt_replay_start_offset
                .map(|prompt_offset| resume_offset.max(prompt_offset))
                .unwrap_or(resume_offset);
        }
        return normalized_relay_prompt_replay_start_offset.unwrap_or(0);
    }
    synthetic_initial_offset
}
pub(crate) fn codex_tui_existing_inflight_raw_cursor(
    tmux_session_name: &str,
    rollout_path: &str,
    existing: &inflight::InflightTurnState,
) -> Option<u64> {
    let normalized_relay =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let output_path = existing.output_path.as_deref()?;
    if std::path::Path::new(output_path) == std::path::Path::new(&normalized_relay) {
        return None;
    }
    if std::path::Path::new(output_path) != std::path::Path::new(rollout_path) {
        return None;
    }
    Some(
        existing
            .last_offset
            .max(existing.turn_start_offset.unwrap_or(0)),
    )
}
pub(crate) fn codex_tui_rebind_prompt_replay_start_offset(
    rollout_path: &str,
    prompt_text: &str,
) -> Option<u64> {
    use std::io::BufRead;

    let prompt_text = prompt_text.trim();
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut offset = 0_u64;
    let mut latest_user_prompt_offset = None;
    let mut latest_matching_prompt_offset = None;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).ok()?;
        if read == 0 {
            break;
        }
        offset = offset.saturating_add(read as u64);
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        let Some((candidate, _entry_id)) =
            crate::services::tui_prompt_dedupe::extract_codex_rollout_user_prompt_with_entry_id(
                &value,
            )
        else {
            continue;
        };
        latest_user_prompt_offset = Some(offset);
        if !prompt_text.is_empty()
            && crate::services::tui_prompt_dedupe::prompts_match(prompt_text, &candidate)
        {
            latest_matching_prompt_offset = Some(offset);
        }
    }
    latest_matching_prompt_offset.or(latest_user_prompt_offset)
}
pub(crate) fn codex_tui_existing_inflight_cursor_is_raw_rollout(
    tmux_session_name: &str,
    existing: &inflight::InflightTurnState,
) -> bool {
    let normalized_relay =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    existing
        .output_path
        .as_deref()
        .is_none_or(|path| std::path::Path::new(path) != std::path::Path::new(&normalized_relay))
}
pub(crate) fn codex_tui_rebind_replays_existing_raw_bytes(
    raw_start_offset: u64,
    codex_rollout_resume_offset: Option<u64>,
    synthetic_initial_offset: u64,
) -> bool {
    let replay_boundary = match codex_rollout_resume_offset {
        Some(resume_offset) if resume_offset < raw_start_offset => synthetic_initial_offset,
        Some(resume_offset) => resume_offset,
        None => synthetic_initial_offset,
    };
    raw_start_offset < replay_boundary
}
pub(crate) fn codex_tui_rebind_should_load_existing_normalized_replay_events(
    raw_start_offset: u64,
    replays_existing_raw_bytes: bool,
    normalized_relay_prompt_replay_start_offset: Option<u64>,
    synthetic_initial_offset: u64,
) -> bool {
    if replays_existing_raw_bytes {
        return true;
    }
    if raw_start_offset >= synthetic_initial_offset {
        return false;
    }
    normalized_relay_prompt_replay_start_offset
        .map(|prompt_offset| raw_start_offset <= prompt_offset)
        .unwrap_or(raw_start_offset == 0)
}
pub(crate) fn codex_tui_rebind_already_relayed_response_prefix(
    tmux_session_name: &str,
    rollout_path: &str,
    existing_inflight: Option<&inflight::InflightTurnState>,
    raw_start_offset: u64,
    should_suppress_existing_normalized_replay: bool,
    normalized_replay_events_available: bool,
) -> String {
    let Some(existing) = existing_inflight else {
        return String::new();
    };
    if existing.full_response.is_empty() {
        return String::new();
    }

    if let Some(raw_cursor) =
        codex_tui_existing_inflight_raw_cursor(tmux_session_name, rollout_path, existing)
    {
        return if raw_start_offset < raw_cursor {
            existing.full_response.clone()
        } else {
            String::new()
        };
    }

    let normalized_relay =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let tracks_normalized_relay = existing
        .output_path
        .as_deref()
        .is_some_and(|path| std::path::Path::new(path) == std::path::Path::new(&normalized_relay));
    if tracks_normalized_relay
        && should_suppress_existing_normalized_replay
        && !normalized_replay_events_available
    {
        return existing.full_response.clone();
    }

    String::new()
}
pub(crate) fn codex_tui_existing_normalized_relay_replay_events(
    relay_path: &str,
    turn_start_offset: Option<u64>,
) -> Vec<serde_json::Value> {
    use std::io::{BufRead, Seek};

    let Some(turn_start_offset) = turn_start_offset else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(relay_path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(file);
    if reader
        .seek(std::io::SeekFrom::Start(turn_start_offset))
        .is_err()
    {
        return Vec::new();
    }
    reader
        .lines()
        .filter_map(|line| {
            let line = line.ok()?;
            let line = line.trim();
            (!line.is_empty())
                .then(|| serde_json::from_str::<serde_json::Value>(line).ok())
                .flatten()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight;
    use crate::services::provider::ProviderKind;
    use std::ffi::OsString;
    use std::path::Path;

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set_path(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn codex_tui_rebind_ignores_rollout_resume_offset_without_inflight() {
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                "AgentDesk-codex-adk-cdx",
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                None,
                128,
                Some(0),
            ),
            128,
            "without an inflight row, stale marker offsets must not replay old Codex output"
        );
    }

    #[test]
    fn codex_tui_rebind_uses_rollout_resume_offset_with_existing_inflight() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-existing-inflight";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            64,
        );
        state.turn_start_offset = Some(32);

        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                Some(&state),
                128,
                None,
            ),
            64,
            "a stale marker must not replay bytes older than the active raw inflight cursor"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(96),
                true,
                Some(&state),
                128,
                None,
            ),
            96,
            "a newer marker can still move the raw replay cursor forward"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                128,
                None,
            ),
            64,
            "without a marker offset, existing inflight resumes from its raw cursor candidate"
        );

        state.output_path = Some("/tmp/old-codex-rollout.jsonl".to_string());
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                Some(&state),
                128,
                None,
            ),
            12,
            "raw cursors from a different rollout file must not clamp marker replay forward"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(24),
                false,
                Some(&state),
                128,
                None,
            ),
            24,
            "a resolved cursor for the selected rollout remains usable when the persisted raw path changed"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                128,
                None,
            ),
            0,
            "without a cursor for the selected rollout, a stale persisted raw path must replay from the beginning"
        );

        state.output_path = Some(normalized_relay);
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(12),
                true,
                Some(&state),
                256,
                Some(88),
            ),
            88,
            "a stale marker must not replay bytes older than the current normalized relay prompt boundary"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(128),
                true,
                Some(&state),
                256,
                Some(88),
            ),
            128,
            "a marker newer than the prompt boundary remains the replay cursor"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(128),
                false,
                Some(&state),
                256,
                None,
            ),
            128,
            "a resolved raw rollout cursor behind EOF must be used even when the inflight row tracks normalized relay bytes"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(256),
                false,
                Some(&state),
                256,
                None,
            ),
            256,
            "a rehydrated runtime-binding EOF remains equivalent to the current raw EOF"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                256,
                Some(88),
            ),
            88,
            "legacy markers without raw cursors must replay from the prompt boundary instead of skipping to EOF"
        );
        assert_eq!(
            codex_tui_rebind_raw_start_offset(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                None,
                false,
                Some(&state),
                256,
                None,
            ),
            0,
            "if no prompt boundary can be recovered, replay from the beginning with normalized-event dedupe rather than skipping to EOF"
        );
    }

    #[test]
    fn codex_tui_rebind_prompt_replay_start_offset_prefers_matching_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rollout = tmp.path().join("rollout.jsonl");
        let first = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "old prompt"}],
                "id": "old-user"
            }
        });
        let second = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "continue deployment"}],
                "id": "current-user"
            }
        });
        let first_line = format!("{first}\n");
        let second_line = format!("{second}\n");
        std::fs::write(&rollout, format!("{first_line}{second_line}")).expect("write rollout");

        assert_eq!(
            codex_tui_rebind_prompt_replay_start_offset(
                rollout.to_str().unwrap(),
                "continue deployment",
            ),
            Some((first_line.len() + second_line.len()) as u64)
        );
    }

    #[test]
    fn codex_tui_rebind_prompt_replay_start_offset_falls_back_to_latest_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rollout = tmp.path().join("rollout.jsonl");
        let first = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "old prompt"}],
                "id": "old-user"
            }
        });
        let second = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "latest prompt"}],
                "id": "latest-user"
            }
        });
        let first_line = format!("{first}\n");
        let second_line = format!("{second}\n");
        std::fs::write(&rollout, format!("{first_line}{second_line}")).expect("write rollout");

        assert_eq!(
            codex_tui_rebind_prompt_replay_start_offset(rollout.to_str().unwrap(), "missing"),
            Some((first_line.len() + second_line.len()) as u64),
            "when the saved Discord text does not exactly match, the latest user prompt is safer than EOF"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_raw_cursor_already_skips_relayed_response() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-raw-prefix";
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            64,
        );
        state.turn_start_offset = Some(32);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                64,
                false,
                false,
            ),
            "",
            "when raw tail resumes at the saved cursor, new post-restart output must not be filtered as replay"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_kept_when_raw_marker_replays_before_saved_cursor() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-marker-prefix";
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/codex-rollout.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = Some(32);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                false,
            ),
            "already relayed",
            "a marker that restarts before the saved raw cursor must strip already-relayed replay text"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_raw_path_changed() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-stale-raw-prefix";
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some("/tmp/old-codex-rollout.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = Some(32);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                false,
            ),
            "",
            "prefix stripping is only safe when the persisted raw cursor belongs to the selected rollout"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_normalized_relay_resumes_from_current_raw_cursor() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-normalized-prefix";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(normalized_relay),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                512,
                false,
                false,
            ),
            "",
            "normalized relay offsets are not raw rollout cursors, so EOF/current-cursor resumes must not use prefix stripping"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_disabled_when_normalized_marker_replay_uses_event_dedupe() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-normalized-marker-prefix";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(normalized_relay),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                true,
            ),
            "",
            "normalized marker replays must dedupe against existing normalized events, not the whole accumulated response"
        );
    }

    #[test]
    fn codex_tui_rebind_prefix_uses_full_response_when_normalized_replay_events_missing() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-empty-normalized-prefix";
        let normalized_relay =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(normalized_relay),
            None,
            128,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "already relayed".to_string();

        assert_eq!(
            codex_tui_rebind_already_relayed_response_prefix(
                tmux_session_name,
                "/tmp/codex-rollout.jsonl",
                Some(&state),
                12,
                true,
                false,
            ),
            "already relayed",
            "when normalized replay events are unavailable, raw replay must strip persisted response text"
        );
    }

    #[test]
    fn codex_tui_rebind_replay_detection_uses_raw_resume_offset_when_available() {
        assert!(
            !codex_tui_rebind_replays_existing_raw_bytes(512, Some(512), 1024),
            "resuming exactly at the saved raw cursor only tails new post-restart bytes"
        );
        assert!(
            codex_tui_rebind_replays_existing_raw_bytes(128, Some(512), 1024),
            "starting before the saved raw cursor replays already-normalized raw bytes"
        );
        assert!(
            codex_tui_rebind_replays_existing_raw_bytes(88, Some(0), 256),
            "a stale marker clamped forward by the prompt boundary still replays existing raw bytes"
        );
        assert!(
            codex_tui_rebind_replays_existing_raw_bytes(128, None, 1024),
            "without a raw cursor, synthetic EOF remains the replay boundary"
        );
        assert!(
            !codex_tui_rebind_replays_existing_raw_bytes(1024, None, 1024),
            "starting at synthetic EOF only tails future bytes"
        );
    }

    #[test]
    fn codex_tui_rebind_loads_normalized_replay_events_for_turn_start_equality() {
        assert!(
            codex_tui_rebind_should_load_existing_normalized_replay_events(0, false, None, 256),
            "raw resume at zero can be a turn-start cursor, so existing normalized events must dedupe replay"
        );
        assert!(
            codex_tui_rebind_should_load_existing_normalized_replay_events(
                88,
                false,
                Some(88),
                256,
            ),
            "raw resume exactly at the prompt boundary can still replay already-normalized assistant output"
        );
        assert!(
            !codex_tui_rebind_should_load_existing_normalized_replay_events(
                512,
                false,
                Some(88),
                1024,
            ),
            "a raw cursor advanced past the prompt boundary should tail post-cursor bytes without old event dedupe"
        );
        assert!(
            !codex_tui_rebind_should_load_existing_normalized_replay_events(
                1024,
                false,
                Some(88),
                1024,
            ),
            "starting at raw EOF does not replay existing bytes"
        );
        assert!(
            codex_tui_rebind_should_load_existing_normalized_replay_events(
                128,
                true,
                Some(88),
                1024,
            ),
            "explicit replay detection always enables existing normalized event dedupe"
        );
    }

    #[test]
    fn codex_tui_existing_normalized_relay_replay_events_start_at_turn_offset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay = tmp.path().join("relay.jsonl");
        let previous_turn = serde_json::json!({"type": "assistant", "content": "same"});
        let current_replay_same = serde_json::json!({"type": "assistant", "content": "same"});
        let current_replay_next = serde_json::json!({"type": "assistant", "content": "next"});
        let previous_line = format!("{previous_turn}\n");
        let current_same_line = format!("{current_replay_same}\n");
        let current_next_line = format!("{current_replay_next}\n");
        std::fs::write(
            &relay,
            format!("{previous_line}{current_same_line}{current_next_line}"),
        )
        .expect("write relay");

        assert_eq!(
            codex_tui_existing_normalized_relay_replay_events(
                relay.to_str().unwrap(),
                Some(previous_line.len() as u64),
            ),
            vec![current_replay_same, current_replay_next],
            "event dedupe must not consume identical events from previous turns"
        );
    }

    #[test]
    fn codex_tui_existing_normalized_relay_replay_events_disabled_without_turn_offset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let relay = tmp.path().join("relay.jsonl");
        std::fs::write(&relay, "{\"type\":\"assistant\",\"content\":\"same\"}\n")
            .expect("write relay");

        assert!(
            codex_tui_existing_normalized_relay_replay_events(relay.to_str().unwrap(), None)
                .is_empty(),
            "legacy rows without a current-turn offset cannot safely scope normalized-event dedupe"
        );
    }

    #[test]
    fn codex_tui_rebind_reuses_existing_nonempty_normalized_relay_file() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-existing-relay";
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::write(&relay_path, "{\"type\":\"assistant\"}\n").expect("write relay");

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(relay_path.clone()),
            None,
            0,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);

        assert_eq!(
            codex_tui_existing_normalized_relay_resume_path(tmux_session_name, Some(&state)),
            Some(relay_path),
            "a persisted normalized relay must be replayed instead of truncating and re-tailing raw rollout"
        );
    }

    #[test]
    fn codex_tui_rebind_does_not_reuse_empty_normalized_relay_file() {
        let _guard = lock_test_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let _root = EnvGuard::set_path("AGENTDESK_ROOT_DIR", tmp.path());
        let tmux_session_name = "AgentDesk-codex-adk-cdx-empty-relay";
        let relay_path =
            crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
        std::fs::write(&relay_path, "").expect("write empty relay");

        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "continue deployment".to_string(),
            Some("session-1".to_string()),
            Some(tmux_session_name.to_string()),
            Some(relay_path),
            None,
            0,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);

        assert_eq!(
            codex_tui_existing_normalized_relay_resume_path(tmux_session_name, Some(&state)),
            None,
            "an empty relay file should let recovery rebuild the normalized stream from raw rollout"
        );
    }
}
