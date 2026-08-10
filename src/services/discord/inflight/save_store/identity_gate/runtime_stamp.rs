use super::*;
use crate::services::discord::inflight::store::persist_under_lock_with_snapshot;

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct AdmittedCodexTerminalRange {
    channel_id: u64,
    identity: InflightTurnIdentity,
    rollout_path: String,
    session_id: String,
    tmux_session_name: String,
    turn_nonce: String,
    source_start: u64,
    complete_record_end: u64,
    generation_mtime_ns: i64,
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn canonical_regular_file(path: &str) -> Option<(PathBuf, u64)> {
    let path = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    metadata.is_file().then_some((path, metadata.len()))
}

fn marker_matches_terminal_range(
    tmux_session_name: &str,
    rollout_path: &Path,
    session_id: &str,
    source_start: u64,
) -> bool {
    crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
        .is_some_and(|marker| {
            nonempty(marker.session_id.as_deref()) == Some(session_id)
                && marker.rollout_start_offset == Some(source_start)
                && std::fs::canonicalize(marker.rollout_path).ok().as_deref() == Some(rollout_path)
        })
}

fn binding_matches_terminal_range(
    tmux_session_name: &str,
    rollout_path: &Path,
    session_id: &str,
    expected_offset: u64,
) -> bool {
    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
        .is_some_and(|binding| {
            binding.runtime_kind == RuntimeHandoffKind::CodexTui
                && nonempty(binding.session_id.as_deref()) == Some(session_id)
                && binding.last_offset == expected_offset
                && std::fs::canonicalize(binding.output_path).ok().as_deref() == Some(rollout_path)
        })
}

impl InflightTurnState {
    pub(in crate::services::discord) fn admit_codex_tui_terminal_frame(
        &mut self,
        persisted_baseline: &mut InflightTurnState,
        expected: &InflightTurnIdentity,
        can_chain_locally: bool,
        message: crate::services::agent_protocol::StreamMessage,
        caller: &'static str,
    ) -> (
        crate::services::agent_protocol::StreamMessage,
        Option<AdmittedCodexTerminalRange>,
        bool,
    ) {
        use crate::services::agent_protocol::StreamMessage;
        let StreamMessage::CodexTuiTerminalDone {
            result,
            session_id,
            rollout_path,
            tmux_session_name,
            turn_nonce,
            source_start,
            complete_record_end,
        } = message
        else {
            return (message, None, false);
        };
        let done = StreamMessage::Done {
            result,
            session_id: session_id.clone(),
        };
        let Some(root) = inflight_runtime_root() else {
            return (done, None, true);
        };
        let admitted = admit_codex_tui_terminal_range_in_root(
            &root,
            self,
            persisted_baseline,
            expected,
            can_chain_locally,
            session_id.as_deref(),
            &rollout_path,
            &tmux_session_name,
            &turn_nonce,
            source_start,
            complete_record_end,
            caller,
        );
        if let Err(outcome) = &admitted {
            tracing::info!(?outcome, caller, "Codex terminal range rejected: NoRange");
        }
        (done, admitted.ok(), true)
    }
}

fn admit_codex_tui_terminal_range_in_root(
    root: &Path,
    local: &mut InflightTurnState,
    persisted_baseline: &mut InflightTurnState,
    expected: &InflightTurnIdentity,
    can_chain_locally: bool,
    session_id: Option<&str>,
    rollout_path: &str,
    tmux_session_name: &str,
    turn_nonce: &str,
    source_start: u64,
    complete_record_end: u64,
    caller: &'static str,
) -> Result<AdmittedCodexTerminalRange, GuardedSaveOutcome> {
    let session_id = nonempty(session_id).ok_or(GuardedSaveOutcome::IdentityMismatch)?;
    let tmux_session_name =
        nonempty(Some(tmux_session_name)).ok_or(GuardedSaveOutcome::IdentityMismatch)?;
    let turn_nonce = nonempty(Some(turn_nonce)).ok_or(GuardedSaveOutcome::IdentityMismatch)?;
    if !can_chain_locally
        || local.provider_kind() != Some(ProviderKind::Codex)
        || local.runtime_kind != Some(RuntimeHandoffKind::CodexTui)
        || !StreamRelayAuthority::from_state(local).bridge_owns_relay()
        || complete_record_end <= source_start
    {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    let path = inflight_state_path(root, &ProviderKind::Codex, local.channel_id);
    let _lock = lock_inflight_state_path(&path).map_err(|_| GuardedSaveOutcome::IoError)?;
    let mut fresh = read_inflight_state_for_guarded_write(
        &path,
        &ProviderKind::Codex,
        local.channel_id,
        expected,
        caller,
    )?;
    if !expected.matches_state(&fresh)
        || fresh.provider_kind() != Some(ProviderKind::Codex)
        || fresh.runtime_kind != Some(RuntimeHandoffKind::CodexTui)
        || fresh.tmux_session_name.as_deref() != Some(tmux_session_name)
        || nonempty(fresh.turn_nonce.as_deref()) != Some(turn_nonce)
        || fresh.turn_start_offset != Some(source_start)
        || fresh.last_offset != source_start
        || fresh.restart_mode.is_some()
        || fresh.rebind_origin
        || fresh.terminal_delivery_committed
        || !StreamRelayAuthority::from_state(&fresh).bridge_owns_relay()
    {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    let (canonical_rollout, file_len) =
        canonical_regular_file(rollout_path).ok_or(GuardedSaveOutcome::IdentityMismatch)?;
    if file_len != complete_record_end
        || !marker_matches_terminal_range(
            tmux_session_name,
            &canonical_rollout,
            session_id,
            source_start,
        )
    {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    let cold_provisional = source_start == 0
        && fresh.output_path.as_deref()
            == Some(
                crate::services::discord::turn_bridge::tmux_runtime_paths(tmux_session_name)
                    .0
                    .as_str(),
            );
    let warm_exact = fresh
        .output_path
        .as_deref()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .as_deref()
        == Some(canonical_rollout.as_path())
        && binding_matches_terminal_range(
            tmux_session_name,
            &canonical_rollout,
            session_id,
            source_start,
        );
    let generation_mtime_ns =
        crate::services::discord::tmux::read_generation_file_mtime_ns(tmux_session_name);
    if (!cold_provisional && !warm_exact) || generation_mtime_ns == 0 {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }

    let canonical_rollout = canonical_rollout.display().to_string();
    fresh.output_path = Some(canonical_rollout.clone());
    fresh.last_offset = complete_record_end;
    let persisted = persist_under_lock_with_snapshot(
        root,
        &path,
        &fresh,
        "src/services/discord/inflight/save_store/identity_gate/runtime_stamp.rs:admit_codex_tui_terminal_range",
    )
    .map_err(|_| GuardedSaveOutcome::IoError)?
    .ok_or(GuardedSaveOutcome::IdentityMismatch)?;
    persisted_baseline.clone_from(&persisted);
    local.output_path.clone_from(&persisted.output_path);
    local.last_offset = persisted.last_offset;
    local.save_generation = persisted.save_generation;
    Ok(AdmittedCodexTerminalRange {
        channel_id: persisted.channel_id,
        identity: InflightTurnIdentity::from_state(&persisted),
        rollout_path: canonical_rollout,
        session_id: session_id.to_string(),
        tmux_session_name: tmux_session_name.to_string(),
        turn_nonce: turn_nonce.to_string(),
        source_start,
        complete_record_end,
        generation_mtime_ns,
    })
}

impl AdmittedCodexTerminalRange {
    pub(in crate::services::discord) fn revalidated_end(&self) -> Option<u64> {
        let root = inflight_runtime_root()?;
        let path = inflight_state_path(&root, &ProviderKind::Codex, self.channel_id);
        let _lock = lock_inflight_state_path(&path).ok()?;
        let fresh = read_inflight_state_for_guarded_write(
            &path,
            &ProviderKind::Codex,
            self.channel_id,
            &self.identity,
            "turn_bridge::stream_loop::codex_terminal_range_exit",
        )
        .ok()?;
        let (canonical_rollout, file_len) = canonical_regular_file(&self.rollout_path)?;
        (self.identity.matches_state(&fresh)
            && fresh.runtime_kind == Some(RuntimeHandoffKind::CodexTui)
            && fresh.tmux_session_name.as_deref() == Some(self.tmux_session_name.as_str())
            && nonempty(fresh.turn_nonce.as_deref()) == Some(self.turn_nonce.as_str())
            && fresh.turn_start_offset == Some(self.source_start)
            && fresh.output_path.as_deref() == Some(self.rollout_path.as_str())
            && fresh.last_offset == self.complete_record_end
            && fresh.restart_mode.is_none()
            && !fresh.rebind_origin
            && !fresh.terminal_delivery_committed
            && StreamRelayAuthority::from_state(&fresh).bridge_owns_relay()
            && file_len == self.complete_record_end
            && marker_matches_terminal_range(
                &self.tmux_session_name,
                &canonical_rollout,
                &self.session_id,
                self.source_start,
            )
            && binding_matches_terminal_range(
                &self.tmux_session_name,
                &canonical_rollout,
                &self.session_id,
                self.complete_record_end,
            )
            && self.generation_mtime_ns != 0
            && crate::services::discord::tmux::read_generation_file_mtime_ns(
                &self.tmux_session_name,
            ) == self.generation_mtime_ns)
            .then_some(self.complete_record_end)
    }
}

pub(in crate::services::discord) fn stamp_runtime_handoff_if_matches_identity<
    T: GuardedStampTarget,
>(
    state: T,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    stamp_runtime_handoff_if_matches_identity_in_root(&root, state, expected, caller)
}

pub(in crate::services::discord::inflight) fn stamp_runtime_handoff_if_matches_identity_in_root<
    T: GuardedStampTarget,
>(
    root: &Path,
    state: T,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let requested = InflightTurnState::clone(state.local_state());
    let baseline = state.baseline_state().cloned();
    let Some(provider) = requested.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, requested.channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let mut on_disk = match read_inflight_state_for_guarded_write(
        &path,
        &provider,
        requested.channel_id,
        expected,
        caller,
    ) {
        Ok(on_disk) => on_disk,
        Err(outcome) => return outcome,
    };
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "runtime-handoff stamp skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "runtime-handoff stamp skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.tmux_session_name.is_some()
        && requested.tmux_session_name != expected.tmux_session_name
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            snapshot_tmux_session_name = ?expected.tmux_session_name,
            requested_tmux_session_name = ?requested.tmux_session_name,
            "runtime-handoff stamp skipped because an established runtime session changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    if !merge_runtime_stamp_progress(&mut on_disk, &requested) {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            "runtime-handoff stamp rejected because local and durable responses diverged"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let requested_runtime = (
        requested.runtime_kind,
        &requested.tmux_session_name,
        &requested.output_path,
        &requested.input_fifo_path,
        &requested.session_id,
    );
    let durable_runtime = (
        on_disk.runtime_kind,
        &on_disk.tmux_session_name,
        &on_disk.output_path,
        &on_disk.input_fifo_path,
        &on_disk.session_id,
    );
    let requested_owner = (
        requested.watcher_owner_channel_id,
        requested.watcher_owns_live_relay,
        requested.relay_owner_kind,
    );
    let durable_owner = (
        on_disk.watcher_owner_channel_id,
        on_disk.watcher_owns_live_relay,
        on_disk.relay_owner_kind,
    );
    let (apply_runtime, apply_owner) = if let Some(baseline) = baseline.as_ref() {
        let baseline_runtime = (
            baseline.runtime_kind,
            &baseline.tmux_session_name,
            &baseline.output_path,
            &baseline.input_fifo_path,
            &baseline.session_id,
        );
        let baseline_owner = (
            baseline.watcher_owner_channel_id,
            baseline.watcher_owns_live_relay,
            baseline.relay_owner_kind,
        );
        let runtime_changed = requested_runtime != baseline_runtime;
        let owner_changed = requested_owner != baseline_owner;
        if runtime_changed
            && durable_runtime != baseline_runtime
            && durable_runtime != requested_runtime
        {
            return GuardedSaveOutcome::IdentityMismatch;
        }
        if owner_changed && durable_owner != baseline_owner && durable_owner != requested_owner {
            return GuardedSaveOutcome::IdentityMismatch;
        }
        (runtime_changed, owner_changed)
    } else {
        (true, true)
    };

    if apply_runtime {
        on_disk.runtime_kind = requested.runtime_kind;
        on_disk
            .tmux_session_name
            .clone_from(&requested.tmux_session_name);
        on_disk.output_path.clone_from(&requested.output_path);
        on_disk
            .input_fifo_path
            .clone_from(&requested.input_fifo_path);
        on_disk.session_id.clone_from(&requested.session_id);
    }
    if apply_owner {
        on_disk.watcher_owner_channel_id = requested.watcher_owner_channel_id;
        on_disk.watcher_owns_live_relay = requested.watcher_owns_live_relay;
        on_disk.relay_owner_kind = requested.relay_owner_kind;
    }
    match persist_under_lock_with_snapshot(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:stamp_runtime_handoff_if_matches_identity_in_root",
    ) {
        Ok(Some(persisted)) => {
            state.adopt_persisted(persisted);
            GuardedSaveOutcome::Saved
        }
        Ok(None) => GuardedSaveOutcome::IdentityMismatch,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = requested.channel_id,
                caller,
                error = %error,
                "runtime-handoff stamp failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_seed(
        provider: ProviderKind,
        channel_id: u64,
        tmux_session_name: Option<&str>,
    ) -> InflightTurnState {
        InflightTurnState::new(
            provider,
            channel_id,
            Some("adk-4259-r2".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "runtime handoff".to_string(),
            Some("provider-session-before-handoff".to_string()),
            tmux_session_name.map(str::to_string),
            Some("/seeded/runtime-output.jsonl".to_string()),
            None,
            512,
        )
    }

    fn load(root: &Path, provider: &ProviderKind, channel_id: u64) -> InflightTurnState {
        let path = inflight_state_path(root, provider, channel_id);
        serde_json::from_str(&std::fs::read_to_string(path).expect("read inflight row"))
            .expect("parse inflight row")
    }

    #[test]
    fn codex_terminal_range_cold_admission_and_exit_owner_5264() {
        let temp = tempfile::tempdir().unwrap();
        let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let root = inflight_runtime_root().unwrap();
        let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmux = "AgentDesk-codex-range-5264";
        let rollout = temp.path().join("rollout.jsonl");
        std::fs::write(&rollout, b"{}\n").unwrap();
        let end = std::fs::metadata(&rollout).unwrap().len();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker_with_start_offset(
            tmux,
            &rollout,
            Some("raw-session"),
            Some(0),
        )
        .unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(tmux, "generation"),
            b"1",
        )
        .unwrap();
        let mut local = runtime_seed(ProviderKind::Codex, 52_640_001, Some(tmux));
        local.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        local.turn_nonce = Some("turn-nonce".into());
        local.turn_start_offset = Some(0);
        local.last_offset = 0;
        local.output_path = Some(crate::services::discord::turn_bridge::tmux_runtime_paths(tmux).0);
        save_inflight_state_in_root(&root, &local).unwrap();
        let expected = InflightTurnIdentity::from_state(&local);
        let mut baseline = local.clone();
        let latch = admit_codex_tui_terminal_range_in_root(
            &root,
            &mut local,
            &mut baseline,
            &expected,
            true,
            Some("raw-session"),
            rollout.to_str().unwrap(),
            tmux,
            "turn-nonce",
            0,
            end,
            "test::cold",
        )
        .unwrap();
        let mut persisted = load(&root, &ProviderKind::Codex, local.channel_id);
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: rollout.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("raw-session".into()),
                last_offset: end,
                relay_last_offset: None,
            },
        );
        assert_eq!(latch.revalidated_end(), Some(end));
        persisted.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(&root, &persisted).unwrap();
        assert_eq!(latch.revalidated_end(), None);
    }

    #[test]
    fn runtime_first_stamp_supports_process_claude_tui_and_codex_tui() {
        for (index, provider, runtime_kind, session_name) in [
            (
                0,
                ProviderKind::Claude,
                RuntimeHandoffKind::ProcessBackend,
                "claude-process-session",
            ),
            (
                1,
                ProviderKind::Claude,
                RuntimeHandoffKind::ClaudeTui,
                "AgentDesk-claude-adk-4259-r2",
            ),
            (
                2,
                ProviderKind::Codex,
                RuntimeHandoffKind::CodexTui,
                "AgentDesk-codex-adk-4259-r2",
            ),
        ] {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = 42_592_100 + index;
            let seed = runtime_seed(provider.clone(), channel_id, None);
            save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
            let expected = InflightTurnIdentity::from_state(&seed);

            let mut stamp = seed.clone();
            stamp.runtime_kind = Some(runtime_kind);
            stamp.tmux_session_name = Some(session_name.to_string());
            stamp.output_path = Some(format!("/runtime/{session_name}.jsonl"));
            stamp.input_fifo_path = matches!(runtime_kind, RuntimeHandoffKind::ClaudeTui)
                .then(|| format!("/runtime/{session_name}.input"));
            stamp.session_id = Some(format!("provider-session-{index}"));
            stamp.last_offset = 4096;
            stamp.watcher_owner_channel_id = Some(channel_id + 100);
            stamp.set_relay_owner_kind(RelayOwnerKind::Watcher);

            assert_eq!(
                stamp_runtime_handoff_if_matches_identity_in_root(
                    root.path(),
                    &stamp,
                    &expected,
                    "test::runtime_first_stamp",
                ),
                GuardedSaveOutcome::Saved,
            );
            let persisted = load(root.path(), &provider, channel_id);
            assert_eq!(persisted.runtime_kind, Some(runtime_kind));
            assert_eq!(persisted.tmux_session_name.as_deref(), Some(session_name));
            assert_eq!(persisted.output_path, stamp.output_path);
            assert_eq!(persisted.input_fifo_path, stamp.input_fifo_path);
            assert_eq!(persisted.session_id, stamp.session_id);
            assert_eq!(persisted.last_offset, 4096);
            assert_eq!(persisted.watcher_owner_channel_id, Some(channel_id + 100));
            assert_eq!(
                persisted.effective_relay_owner_kind(),
                RelayOwnerKind::Watcher
            );
        }
    }

    #[test]
    fn claude_tui_runtime_stamp_accepts_none_to_projects_output_path() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Claude;
        let channel_id = 42_592_150;
        let mut seed = runtime_seed(provider.clone(), channel_id, Some("AgentDesk-claude-4997"));
        seed.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        seed.output_path = None;
        seed.input_fifo_path = Some("/runtime/claude-4997.input".to_string());
        save_inflight_state_in_root(root.path(), &seed).expect("seed ClaudeTui row");
        let expected = InflightTurnIdentity::from_state(&seed);

        let mut handoff = seed.clone();
        handoff.output_path = Some("/projects/claude-4997.jsonl".to_string());
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &handoff,
                &expected,
                "test::claude_tui_none_to_projects_output",
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted = load(root.path(), &provider, channel_id);
        assert_eq!(
            persisted.output_path.as_deref(),
            Some("/projects/claude-4997.jsonl")
        );
        assert_eq!(persisted.input_fifo_path, seed.input_fifo_path);
    }

    #[test]
    fn runtime_stamp_accepts_same_session_restamp_and_rejects_changed_session() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Codex;
        let channel_id = 42_592_200;
        let seed = runtime_seed(provider.clone(), channel_id, Some("AgentDesk-codex-stable"));
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&seed);

        let mut same_session = seed.clone();
        same_session.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        same_session.output_path = Some("/runtime/restamped-rollout.jsonl".to_string());
        same_session.last_offset = 2048;
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &same_session,
                &expected,
                "test::same_session_restamp",
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted = load(root.path(), &provider, channel_id);
        let persisted_expected = InflightTurnIdentity::from_state(&persisted);
        let mut changed_session = persisted.clone();
        changed_session.tmux_session_name = Some("AgentDesk-codex-different".to_string());
        changed_session.output_path = Some("/runtime/should-not-land.jsonl".to_string());
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &changed_session,
                &persisted_expected,
                "test::changed_session_rejected",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let preserved = load(root.path(), &provider, channel_id);
        assert_eq!(
            preserved.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-stable")
        );
        assert_eq!(
            preserved.output_path.as_deref(),
            Some("/runtime/restamped-rollout.jsonl")
        );
    }

    #[test]
    fn runtime_stamp_never_creates_or_overwrites_unowned_rows() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Claude;
        let channel_id = 42_592_300;
        let seed = runtime_seed(provider.clone(), channel_id, None);
        let expected = InflightTurnIdentity::from_state(&seed);
        let mut stamp = seed.clone();
        stamp.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        stamp.tmux_session_name = Some("AgentDesk-claude-r2".to_string());

        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &stamp,
                &expected,
                "test::missing_row",
            ),
            GuardedSaveOutcome::Missing,
        );

        let mut newer = seed.clone();
        newer.user_msg_id = 99_999;
        newer.output_path = Some("/runtime/newer-turn.jsonl".to_string());
        save_inflight_state_in_root(root.path(), &newer).expect("seed re-owned row");
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &stamp,
                &expected,
                "test::concurrent_reowner",
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let preserved = load(root.path(), &provider, channel_id);
        assert_eq!(preserved.user_msg_id, 99_999);
        assert_eq!(
            preserved.output_path.as_deref(),
            Some("/runtime/newer-turn.jsonl")
        );
    }

    #[test]
    fn runtime_stamp_fails_closed_for_ambiguous_or_reserved_authority() {
        let provider = ProviderKind::Codex;
        for (index, mutate) in ["id0", "restart", "rebind"].into_iter().enumerate() {
            let root = tempfile::tempdir().expect("runtime root");
            let channel_id = 42_592_400 + index as u64;
            let mut seed = runtime_seed(provider.clone(), channel_id, None);
            match mutate {
                "id0" => {
                    seed.user_msg_id = 0;
                    seed.turn_start_offset = None;
                }
                "restart" => seed.set_restart_mode(InflightRestartMode::DrainRestart),
                "rebind" => seed.rebind_origin = true,
                _ => unreachable!(),
            }
            save_inflight_state_in_root(root.path(), &seed).expect("seed reserved row");
            let expected = InflightTurnIdentity::from_state(&seed);
            let mut stamp = seed.clone();
            stamp.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
            stamp.tmux_session_name = Some("AgentDesk-codex-r2".to_string());

            assert_eq!(
                stamp_runtime_handoff_if_matches_identity_in_root(
                    root.path(),
                    &stamp,
                    &expected,
                    "test::reserved_authority",
                ),
                GuardedSaveOutcome::IdentityMismatch,
                "{mutate} authority must fail closed",
            );
            let preserved = load(root.path(), &provider, channel_id);
            assert_eq!(preserved.runtime_kind, seed.runtime_kind);
            assert_eq!(preserved.tmux_session_name, seed.tmux_session_name);
        }
    }

    #[test]
    fn runtime_stamp_commits_only_final_standby_or_watcher_owner_decision() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Claude;
        let channel_id = 42_592_500;
        let seed = runtime_seed(provider.clone(), channel_id, None);
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let expected = InflightTurnIdentity::from_state(&seed);

        let mut standby = seed.clone();
        standby.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        standby.tmux_session_name = Some("AgentDesk-claude-owner-r2".to_string());
        standby.set_relay_owner_kind(RelayOwnerKind::StandbyRelay);
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &standby,
                &expected,
                "test::standby_owner",
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted_standby = load(root.path(), &provider, channel_id);
        assert_eq!(
            persisted_standby.effective_relay_owner_kind(),
            RelayOwnerKind::StandbyRelay
        );

        let standby_expected = InflightTurnIdentity::from_state(&persisted_standby);
        let mut watcher = persisted_standby.clone();
        watcher.set_relay_owner_kind(RelayOwnerKind::Watcher);
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                &watcher,
                &standby_expected,
                "test::watcher_owner",
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted_watcher = load(root.path(), &provider, channel_id);
        assert_eq!(
            persisted_watcher.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
    }

    #[test]
    fn runtime_stamp_preserves_concurrent_progress_and_adopts_exact_persisted_row() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Codex;
        let channel_id = 42_592_550;
        let seed = runtime_seed(provider.clone(), channel_id, None);
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let baseline = load(root.path(), &provider, channel_id);
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut durable_progress = baseline.clone();
        durable_progress.current_msg_id = 800_001;
        durable_progress.current_msg_len = 37;
        durable_progress.full_response = "watcher response".to_string();
        durable_progress.response_sent_offset = durable_progress.full_response.len();
        durable_progress.current_tool_line = Some("watcher tool".to_string());
        durable_progress.any_tool_used = true;
        durable_progress.watcher_owner_channel_id = Some(channel_id + 1);
        durable_progress.set_relay_owner_kind(RelayOwnerKind::Watcher);
        save_inflight_state_in_root(root.path(), &durable_progress)
            .expect("advance same-turn durable progress");
        let durable_progress = load(root.path(), &provider, channel_id);

        let mut local = baseline.clone();
        local.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        local.tmux_session_name = Some("AgentDesk-codex-r7-exact".to_string());
        local.output_path = Some("/runtime/r7-exact.jsonl".to_string());
        local.last_offset = 4_096;
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                (&baseline, &mut local),
                &expected,
                "test::runtime_exact_adoption",
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted = load(root.path(), &provider, channel_id);
        assert_eq!(
            serde_json::to_value(&local).expect("serialize adopted local row"),
            serde_json::to_value(&persisted).expect("serialize persisted row"),
        );
        assert!(persisted.save_generation > durable_progress.save_generation);
        assert_eq!(persisted.current_msg_id, 800_001);
        assert_eq!(persisted.full_response, "watcher response");
        assert_eq!(persisted.current_tool_line.as_deref(), Some("watcher tool"));
        assert_eq!(persisted.watcher_owner_channel_id, Some(channel_id + 1));
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        assert_eq!(persisted.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        assert_eq!(
            persisted.tmux_session_name.as_deref(),
            Some("AgentDesk-codex-r7-exact")
        );
        assert_eq!(persisted.last_offset, 4_096);
    }

    #[test]
    fn transient_runtime_stamp_read_error_is_retryable_and_preserves_local_frame() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Codex;
        let channel_id = 42_593_121;
        let seed = runtime_seed(provider.clone(), channel_id, None);
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let baseline = load(root.path(), &provider, channel_id);
        let expected = InflightTurnIdentity::from_state(&baseline);
        let mut local = baseline.clone();
        local.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        local.tmux_session_name = Some("AgentDesk-codex-r9-retry".to_string());
        local.output_path = Some("/runtime/r9-retry.jsonl".to_string());
        let local_before = serde_json::to_value(&local).expect("serialize local frame");

        let path = inflight_state_path(root.path(), &provider, channel_id);
        std::fs::remove_file(&path).expect("replace row with deterministic read failure");
        std::fs::create_dir(&path).expect("directory at row path forces read error");
        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                (&baseline, &mut local),
                &expected,
                "test::transient_runtime_stamp_read_error",
            ),
            GuardedSaveOutcome::IoError,
        );
        assert_eq!(
            serde_json::to_value(&local).expect("serialize retained local frame"),
            local_before,
            "retryable guarded-read failure must not adopt or mutate local handoff identity",
        );
    }

    #[test]
    fn divergent_runtime_response_is_non_retryable_and_preserves_both_snapshots() {
        let root = tempfile::tempdir().expect("runtime root");
        let provider = ProviderKind::Codex;
        let channel_id = 42_593_122;
        let mut seed = runtime_seed(provider.clone(), channel_id, None);
        seed.full_response = "shared base".to_string();
        save_inflight_state_in_root(root.path(), &seed).expect("seed owner row");
        let baseline = load(root.path(), &provider, channel_id);
        let expected = InflightTurnIdentity::from_state(&baseline);

        let mut durable = baseline.clone();
        durable.full_response = "durable watcher branch".to_string();
        save_inflight_state_in_root(root.path(), &durable).expect("persist divergent durable row");
        let durable_before = load(root.path(), &provider, channel_id);
        let mut local = baseline.clone();
        local.full_response = "resolved terminal branch".to_string();
        local.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        let local_before = serde_json::to_value(&local).expect("serialize local frame");

        assert_eq!(
            stamp_runtime_handoff_if_matches_identity_in_root(
                root.path(),
                (&baseline, &mut local),
                &expected,
                "test::divergent_runtime_response",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "semantic body divergence must not enter the transient-I/O retry loop",
        );
        assert_eq!(serde_json::to_value(&local).unwrap(), local_before);
        assert_eq!(
            serde_json::to_value(load(root.path(), &provider, channel_id)).unwrap(),
            serde_json::to_value(durable_before).unwrap(),
        );
    }
}
