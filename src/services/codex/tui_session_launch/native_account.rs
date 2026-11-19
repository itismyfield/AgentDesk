//! Account evidence for a saved Codex rollout is independent of its pane.

use crate::services::codex_tui::session::{CodexTuiSessionSelection, resolve_codex_tui_session};
use crate::services::platform::tmux::{PaneLiveness, pane_liveness};
use crate::services::provider_auth_profile::ProviderAuthOverlay;
use crate::services::tmux_common::{require_native_account, with_tmux_source_authority};
use crate::services::tui_input::routine_model as proof;
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, SpawnNonceMarker, observe_spawn_nonce_marker,
};

pub(in crate::services::codex) fn select_for_routine_or_legacy(
    tmux: &str,
    cwd: &str,
    selected: Option<&str>,
    auth: &ProviderAuthOverlay,
    (profile_matches, force_fresh): (bool, bool),
) -> Result<(CodexTuiSessionSelection, bool), String> {
    let strict = proof::current().is_some();
    let live = if strict {
        match pane_liveness(tmux) {
            PaneLiveness::Live => true,
            PaneLiveness::DeadOrAbsent => false,
            PaneLiveness::ProbeError => {
                return Err("NotSent(\"routine_resume_pane_unobserved\")".into());
            }
        }
    } else {
        crate::services::session_host::legacy_collapse::tmux_live_pane_bool(tmux)
    };
    let (selected, sessions) = if strict
        && !live
        && let Some(session) = selected
    {
        require_native_account(tmux, session, &auth.profile_id)?;
        let (home, _) = super::codex_tui_home(auth)?;
        (Some(session), home.map(|home| home.join("sessions")))
    } else {
        (profile_matches.then_some(selected).flatten(), None)
    };
    let selection =
        resolve_codex_tui_session(selected, cwd.as_ref(), sessions.as_deref(), force_fresh);
    Ok((selection, live && profile_matches))
}

pub(super) fn record(
    context: &BindingContext,
    session: &str,
    profile: &str,
    strict: bool,
) -> Result<(), String> {
    if context
        .expected_native_session_id
        .as_deref()
        .is_some_and(|expected| expected != session)
    {
        return Ok(());
    }
    with_tmux_source_authority(&context.tmux_session, |authority| {
        if !matches!(observe_spawn_nonce_marker(authority.session()),
            SpawnNonceMarker::Known(nonce) if nonce == context.execution_nonce)
        {
            return Ok(());
        }
        crate::services::tmux_common::write_native_session_auth(
            authority.session(),
            session,
            profile,
        )?;
        if strict {
            proof::bind_session_under_authority(
                authority,
                "codex",
                &context.execution_nonce,
                session,
            )?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tmux_common as tc;

    #[test]
    fn routine_fix6_codex_native_account_follows_the_observed_rollout() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let tmux = "codex-routine-account-evidence";
        let session = "ad259d2a-7922-4915-961d-f25cd9b3d89b";
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: "12345678901234567890123456789012".into(),
            tmux_session: tmux.into(),
            channel_id: None,
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
            resume_start: None,
        };
        let requirement = proof::Requirement {
            provider: "codex".into(),
            tmux: tmux.into(),
            model: Some("test-model".into()),
            default_model: None,
            resume: None,
        };
        proof::scope(requirement, || {
            proof::prepared(
                "codex",
                tmux,
                "",
                false,
                &context.execution_nonce,
                &["-m".into(), "test-model".into(), "--".into()],
            )
            .unwrap();
        });
        tc::write_tmux_session_auth_profile(tmux, "work").unwrap();
        record(&context, session, "work", true).unwrap();
        assert!(require_native_account(tmux, session, "work").is_err());
        std::fs::write(
            tc::session_temp_path(tmux, "spawn_nonce"),
            &context.execution_nonce,
        )
        .unwrap();
        record(&context, session, "work", true).unwrap();
        tc::cleanup_session_temp_files(tmux);
        assert!(proof::tracks_native_session("codex", tmux, session));
        assert_eq!(require_native_account(tmux, session, "work"), Ok(()));
        assert!(require_native_account(tmux, session, "other").is_err());
        assert!(!std::path::Path::new(&tc::session_temp_path(tmux, "spawn_nonce")).exists());
    }
}
