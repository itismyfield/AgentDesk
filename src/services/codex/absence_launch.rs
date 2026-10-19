use super::*;
use crate::services::discord::codex_absence::InflightObs;
use crate::services::tui_prompt_dedupe::clear_relaunch;

#[cfg(test)]
pub(crate) type AdmissionObserver =
    Box<dyn FnOnce(Option<&InflightObs>, Option<&std::sync::Arc<CancelToken>>)>;

#[cfg(test)]
pub(crate) type InitialCaptureObserver = Box<dyn FnOnce(bool)>;

#[cfg(test)]
thread_local! {
    pub(super) static AFTER_INITIAL_CAPTURE: std::cell::RefCell<Option<InitialCaptureObserver>> = const { std::cell::RefCell::new(None) };
    pub(super) static ADMISSION_OBSERVER: std::cell::RefCell<Option<AdmissionObserver>> = const { std::cell::RefCell::new(None) };
}

enum LaunchReason {
    AbsentVerified(Box<clear_relaunch::Captured>),
    ExistingRail,
}

impl LaunchReason {
    fn captured(&self) -> Option<&clear_relaunch::Captured> {
        match self {
            Self::AbsentVerified(captured) => Some(captured.as_ref()),
            Self::ExistingRail => None,
        }
    }

    fn preserves_existing(&self, tmux: &str) -> bool {
        matches!(self, Self::ExistingRail) && verified_hold::existing_incarnation(tmux)
    }
}

pub(super) struct LocalTuiTurn<'a> {
    pub prompt: &'a str,
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub fast_mode_enabled: Option<bool>,
    pub goals_enabled: Option<bool>,
    pub working_dir: &'a str,
    pub sender: Sender<StreamMessage>,
    pub cancel_token: Option<std::sync::Arc<CancelToken>>,
    pub tmux_session_name: &'a str,
    pub teardown: Option<&'a TeardownClearance>,
    pub report_channel_id: Option<u64>,
    pub report_provider: Option<ProviderKind>,
    pub developer_instructions: Option<&'a str>,
    pub readonly_mode: bool,
    pub compact_token_limit: Option<u64>,
    pub force_fresh_provider_session: bool,
}

pub(super) fn execute_observed(
    turn: LocalTuiTurn<'_>,
    observation: Option<&InflightObs>,
) -> Result<(), String> {
    let LocalTuiTurn {
        prompt,
        session_id,
        model,
        fast_mode_enabled,
        goals_enabled,
        working_dir,
        sender,
        cancel_token,
        tmux_session_name,
        teardown,
        report_channel_id,
        report_provider,
        developer_instructions,
        readonly_mode,
        compact_token_limit,
        force_fresh_provider_session,
    } = turn;
    // A Herdr channel is refused before the turn lock, warm follow-up or any session cleanup.
    if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) {
        return Err(crate::services::herdr_launch::HERDR_NOT_ADMITTED.to_string());
    }
    let observation = observation.filter(|observed| {
        cancel_token
            .as_ref()
            .is_some_and(|token| observed.matches_token(token))
    });
    #[cfg(test)]
    ADMISSION_OBSERVER.with_borrow_mut(|slot| {
        if let Some(observer) = slot.take() {
            observer(observation, cancel_token.as_ref());
        }
    });
    let captured = (!clear_relaunch::relaunch_open(tmux_session_name))
        .then(|| {
            observation
                .and_then(|observed| clear_relaunch::absent_capture(tmux_session_name, observed))
        })
        .flatten();
    #[cfg(test)]
    AFTER_INITIAL_CAPTURE.with_borrow_mut(|slot| {
        if let Some(effect) = slot.take() {
            effect(captured.is_some());
        }
    });
    // Only the initial capture selects absence; later checks may only revoke that reason.
    let reason = captured.map_or(LaunchReason::ExistingRail, |captured| {
        LaunchReason::AbsentVerified(Box::new(captured))
    });
    let captured = reason.captured();
    let inherited_verified = captured.is_some();
    let session_id = if inherited_verified { None } else { session_id };
    if reason.preserves_existing(tmux_session_name)
        && !force_fresh_provider_session
        && crate::services::codex_tui::warm_followup::codex_tui_warm_followup_enabled()
        && let (Some(session_id), Some(channel), Some(token)) =
            (session_id, report_channel_id, cancel_token.as_ref())
    {
        return crate::services::codex_tui::warm_followup::verified_warm(
            crate::services::codex_tui::warm_followup::VerifiedWarmRequest {
                tmux: tmux_session_name,
                channel,
                session_id,
                cwd: std::path::Path::new(working_dir),
                prompt,
                options: &CodexLaunchOptions::new(prompt)
                    .with_resume_session_id(Some(session_id))
                    .with_developer_instructions(developer_instructions)
                    .with_model(model)
                    .with_reasoning_effort(codex_reasoning_effort_from_env().as_deref())
                    .with_compact_token_limit(compact_token_limit)
                    .with_readonly_mode(readonly_mode)
                    .with_fast_mode_enabled(fast_mode_enabled)
                    .with_goals_enabled(goals_enabled)
                    .with_cwd(Some(working_dir)),
                token: std::sync::Arc::clone(token),
                sender,
            },
        );
    }
    let preflight = match &reason {
        LaunchReason::AbsentVerified(_) => None,
        LaunchReason::ExistingRail => {
            #[cfg(test)]
            {
                let reopened = crate::services::claude_tui::source_verify::n2b_mutant("b1b-reason")
                    && observation.is_some_and(|observed| {
                        clear_relaunch::absent_capture(tmux_session_name, observed).is_some()
                    });
                if reopened {
                    None
                } else {
                    verified_hold::preflight(tmux_session_name, cancel_token.as_ref())
                }
            }
            #[cfg(not(test))]
            {
                verified_hold::preflight(tmux_session_name, cancel_token.as_ref())
            }
        }
    };
    let warm_followup_enabled = match preflight {
        Some(result) => return result,
        None => crate::services::codex_tui::warm_followup::codex_tui_warm_followup_enabled(),
    };
    let turn_lock = warm_followup_enabled.then(|| codex_tui_session_turn_lock(tmux_session_name));
    let _turn_guard = turn_lock
        .as_ref()
        .map(|lock| lock.lock().unwrap_or_else(|error| error.into_inner()));
    let auth_overlay = crate::services::discord::org_schema::overlay_from_tmux_session(
        ProviderKind::Codex,
        tmux_session_name,
    )?;
    let session_exists = tmux_present_bool(tmux_session_name);
    let profile_matches = crate::services::tmux_common::tmux_session_auth_profile_matches(
        tmux_session_name,
        &auth_overlay.profile_id,
    );
    let session_id = profile_matches.then_some(session_id).flatten();
    let session_selection = crate::services::codex_tui::session::resolve_codex_tui_session(
        session_id,
        std::path::Path::new(working_dir),
        None,
        force_fresh_provider_session,
    );
    let reasoning_effort = codex_reasoning_effort_from_env();
    let launch_options = CodexLaunchOptions::new(prompt)
        .with_resume_session_id(session_selection.resume_session_id())
        .with_developer_instructions(developer_instructions)
        .with_model(model)
        .with_reasoning_effort(reasoning_effort.as_deref())
        .with_compact_token_limit(compact_token_limit)
        .with_readonly_mode(readonly_mode)
        .with_fast_mode_enabled(fast_mode_enabled)
        .with_goals_enabled(goals_enabled)
        .with_cwd(Some(working_dir));
    let (prepared, resume_root) = tui_session_launch::preflight_codex_tui_turn_with_policy(
        tmux_session_name,
        report_channel_id,
        &launch_options,
        &session_selection,
        (session_exists, force_fresh_provider_session),
        &auth_overlay,
        inherited_verified,
    )
    .map_err(|error| error.to_string())?;
    if let Some(fallback_reason) = direct_tui_material_fallback_reason(&launch_options) {
        tracing::warn!(
            provider = "codex",
            unsupported_options = fallback_reason,
            tmux_session_name,
            "codex direct TUI launch options are unsupported; falling back to wrapper"
        );
        return execute_streaming_local_tmux(
            prompt,
            model,
            fast_mode_enabled,
            goals_enabled,
            session_id,
            working_dir,
            sender,
            cancel_token,
            tmux_session_name,
            teardown,
            report_channel_id,
            report_provider,
            developer_instructions,
            compact_token_limit,
            force_fresh_provider_session,
        );
    }

    let has_live_pane = tmux_live_pane_bool(tmux_session_name) && profile_matches;
    let mut warm_fallback_reason = None;
    let mut warm_fallback_pane_stopped = false;

    // Disabled warm reuse keeps the existing cleanup and relaunch path.
    if warm_followup_enabled && prepared.is_none() {
        match crate::services::codex_tui::warm_followup::try_codex_tui_warm_followup(
            &session_selection,
            &launch_options,
            force_fresh_provider_session,
            session_exists,
            has_live_pane,
            prompt,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
            report_channel_id,
        ) {
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::Terminal(
                result,
            ) => return result,
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::Fallback(
                reason,
            ) => warm_fallback_reason = Some(reason),
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::FallbackAfterPaneKill(
                reason,
            ) => {
                warm_fallback_reason = Some(reason);
                warm_fallback_pane_stopped = true;
            }
            crate::services::codex_tui::warm_followup::CodexWarmFollowupOutcome::LegacyPath => {}
        }
    }

    tui_session_launch::tui_preflight_with_policy(
        tmux_session_name,
        report_channel_id,
        &launch_options,
        resume_root.as_deref(),
        inherited_verified,
    )
    .map_err(|error| error.to_string())?;
    if session_exists {
        cleanup_existing_codex_tui_session(
            tmux_session_name,
            force_fresh_provider_session,
            has_live_pane,
            warm_fallback_reason,
            warm_fallback_pane_stopped,
        );
    }

    let cleaned =
        crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
            if captured.is_some_and(|captured| {
                !observation.is_some_and(|observed| {
                    clear_relaunch::absent_matches_under(authority, observed, captured)
                })
            }) {
                return false;
            }
            #[cfg(test)]
            actual_submit_tests::note_dead_temp_cleanup(tmux_session_name, "session_temp");
            crate::services::tmux_common::cleanup_session_temp_files_under_source_authority(
                authority,
                tmux_session_name,
            );
            #[cfg(test)]
            actual_submit_tests::note_dead_temp_cleanup(tmux_session_name, "codex_session_files");
            crate::services::codex_tui::session::CodexTuiSessionFiles::for_tmux_session(
                tmux_session_name,
            )
            .cleanup_best_effort();
            true
        });
    if !cleaned {
        return verified_hold::wait_for_cancel(tmux_session_name, cancel_token.as_ref());
    }

    let prepared = match prepared {
        Some(prepared) => prepared,
        None => tui_session_launch::prepare_codex_tui_incarnation_with_policy(
            tmux_session_name,
            report_channel_id,
            &launch_options,
            &auth_overlay,
            resume_root.as_deref(),
            inherited_verified,
        )
        .map_err(|error| error.to_string())?,
    };

    let CodexTuiLaunchScript {
        prepared,
        script_path,
        owner_path,
        rollout_modified_since,
        handoff: _handoff,
    } = publish_codex_tui_launch_script(
        tmux_session_name,
        session_id,
        prompt,
        &launch_options,
        (report_channel_id, report_provider),
        warm_followup_enabled,
        (&auth_overlay, prepared),
    )?;
    if let Some(channel_id) = report_channel_id {
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux_session_name, channel_id);
    }

    #[cfg(test)]
    actual_submit_tests::run_before_create_effect();
    if captured.is_some_and(|captured| {
        !observation.is_some_and(|observed| {
            clear_relaunch::absent_matches(tmux_session_name, observed, captured)
        })
    }) {
        return verified_hold::wait_for_cancel(tmux_session_name, cancel_token.as_ref());
    }
    tui_session_launch::tui_preflight_with_policy(
        tmux_session_name,
        report_channel_id,
        &launch_options,
        resume_root.as_deref(),
        inherited_verified,
    )
    .map_err(|error| error.to_string())?;
    let tmux_result = crate::services::platform::tmux::create_session(
        tmux_session_name,
        Some(working_dir),
        &format!("bash {}", shell_escape(&script_path)),
    )?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        remove_failed_tui_launch_files(&owner_path, &script_path);
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            ProviderKind::Codex.as_str(),
            tmux_session_name,
            prompt,
        );
        return Err(format!("tmux error: {}", stderr));
    }

    crate::services::tmux_common::write_tmux_session_auth_profile(
        tmux_session_name,
        &auth_overlay.profile_id,
    )?;

    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // Before the spawn marker: the marker is restart evidence that these checks passed.
    let validated_tail = crate::services::codex_tui::tail_launch::validate_selection(
        &prepared.context,
        &session_selection,
    )?;
    prepared.finish_spawn(crate::services::discord::stamp_spawn_markers(
        tmux_session_name,
        Some(&prepared),
    ))?;

    wire_cancel_token_to_tmux_session(cancel_token.as_ref(), tmux_session_name);

    // Tail dispatch owns one token clone; post-tail completion and failures share another.
    let cancel_token_for_post_tail = cancel_token.clone();
    crate::services::codex_tui::tail_launch::record_resume_proof(
        &prepared.context,
        &validated_tail,
    )?;
    let tail_result = crate::services::codex_tui::tail_launch::dispatch(
        validated_tail,
        working_dir,
        rollout_modified_since,
        sender.clone(),
        cancel_token,
        tmux_session_name,
        prompt,
    );
    #[cfg(test)]
    let tail_result = if actual_submit_tests::take_tail_fault() {
        Err("injected Codex transcript tail failure".to_owned())
    } else {
        tail_result
    };
    let tail_result = match resolve_codex_tui_tail_result(
        tail_result,
        cancel_token_for_post_tail.as_ref(),
        tmux_session_name,
    )? {
        Some(result) => result,
        // Cancel observed before the transcript was discovered: suppress the
        // tail Err and defer tmux cleanup to the cancel path (return Ok(())).
        None => return Ok(()),
    };

    emit_codex_tui_post_tail_handoff(
        tail_result,
        sender,
        cancel_token_for_post_tail,
        tmux_session_name,
    )
}
