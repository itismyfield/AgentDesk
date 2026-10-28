use super::*;

#[cfg(all(test, unix))]
thread_local! {
    static ISOLATED_HERDR_PATH: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
pub(crate) struct IsolatedHerdrPath(Option<String>);

// The real-client fixture controls its execution environment without changing provider resolution.
#[cfg(all(test, unix))]
pub(crate) fn isolated_herdr_path_for_test(root: &std::path::Path) -> IsolatedHerdrPath {
    let root = root.canonicalize().unwrap();
    assert_eq!(root.parent(), Some(std::path::Path::new("/private/tmp")));
    let owner = root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("adk-e-t14-")
        .unwrap();
    assert!(!owner.is_empty() && owner.bytes().all(|c| c.is_ascii_digit()));
    let path = format!("{}:/usr/bin:/bin", root.join("bin").display());
    IsolatedHerdrPath(ISOLATED_HERDR_PATH.with_borrow_mut(|slot| slot.replace(path)))
}

#[cfg(all(test, unix))]
impl Drop for IsolatedHerdrPath {
    fn drop(&mut self) {
        ISOLATED_HERDR_PATH.with_borrow_mut(|slot| *slot = self.0.take());
    }
}

#[cfg(unix)]
pub(in crate::services::codex) fn tui_preflight_with_policy(
    tmux: &str,
    channel: Option<u64>,
    options: &CodexLaunchOptions,
    resume_root: Option<&std::path::Path>,
    inherited_verified: bool,
) -> Result<&'static str, CodexTuiLaunchRefused> {
    let ambient = crate::services::codex_tui::canary::launch_policy(tmux, channel)
        .map_err(CodexTuiLaunchRefused::SourceModeInvalid)?;
    let policy = if inherited_verified {
        "verified"
    } else {
        ambient
    };
    if policy == "verified" {
        verified_eligibility(options, resume_root)?;
    }
    Ok(policy)
}

#[cfg(unix)]
pub(crate) fn successor_hooks_ready() -> bool {
    verified_hook_eligibility(None).is_ok()
}

#[cfg(unix)]
pub(in crate::services::codex) fn prepare_codex_tui_incarnation_with_policy(
    tmux: &str,
    channel: Option<u64>,
    options: &CodexLaunchOptions,
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
    resume_root: Option<&std::path::Path>,
    inherited_verified: bool,
) -> Result<PreparedIncarnation, CodexTuiLaunchRefused> {
    if crate::services::herdr_launch::herdr_configured_for_tui_launch(channel) {
        return Err(CodexTuiLaunchRefused::HerdrNotAdmitted);
    }
    let policy =
        tui_preflight_with_policy(tmux, channel, options, resume_root, inherited_verified)?;
    use sha2::{Digest, Sha256};
    let digest = format!("sha256:{:x}", Sha256::digest(options.prompt.as_bytes()));
    PreparedIncarnation::prepare_pinned(
        "codex",
        tmux,
        channel,
        options.resume_session_id.as_deref(),
        options.resume_session_id.is_some(),
        // Ownership compares canonical rollout paths to this root, so record it unaliased.
        codex_tui_home(overlay)
            .0
            .map(|home| home.canonicalize().unwrap_or(home).join("sessions")),
        (Some(digest), Some(policy.to_owned())),
    )
    .map_err(CodexTuiLaunchRefused::PreparationUnavailable)
}

/// Verified panes cannot use legacy warm input; prepare only the new incarnation.
#[cfg(unix)]
pub(in crate::services::codex) fn preflight_codex_tui_turn_with_policy(
    tmux: &str,
    channel: Option<u64>,
    options: &CodexLaunchOptions,
    selection: &crate::services::codex_tui::session::CodexTuiSessionSelection,
    existing: (bool, bool),
    overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
    inherited_verified: bool,
) -> Result<(Option<PreparedIncarnation>, Option<PathBuf>), CodexTuiLaunchRefused> {
    let exact = selection.candidate_count == 1
        && selection.resume
        && selection.requested_session_id == selection.selected_session_id
        && options.resume_session_id.as_deref() == selection.resume_session_id()
        && selection
            .resume_session_id()
            .is_some_and(exact_codex_resume_id);
    let resume_root = exact
        .then(|| codex_tui_home(overlay).0.map(|home| home.join("sessions")))
        .flatten();
    let policy = tui_preflight_with_policy(
        tmux,
        channel,
        options,
        resume_root.as_deref(),
        inherited_verified,
    )?;
    if policy != "verified" {
        return Ok((None, resume_root));
    }
    if !existing.1 && selection.requested_session_id.is_some() && resume_root.is_none() {
        return Err(CodexTuiLaunchRefused::ResumeUnavailable);
    }
    if existing.0 && !existing.1 && resume_root.is_none() {
        return Err(CodexTuiLaunchRefused::UnprovedPane);
    }
    let prepared = prepare_codex_tui_incarnation_with_policy(
        tmux,
        channel,
        options,
        overlay,
        resume_root.as_deref(),
        inherited_verified,
    )?;
    #[cfg(test)]
    BEFORE_PANE_EFFECT.with_borrow_mut(|slot| {
        if let Some(effect) = slot.take() {
            effect();
        }
    });
    Ok((Some(prepared), resume_root))
}

/// The TUI argv up to its prompt; Herdr types the prompt into the composer instead.
pub(in crate::services::codex) fn codex_tui_option_args(
    options: &CodexLaunchOptions,
) -> Vec<String> {
    let mut args = Vec::new();
    if options.resume_session_id.is_some() {
        args.push("resume".to_string());
    }
    append_codex_common_launch_args(&mut args, options);
    // Startup menus must never turn a routed Enter into a global toolchain update.
    args.extend([
        "-c".to_string(),
        "check_for_update_on_startup=false".to_string(),
    ]);
    append_codex_sandbox_args(&mut args, options.readonly_mode);
    if let Some(session_id) = options.resume_session_id.as_deref() {
        args.push(session_id.to_string());
    }
    args
}

/// The Herdr pane command of one prepared incarnation: no prompt in the argv, `--no-daemon`,
/// hooks and an exported home, with the Herdr pane variables unset right before the exec.
#[cfg(unix)]
pub(in crate::services::codex) fn prepare_codex_herdr_launch(
    prepared: &PreparedIncarnation,
    launchable: &CodexHerdrLaunchable,
    launch_options: &CodexLaunchOptions,
    auth_overlay: &crate::services::provider_auth_profile::ProviderAuthOverlay,
    home: &std::path::Path,
) -> Result<crate::services::herdr_launch::HerdrLaunchCommand, String> {
    use crate::services::herdr_launch::{HerdrLaunchCommand, unset_herdr_env_before_exec};
    crate::services::codex::validate_herdr_fresh_argv(
        &prepared.context.launch_mode,
        launch_options.resume_session_id.as_deref(),
        &[],
    )?;
    let logical = prepared.context.tmux_session.as_str();
    let (codex_bin, exec_path) = (
        &launchable.codex_bin,
        launchable.resolution.exec_path.as_deref(),
    );
    #[cfg(test)]
    let isolated_path = ISOLATED_HERDR_PATH.with_borrow(Clone::clone);
    #[cfg(test)]
    let exec_path = isolated_path.as_deref().or(exec_path);
    let channel = prepared.context.channel_id;
    let mut env_lines = build_tmux_launch_env_lines(exec_path, channel, Some(ProviderKind::Codex));
    env_lines
        .push_str(&crate::services::provider_auth_profile::overlay_shell_env_lines(auth_overlay));
    env_lines.push_str(&prepared.env_lines());
    env_lines.push_str(&format!(
        "export AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE={}\nexport CODEX_HOME={}\n",
        shell_escape(launchable.policy),
        shell_escape(&home.to_string_lossy())
    ));
    let mut args = codex_tui_option_args(launch_options);
    args.insert(0, "--no-daemon".to_string());
    let overrides = prepare_codex_tui_hook_overrides(logical, None, codex_bin, exec_path);
    if overrides.is_empty() {
        return Err("codex herdr launch: no hook overrides".into());
    }
    append_codex_config_overrides(&mut args, overrides);
    insert_codex_resume_option_before_other_options(&mut args, "--dangerously-bypass-hook-trust");
    crate::services::codex::validate_herdr_fresh_argv(
        &prepared.context.launch_mode,
        launch_options.resume_session_id.as_deref(),
        &args,
    )?;
    let script = render_codex_tui_tmux_script(&env_lines, codex_bin, &args);
    let script = unset_herdr_env_before_exec(&script)?;
    let path = crate::services::tmux_common::session_temp_path(logical, "sh");
    std::fs::write(&path, script)
        .map_err(|error| format!("Failed to write Codex Herdr launch script: {error}"))?;
    let cwd = launch_options.cwd.as_deref().map(PathBuf::from);
    Ok(HerdrLaunchCommand {
        cwd: cwd.ok_or("codex herdr launch: no working directory")?,
        command: format!("bash {}", shell_escape(&path)),
    })
}

/// Fresh panes must carry no resume command, even if the renderer changes its option order.
#[cfg(unix)]
pub(crate) fn validate_herdr_fresh_argv(
    mode: &str,
    resume: Option<&str>,
    args: &[String],
) -> Result<(), String> {
    let mut arguments = args.iter();
    let mut command = None;
    while let Some(arg) = arguments.next() {
        match arg.as_str() {
            "-c" | "--config" | "-m" | "--model" | "--sandbox" | "-C" | "--cd" | "--add-dir"
            | "--profile" | "--enable" | "--disable" => {
                if arguments.next().is_none() {
                    return Err("codex herdr launch: option value is missing".into());
                }
            }
            "--no-daemon"
            | "--dangerously-bypass-hook-trust"
            | "--dangerously-bypass-approvals-and-sandbox" => {}
            value if value.starts_with('-') => {
                return Err("codex herdr launch: unknown fresh option".into());
            }
            value => {
                command = Some(value);
                break;
            }
        }
    }
    if mode != "fresh" || resume.is_some() || command == Some("resume") {
        return Err(
            "codex herdr launch: fresh mode and no resume session or resume argv are required"
                .into(),
        );
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod fresh_tests;
#[cfg(all(test, unix))]
mod symlink_home_tests;
