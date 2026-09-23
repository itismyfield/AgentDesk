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
/// Why a TUI-direct row's saved offsets cannot be kept on the live transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdoptFenceForwardCause {
    TranscriptRotated,
    CoordinateSpaceMismatch,
    NewerTurnMixed,
}

impl AdoptFenceForwardCause {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TranscriptRotated => "transcript_rotated",
            Self::CoordinateSpaceMismatch => "coordinate_space_mismatch",
            Self::NewerTurnMixed => "newer_turn_mixed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TuiDirectAdoptOffsets {
    Preserve,
    FenceForward(AdoptFenceForwardCause),
}

/// `None` leaves rows other than a TUI-direct row adopted onto a Claude transcript to
/// [`claude_tui_force_initial_offset_for_adopted_transcript`].
pub(crate) fn tui_direct_adopt_offsets(
    runtime_kind: Option<RuntimeHandoffKind>,
    existing: &inflight::InflightTurnState,
    output_path: &str,
    latest_lease_turn_id: Option<&str>,
) -> Option<TuiDirectAdoptOffsets> {
    if existing.request_owner_user_id
        != crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
        || runtime_kind != Some(RuntimeHandoffKind::ClaudeTui)
        || claude_rebind_transcript_path(output_path).is_none()
    {
        return None;
    }
    fn non_empty(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }
    let cause = match non_empty(existing.output_path.as_deref()) {
        Some(saved) if !rebind_output_paths_same(saved, output_path) => {
            Some(AdoptFenceForwardCause::TranscriptRotated)
        }
        None => Some(AdoptFenceForwardCause::CoordinateSpaceMismatch),
        Some(_)
            if existing.runtime_kind != Some(RuntimeHandoffKind::ClaudeTui)
                || non_empty(existing.input_fifo_path.as_deref()).is_some() =>
        {
            Some(AdoptFenceForwardCause::CoordinateSpaceMismatch)
        }
        // A newer external turn on this tmux means `[turn_start_offset, EOF)` holds its output too.
        Some(_)
            if non_empty(latest_lease_turn_id).is_some_and(|latest| {
                non_empty(existing.external_turn_id.as_deref()) != Some(latest)
            }) =>
        {
            Some(AdoptFenceForwardCause::NewerTurnMixed)
        }
        Some(_) => None,
    };
    Some(cause.map_or(
        TuiDirectAdoptOffsets::Preserve,
        TuiDirectAdoptOffsets::FenceForward,
    ))
}

pub(crate) const ADOPT_FENCE_FORWARD_INVARIANT: &str = "tui_direct_adopt_keeps_its_turn_offsets";

pub(crate) struct AdoptFenceForward<'a> {
    pub cause: AdoptFenceForwardCause,
    /// The row as it was before adoption, in its own coordinate space.
    pub existing: &'a inflight::InflightTurnState,
    pub tmux_session_name: &'a str,
    pub output_path: &'a str,
    pub initial_offset: u64,
    pub latest_lease_turn_id: Option<&'a str>,
}

#[cfg(test)]
pub(crate) static ADOPT_FENCE_FORWARD_DISPATCHES: std::sync::Mutex<
    Vec<(crate::db::relay_dead_letter::RelayDeadLetterRecord, String)>,
> = std::sync::Mutex::new(Vec::new());

/// Fence-forward drops the row's unread range `[turn_start_offset, old EOF)`, so it is never
/// silent: its body goes to the dead-letter table, the loss to the invariant log, and one line
/// to the channel.
pub(crate) fn record_adopt_fence_forward(
    shared: &crate::services::discord::SharedData,
    provider: &crate::services::provider::ProviderKind,
    channel_id: u64,
    facts: &AdoptFenceForward<'_>,
) {
    let existing = facts.existing;
    let range_start = existing.turn_start_offset.unwrap_or(existing.last_offset);
    let old_path = (existing.output_path.as_deref().map(str::trim)).filter(|path| !path.is_empty());
    let old_eof = old_path
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or(0, |metadata| metadata.len().max(range_start));
    let dropped_bytes = old_eof.saturating_sub(range_start);
    let message_id = (existing.user_msg_id != 0).then(|| existing.user_msg_id.to_string());
    let record = crate::db::relay_dead_letter::RelayDeadLetterRecord {
        kind: crate::db::relay_dead_letter::KIND_ADOPT_FENCE_FORWARD.to_string(),
        channel_id: channel_id.to_string(),
        author_id: message_id.clone(),
        message_id,
        content: old_path.map_or_else(String::new, |path| {
            crate::services::discord::recovery_engine::extract_response_from_output_pub(
                path,
                range_start,
            )
        }),
        reason: format!(
            "cause={} dropped_bytes={dropped_bytes} range_start={range_start} old_eof={old_eof} old_path={} new_path={} new_offset={} tmux={} row_turn_id={} lease_turn_id={}",
            facts.cause.as_str(),
            old_path.unwrap_or("-"),
            facts.output_path,
            facts.initial_offset,
            facts.tmux_session_name,
            existing.external_turn_id.as_deref().unwrap_or("-"),
            facts.latest_lease_turn_id.unwrap_or("-"),
        ),
    };
    let notice = format!(
        "⚠️ **응답 이어받기 실패** — 세션 복구 중 이전 응답 {dropped_bytes}바이트를 이어서 전달하지 못해 보관했습니다 (relay dead-letter `{}`).",
        crate::db::relay_dead_letter::KIND_ADOPT_FENCE_FORWARD
    );
    crate::services::observability::record_invariant_check(
        false,
        crate::services::observability::InvariantViolation {
            provider: Some(provider.as_str()),
            channel_id: Some(channel_id),
            dispatch_id: None,
            session_key: Some(facts.tmux_session_name),
            turn_id: existing.external_turn_id.as_deref(),
            invariant: ADOPT_FENCE_FORWARD_INVARIANT,
            code_location: "src/services/discord/recovery_engine/manual_rebind/adoption.rs:record_adopt_fence_forward",
            message: "rebind could not keep a TUI-direct row's offsets and restarted the watcher at transcript EOF",
            details: serde_json::json!({ "cause": facts.cause.as_str(), "reason": record.reason }),
        },
    );
    #[cfg(test)]
    ADOPT_FENCE_FORWARD_DISPATCHES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push((record.clone(), notice.clone()));
    crate::db::relay_dead_letter::record_detached(shared.pg_pool.as_ref(), record);

    let Some(pool) = shared.pg_pool.clone() else {
        tracing::warn!(
            channel_id,
            "adopt fence-forward notice not sent: no outbox pool"
        );
        return;
    };
    tokio::spawn(async move {
        let target = format!("channel:{channel_id}");
        let message = crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: &notice,
            bot: crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
            source: "adopt_fence_forward_notice",
            reason_code: Some("adopt.fence_forward"),
            session_key: Some(&target),
        };
        if let Err(error) = crate::services::message_outbox::enqueue_outbox_pg(&pool, message).await
        {
            tracing::warn!("[dlq] failed to enqueue adopt fence-forward notice: {error}");
        }
    });
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

    fn tui_direct_row(
        channel_id: u64,
        tmux: &str,
        output_path: &str,
    ) -> inflight::InflightTurnState {
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            None,
            crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
            6_159_000_001,
            6_159_000_002,
            "tui prompt".to_string(),
            Some("61590000-0000-4000-8000-000000000000".to_string()),
            Some(tmux.to_string()),
            Some(output_path.to_string()),
            None,
            4_096,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.turn_start_offset = Some(4_096);
        row.external_turn_id = Some("turn-a".to_string());
        row.turn_source = inflight::TurnSource::ExternalInput;
        row
    }

    #[test]
    fn tui_direct_adopt_keeps_offsets_only_in_their_own_coordinate_space() {
        use AdoptFenceForwardCause::*;
        use TuiDirectAdoptOffsets::*;
        type Edit = fn(&mut inflight::InflightTurnState);
        let transcript = "/tmp/61590000-0000-4000-8000-000000000000.jsonl";
        let base = tui_direct_row(6_159_001, "AgentDesk-claude-6159-cc", transcript);
        const ROTATED: &str = "/tmp/71590000-0000-4000-8000-000000000000.jsonl";
        let mismatch = Some(FenceForward(CoordinateSpaceMismatch));
        let cases: [(Edit, Option<&str>, Option<TuiDirectAdoptOffsets>); 8] = [
            (|_| {}, None, Some(Preserve)),
            (|_| {}, Some("turn-a"), Some(Preserve)),
            (|_| {}, Some("turn-b"), Some(FenceForward(NewerTurnMixed))),
            (
                |row| row.output_path = Some(ROTATED.into()),
                None,
                Some(FenceForward(TranscriptRotated)),
            ),
            (|row| row.output_path = None, None, mismatch),
            (
                |row| row.input_fifo_path = Some("fifo".into()),
                None,
                mismatch,
            ),
            (|row| row.runtime_kind = None, None, mismatch),
            (|row| row.request_owner_user_id = 456, Some("turn-b"), None),
        ];
        for (index, (edit, lease, expected)) in cases.into_iter().enumerate() {
            let mut row = base.clone();
            edit(&mut row);
            let claude = Some(RuntimeHandoffKind::ClaudeTui);
            let actual = tui_direct_adopt_offsets(claude, &row, transcript, lease);
            assert_eq!(actual, expected, "case {index}");
        }
        let non_claude = tui_direct_adopt_offsets(None, &base, transcript, None);
        assert_eq!(non_claude, None);
    }

    /// Asserts exactly one fence-forward record (dead-letter + notice) and one violation.
    fn assert_one_fence_forward(
        events: &[crate::services::observability::events::StructuredEvent],
        channel_id: u64,
        needles: &[&str],
    ) {
        let dispatches: Vec<_> = ADOPT_FENCE_FORWARD_DISPATCHES
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(record, _)| record.channel_id == channel_id.to_string())
            .cloned()
            .collect();
        assert_eq!(dispatches.len(), 1, "Ok with no record is a silent loss");
        let (record, notice) = &dispatches[0];
        use crate::db::relay_dead_letter::KIND_ADOPT_FENCE_FORWARD;
        assert_eq!(record.kind, KIND_ADOPT_FENCE_FORWARD);
        let dump = format!("{record:?} {notice}");
        for needle in needles {
            assert!(dump.contains(needle), "missing {needle}: {dump}");
        }
        let violations = events.iter().filter(|event| {
            event.event_type == "invariant_violation"
                && event.channel_id == Some(channel_id)
                && event
                    .payload
                    .to_string()
                    .contains(ADOPT_FENCE_FORWARD_INVARIANT)
        });
        assert_eq!(violations.count(), 1);
    }

    /// Writes filler up to `turn_start`, then an assistant line holding `body` with no newline.
    fn write_transcript(path: &std::path::Path, body: &str) -> u64 {
        let mut bytes = vec![b'x'; 4_095];
        bytes.push(b'\n');
        let line = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": body}]}});
        bytes.extend_from_slice(line.to_string().as_bytes());
        std::fs::write(path, &bytes).expect("write transcript");
        bytes.len() as u64
    }

    #[cfg(unix)]
    use crate::services::observability::events::{StructuredEvent, test_capture::capture_sync};
    #[cfg(unix)]
    const SESSION_FILE: &str = "61590000-0000-4000-8000-000000000000.jsonl";

    /// Drives `rebind_inflight_for_channel` for a TUI-direct row resting at `turn_start_offset`
    /// 4096, with an optional newer lease turn and an in-memory committed offset.
    #[cfg(unix)]
    fn rebind_tui_direct_row(
        channel_id: u64,
        case: &str,
        lease_turn_id: Option<&str>,
        committed: u64,
        row_runtime_kind: Option<RuntimeHandoffKind>,
    ) -> Option<(u64, u64, Vec<StructuredEvent>)> {
        use crate::services::platform::tmux;
        use crate::services::tui_prompt_dedupe as dedupe;
        use std::sync::atomic::Ordering::SeqCst;
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tmp.path(),
        );
        if !tmux::is_available() {
            eprintln!("skipping TUI-direct full-path rebind test: tmux is not available");
            return None;
        }
        let session = format!("AgentDesk-claude-e2e6159{case}-{}-cc", std::process::id());
        let created = tmux::create_session(&session, None, "sleep 60").expect("create tmux");
        assert!(created.status.success(), "create tmux session");
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &session,
            RuntimeHandoffKind::ClaudeTui,
        )
        .expect("write runtime-kind marker");
        let transcript = tmp.path().join(SESSION_FILE);
        let eof = write_transcript(&transcript, &"BACKLOG_BODY_6159 ".repeat(512));
        let mut row = tui_direct_row(channel_id, &session, transcript.to_str().unwrap());
        row.runtime_kind = row_runtime_kind;
        assert!(inflight::save_inflight_state_if_absent(&row).expect("persist row"));
        if let Some(turn_id) = lease_turn_id {
            let mut lease = dedupe::ExternalInputRelayLease::unassigned(Some(channel_id));
            lease.turn_id = Some(turn_id.to_string());
            dedupe::record_external_input_turn_lease("claude", &session, lease);
        }
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = poise::serenity_prelude::ChannelId::new(channel_id);
        let coord = shared.tmux_relay_coord(channel);
        coord.confirmed_end_offset.store(committed, SeqCst);
        let http = std::sync::Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (result, events) = capture_sync(|| {
            runtime.block_on(super::super::rebind_inflight_for_channel(
                &http,
                &shared,
                &ProviderKind::Claude,
                channel_id,
                Some(session.clone()),
                super::super::ManualRebindOverrides::default(),
                None,
            ))
        });
        // Dropping the runtime cancels the adopted watcher's tasks.
        drop(runtime);
        dedupe::clear_external_input_relay_lease("claude", &session, channel_id);
        let _ = crate::services::platform::tmux::kill_session(&session, "rebind test cleanup");
        let outcome = result.expect("TUI-direct row is adopted");
        Some((outcome.initial_offset, eof, events))
    }

    #[cfg(unix)]
    #[test]
    fn rebind_fences_a_tui_direct_row_forward_past_a_newer_turn_and_records_it() {
        let channel_id = 6_159_003_u64;
        let claude = Some(RuntimeHandoffKind::ClaudeTui);
        let Some((initial, eof, events)) =
            rebind_tui_direct_row(channel_id, "mixed", Some("turn-b"), 0, claude)
        else {
            return;
        };
        assert_eq!(initial, eof, "the newer turn must not replay here");
        // The dead-letter keeps the dropped range's body; the notice names its size.
        let dropped = eof - 4_096;
        let needles = [
            "cause=newer_turn_mixed",
            "BACKLOG_BODY_6159",
            &format!("dropped_bytes={dropped}"),
            &format!("{dropped}바이트"),
        ];
        assert_one_fence_forward(&events, channel_id, &needles);
    }

    #[cfg(unix)]
    #[test]
    fn rebind_preserves_a_tui_direct_row_but_never_below_the_committed_offset() {
        let channel_id = 6_159_004_u64;
        let claude = Some(RuntimeHandoffKind::ClaudeTui);
        let Some((initial, eof, _)) =
            rebind_tui_direct_row(channel_id, "keep", None, 8_192, claude)
        else {
            return;
        };
        assert!(eof > 8_192);
        assert_eq!(initial, 8_192, "resume, clamped to committed");
        let dispatches = ADOPT_FENCE_FORWARD_DISPATCHES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fenced = dispatches
            .iter()
            .any(|(record, _)| record.channel_id == channel_id.to_string());
        assert!(!fenced, "preserve is not a fence-forward");
    }

    #[cfg(unix)]
    #[test]
    fn rebind_never_rebases_an_unstamped_tui_direct_row_to_eof_without_a_record() {
        let channel_id = 6_159_005_u64;
        let Some((initial, eof, events)) =
            rebind_tui_direct_row(channel_id, "unstamped", None, 0, None)
        else {
            return;
        };
        assert_eq!(initial, eof);
        assert_one_fence_forward(&events, channel_id, &["cause=coordinate_space_mismatch"]);
    }
}
