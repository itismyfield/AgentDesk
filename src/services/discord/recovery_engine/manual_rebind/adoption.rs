use super::super::rebind_runtime::claude_rebind_transcript_path;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight;

pub(crate) fn rebind_initial_offset_with_floor(
    initial_offset: u64,
    minimum_initial_offset: Option<u64>,
    output_len: u64,
) -> u64 {
    match minimum_initial_offset {
        Some(floor) if floor > initial_offset && floor <= output_len => floor,
        _ => initial_offset,
    }
}
pub(crate) fn rebind_initial_offset_with_floor_unless_forced(
    initial_offset: u64,
    minimum_initial_offset: Option<u64>,
    output_len: Option<u64>,
    force_initial_offset: Option<u64>,
) -> u64 {
    if force_initial_offset.is_some() {
        return initial_offset;
    }
    rebind_initial_offset_with_floor(
        initial_offset,
        minimum_initial_offset,
        output_len.unwrap_or(0),
    )
}
pub(crate) fn claude_tui_force_initial_offset_for_adopted_transcript(
    runtime_kind: Option<RuntimeHandoffKind>,
    existing_inflight: Option<&inflight::InflightTurnState>,
    output_path: &str,
    synthetic_initial_offset: u64,
) -> Option<u64> {
    let existing = existing_inflight?;
    if runtime_kind != Some(RuntimeHandoffKind::ClaudeTui)
        || claude_rebind_transcript_path(output_path).is_none()
    {
        return None;
    }

    let existing_saved_output_path = existing
        .output_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let already_durable_claude_transcript = existing_saved_output_path
        .is_some_and(|saved_path| rebind_output_paths_same(saved_path, output_path))
        && existing.runtime_kind == Some(RuntimeHandoffKind::ClaudeTui)
        && existing
            .input_fifo_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .is_none();
    // #4400 (b) review r2: the adopted #3107 self-heal orphan (zero-id,
    // watcher-owned) was born FROM the live transcript stream — its persisted
    // offsets are transcript-space by construction whenever its saved output
    // path IS the resolved transcript. The self-heal does not stamp
    // `runtime_kind`, so the durable-stamp check above can never admit it;
    // without this arm the EOF rebase below would drop the backlog written
    // while the watcher was dead (invariant I3 — the 16:30~16:37Z window).
    // Path equality plus the fifo-less shape keep wrapper-space coordinates
    // excluded exactly as the durable check does.
    let adopted_orphan_same_transcript = existing_saved_output_path
        .is_some_and(|saved_path| rebind_output_paths_same(saved_path, output_path))
        && existing.is_adoptable_orphaned_synthetic_watcher_row()
        && existing
            .input_fifo_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .is_none();
    if already_durable_claude_transcript || adopted_orphan_same_transcript {
        return None;
    }

    Some(synthetic_initial_offset)
}
pub(crate) fn claude_tui_rebind_should_reregister_runtime_binding(
    runtime_kind: Option<RuntimeHandoffKind>,
    output_path: &str,
) -> bool {
    runtime_kind == Some(RuntimeHandoffKind::ClaudeTui)
        && claude_rebind_transcript_path(output_path).is_some()
}
pub(crate) fn rebind_output_paths_same(left: &str, right: &str) -> bool {
    let left_path = std::path::Path::new(left);
    let right_path = std::path::Path::new(right);
    let left_path = std::fs::canonicalize(left_path).unwrap_or_else(|_| left_path.to_path_buf());
    let right_path = std::fs::canonicalize(right_path).unwrap_or_else(|_| right_path.to_path_buf());
    left_path == right_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight;
    use crate::services::provider::ProviderKind;

    #[test]
    fn rebind_initial_offset_floor_uses_committed_frontier_within_output_len() {
        assert_eq!(
            rebind_initial_offset_with_floor(0, Some(13_400_000), 14_930_326),
            13_400_000,
            "force-clean respawn must not restart from zero when the durable frontier is in-file"
        );
        assert_eq!(
            rebind_initial_offset_with_floor(14_930_326, Some(13_400_000), 14_930_326),
            14_930_326,
            "the floor must never move an already-newer resume offset backward"
        );
        assert_eq!(
            rebind_initial_offset_with_floor(0, Some(13_400_000), 1024),
            0,
            "if the output file was truncated below the durable frontier, keep the boot-path safe restart behavior"
        );
        assert_eq!(rebind_initial_offset_with_floor(512, None, 4096), 512);
    }

    #[test]
    fn codex_tui_truncate_rebind_force_initial_offset_skips_floor() {
        assert_eq!(
            rebind_initial_offset_with_floor_unless_forced(
                0,
                Some(13_400_000),
                Some(14_930_326),
                Some(0),
            ),
            0,
            "Codex-TUI truncate rebuild resets relay coordinates, so a durable old-space frontier must not raise the forced zero offset"
        );
        assert_eq!(
            rebind_initial_offset_with_floor_unless_forced(
                0,
                Some(13_400_000),
                Some(14_930_326),
                None,
            ),
            13_400_000,
            "non-forced rebinds still honor an in-file durable floor"
        );
    }

    #[test]
    fn claude_tui_adopted_transcript_rebind_starts_existing_inflight_at_eof() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wrapper_path = tmp.path().join("wrapper.jsonl");
        let transcript_path = tmp
            .path()
            .join("48fdb7f3-0000-4000-8000-000000000000.jsonl");
        std::fs::write(&wrapper_path, vec![b'w'; 128]).expect("write wrapper");
        std::fs::write(&transcript_path, vec![b't'; 512_000]).expect("write transcript");
        let transcript_eof = std::fs::metadata(&transcript_path)
            .expect("transcript metadata")
            .len();
        let existing = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            42_001,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("old-session".to_string()),
            Some("AgentDesk-claude-adopted-transcript-eof-42001".to_string()),
            Some(wrapper_path.display().to_string()),
            Some("/tmp/wrapper.input".to_string()),
            128,
        );

        let forced = claude_tui_force_initial_offset_for_adopted_transcript(
            Some(RuntimeHandoffKind::ClaudeTui),
            Some(&existing),
            transcript_path.to_str().expect("utf8 transcript path"),
            transcript_eof,
        );
        let initial_offset = rebind_initial_offset_with_floor_unless_forced(
            forced.expect("adopted transcript must force EOF"),
            Some(64),
            Some(transcript_eof),
            forced,
        );

        assert_eq!(
            initial_offset, transcript_eof,
            "existing wrapper offsets are not valid coordinates in the adopted Claude transcript"
        );
    }

    #[test]
    fn claude_tui_adopted_transcript_rebind_forces_eof_when_saved_output_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp
            .path()
            .join("58fdb7f3-0000-4000-8000-000000000000.jsonl");
        std::fs::write(&transcript_path, vec![b't'; 4096]).expect("write transcript");
        let transcript_eof = std::fs::metadata(&transcript_path).unwrap().len();
        let mut existing = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            43_001,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("old-session".to_string()),
            Some("AgentDesk-claude-adopted-transcript-missing-output-43001".to_string()),
            None,
            Some("/tmp/wrapper.input".to_string()),
            128,
        );

        assert_eq!(
            claude_tui_force_initial_offset_for_adopted_transcript(
                Some(RuntimeHandoffKind::ClaudeTui),
                Some(&existing),
                transcript_path.to_str().unwrap(),
                transcript_eof,
            ),
            Some(transcript_eof),
            "adopting a transcript from an empty saved output path must still rebase old coordinates"
        );

        existing.output_path = Some("   ".to_string());
        assert_eq!(
            claude_tui_force_initial_offset_for_adopted_transcript(
                Some(RuntimeHandoffKind::ClaudeTui),
                Some(&existing),
                transcript_path.to_str().unwrap(),
                transcript_eof,
            ),
            Some(transcript_eof),
            "blank saved output paths are equivalent to missing paths for transcript adoption"
        );
    }

    #[test]
    fn claude_tui_same_transcript_without_durable_runtime_stamp_forces_eof() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let transcript_path = tmp
            .path()
            .join("68fdb7f3-0000-4000-8000-000000000000.jsonl");
        std::fs::write(&transcript_path, vec![b't'; 8192]).expect("write transcript");
        let transcript_eof = std::fs::metadata(&transcript_path).unwrap().len();
        let mut existing = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            44_001,
            Some("adk-cc".to_string()),
            123,
            456,
            789,
            "continue".to_string(),
            Some("old-session".to_string()),
            Some("AgentDesk-claude-same-transcript-runtime-stamp-44001".to_string()),
            Some(transcript_path.display().to_string()),
            Some("/tmp/wrapper.input".to_string()),
            128,
        );
        existing.runtime_kind = None;

        assert_eq!(
            claude_tui_force_initial_offset_for_adopted_transcript(
                Some(RuntimeHandoffKind::ClaudeTui),
                Some(&existing),
                transcript_path.to_str().unwrap(),
                transcript_eof,
            ),
            Some(transcript_eof),
            "path equality alone does not prove persisted offsets are transcript-space"
        );

        existing.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        existing.input_fifo_path = None;
        assert_eq!(
            claude_tui_force_initial_offset_for_adopted_transcript(
                Some(RuntimeHandoffKind::ClaudeTui),
                Some(&existing),
                transcript_path.to_str().unwrap(),
                transcript_eof,
            ),
            None,
            "a durable ClaudeTui transcript row can safely resume from its saved transcript offsets"
        );
    }

    #[test]
    fn claude_tui_rebind_reregister_requires_transcript_output_path() {
        assert!(claude_tui_rebind_should_reregister_runtime_binding(
            Some(RuntimeHandoffKind::ClaudeTui),
            "/tmp/78fdb7f3-0000-4000-8000-000000000000.jsonl",
        ));
        assert!(!claude_tui_rebind_should_reregister_runtime_binding(
            Some(RuntimeHandoffKind::ClaudeTui),
            "/tmp/AgentDesk-claude-reregister-wrapper-output-1355.jsonl",
        ));
        assert!(!claude_tui_rebind_should_reregister_runtime_binding(
            Some(RuntimeHandoffKind::LegacyTmuxWrapper),
            "/tmp/78fdb7f3-0000-4000-8000-000000000000.jsonl",
        ));
    }
}
