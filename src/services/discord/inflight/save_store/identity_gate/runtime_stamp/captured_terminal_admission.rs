use super::*;

pub(super) async fn admit_uncaptured_terminal(
    local: &mut InflightTurnState,
    baseline: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    can_deliver_directly: bool,
    message: StreamMessage,
) -> Result<(StreamMessage, Option<TuiTerminalRange>, bool), GuardedSaveOutcome> {
    if !matches!(
        &message,
        StreamMessage::CodexTuiTerminalDone {
            captured_source: None,
            ..
        }
    ) {
        return Ok((message, None, false));
    }
    let mut owned_local = local.clone();
    let mut owned_baseline = baseline.clone();
    let expected = expected.clone();
    // Legacy Codex admission must also acquire, commit and release off the scheduler.
    let (admitted, owned_local, owned_baseline) = tokio::task::spawn_blocking(move || {
        let admitted = owned_local.admit_codex_tui_terminal_frame(
            &mut owned_baseline,
            &expected,
            can_deliver_directly,
            message,
        );
        (admitted, owned_local, owned_baseline)
    })
    .await
    .map_err(|_| GuardedSaveOutcome::IoError)?;
    *local = owned_local;
    *baseline = owned_baseline;
    Ok(admitted)
}

pub(super) struct CapturedTerminalAdmission {
    pub(super) provider: ProviderKind,
    pub(super) runtime: RuntimeHandoffKind,
    pub(super) result: String,
    pub(super) session_id: Option<String>,
    pub(super) transcript_path: String,
    pub(super) tmux_session_name: String,
    pub(super) turn_nonce: String,
    pub(super) source_start: u64,
    pub(super) complete_record_end: u64,
    pub(super) generation_mtime_ns: i64,
    pub(super) source_file_dev: u64,
    pub(super) source_file_ino: u64,
    pub(super) captured: std::sync::Arc<crate::services::provider::CancelToken>,
    pub(super) can_deliver_directly: bool,
}

impl CapturedTerminalAdmission {
    pub(super) fn admit(
        self,
        local: &mut InflightTurnState,
        baseline: &mut InflightTurnState,
        expected: &InflightTurnIdentity,
    ) -> Result<(StreamMessage, Option<TuiTerminalRange>, bool), GuardedSaveOutcome> {
        let Self {
            provider,
            runtime,
            result,
            session_id,
            transcript_path,
            tmux_session_name,
            turn_nonce,
            source_start,
            complete_record_end,
            generation_mtime_ns,
            source_file_dev,
            source_file_ino,
            captured,
            can_deliver_directly,
        } = self;
        let mismatch = GuardedSaveOutcome::SuccessorOwned;
        let root = inflight_runtime_root().ok_or(mismatch)?;
        let path = inflight_state_path(&root, &provider, local.channel_id);
        let _lock = lock_inflight_state_path(&path).map_err(|_| GuardedSaveOutcome::IoError)?;
        let mut fresh = read_inflight_state_for_guarded_write(
            &path,
            &provider,
            local.channel_id,
            expected,
            "turn_bridge::captured_tui_terminal_range",
        )?;
        crate::services::tmux_common::with_tmux_source_authority(&tmux_session_name, |authority| {
            let (canonical, file_len) = canonical_regular_file(&transcript_path).ok_or(mismatch)?;
            let mut binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)
            .ok_or(mismatch)?;
            let session = nonempty(session_id.as_deref())
                .or(nonempty(local.session_id.as_deref()))
                .unwrap_or_default()
                .to_owned();
            // Only the captured FD/generation/original-actor witness fills a missing session ID;
            // conflicting known sessions remain a hard mismatch.
            if [
                local.session_id.as_deref(),
                fresh.session_id.as_deref(),
                binding.session_id.as_deref(),
            ]
            .into_iter()
            .filter_map(nonempty)
            .any(|known| known != session)
            {
                return Err(mismatch);
            }
            binding.session_id = nonempty(Some(&session)).map(str::to_owned);
            if !can_deliver_directly
                || local.provider_kind() != Some(provider.clone())
                || local.runtime_kind != Some(runtime)
                || local.turn_start_offset != Some(source_start)
                || source_start >= complete_record_end
                || file_len < complete_record_end
                || generation_mtime_ns <= 0
                || tmux_generation_file_mtime_ns(&tmux_session_name) != generation_mtime_ns
                || file_identity(&canonical) != Some((source_file_dev, source_file_ino))
                || !captured_binding_matches(
                    &binding,
                    &canonical,
                    &session,
                    (source_start, complete_record_end),
                    false,
                    runtime,
                )
            {
                return Err(mismatch);
            }
            if fresh.turn_nonce.as_deref() != Some(turn_nonce.as_str())
                || fresh.tmux_session_name.as_deref() != Some(tmux_session_name.as_str())
                || fresh.runtime_kind != Some(runtime)
                || fresh.turn_start_offset != Some(source_start)
                || fresh.last_offset > complete_record_end
                || fresh.restart_mode.is_some()
                || fresh.rebind_origin
                || fresh.terminal_delivery_committed
                || !StreamRelayAuthority::from_state(&fresh).bridge_owns_relay()
                || fresh
                    .output_path
                    .as_deref()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    .as_deref()
                    != Some(canonical.as_path())
            {
                return Err(mismatch);
            }
            let before_admission = InflightEpisodePin::from_state(&fresh);
            fresh.session_id.clone_from(&binding.session_id);
            let range = persist_terminal_range(
                &root,
                &path,
                (local, &mut *baseline),
                fresh,
                (&result, canonical, &session),
                (
                    (source_start, complete_record_end),
                    generation_mtime_ns,
                    Some((source_file_dev, source_file_ino)),
                ),
            )?;
            crate::services::discord::tui_prompt_relay::preserve_admitted_source(
                &before_admission,
                baseline,
                &captured,
            );
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding_under_source_authority(authority, binding);
            Ok((
                StreamMessage::Done { result, session_id },
                Some(range),
                true,
            ))
        })
    }
}
