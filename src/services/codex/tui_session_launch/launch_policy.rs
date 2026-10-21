use super::*;

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
        codex_tui_home(overlay).0.map(|home| home.join("sessions")),
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
