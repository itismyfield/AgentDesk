//! A persistent routine resumes its own native conversation before changing the model.
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::routine_model::{self as proof, Requirement};
use std::{path::Path, sync::Arc};

pub(in crate::services::discord) struct Input<'a> {
    pub shared: &'a Arc<SharedData>,
    pub provider: &'a ProviderKind,
    pub channel: poise::serenity_prelude::ChannelId,
    pub tmux: &'a str,
    pub cwd: &'a str,
    pub session: Option<&'a str>,
    pub model: Option<&'a str>,
}
pub(in crate::services::discord) struct Prepared {
    pub requirement: Requirement,
    pub reused: bool,
}
fn refused(reason: impl std::fmt::Display) -> String {
    format!("NotSent(\"routine_model: {reason}\")")
}

pub(in crate::services::discord) async fn prepare(input: Input<'_>) -> Result<Prepared, String> {
    use super::tmux_recreate::{Relaunch, relaunch_channel_tmux_keeping_session};
    use crate::services::platform::tmux::{PaneLiveness, pane_liveness};
    use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
    let Input {
        shared,
        provider,
        channel,
        tmux,
        cwd,
        session,
        model,
    } = input;
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        return Err(refused("unsupported native provider"));
    }
    if crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
        provider,
        true,
        Some(channel.get()),
    )
    .driver
        != crate::services::provider_hosting::ProviderSessionDriver::TuiHosting
    {
        return Err(refused("native driver configuration changed"));
    }
    let overlay =
        crate::services::discord::org_schema::overlay_from_tmux_session(provider.clone(), tmux)
            .map_err(refused)?;
    let codex_home = if *provider == ProviderKind::Codex {
        crate::services::codex::codex_tui_home(&overlay)
            .map_err(refused)?
            .0
    } else {
        None
    };
    let model = model.filter(|model| !model.trim().is_empty());
    let live = match pane_liveness(tmux) {
        PaneLiveness::Live => true,
        PaneLiveness::DeadOrAbsent => false,
        PaneLiveness::ProbeError => return Err(refused("pane unobserved")),
    };
    let (native, context) = if live {
        if !crate::services::tmux_common::tmux_session_auth_profile_matches(
            tmux,
            &overlay.profile_id,
        ) {
            return Err(refused("native account changed"));
        }
        let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(tmux)
        else {
            return Err(refused("execution selector unproved"));
        };
        let context =
            binding_context::execution_context(provider.as_str(), &nonce).map_err(refused)?;
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux);
        let native = binding
            .and_then(|binding| binding.session_id)
            .or_else(|| context.expected_native_session_id.clone())
            .or_else(|| session.map(str::to_owned))
            .ok_or_else(|| refused("native session selector missing"))?;
        if context.tmux_session != tmux || context.provider != provider.as_str() {
            return Err(refused("execution belongs to another target"));
        }
        if context
            .expected_native_session_id
            .as_deref()
            .is_some_and(|expected| expected != native)
        {
            return Err(refused("native binding changed"));
        }
        if session.is_some_and(|requested| requested != native) {
            return Err(refused("native session differs from the claimed selector"));
        }
        (Some(native), Some(context))
    } else {
        (session.map(str::to_owned), None)
    };
    let native = native.as_deref();
    // Prove the same conversation under the account the real launch will take before ending it.
    if let Some(native) = native {
        let resumed = match provider {
            ProviderKind::Claude => {
                if !live {
                    crate::services::tmux_common::session_evidence::require_native_account(
                        tmux,
                        native,
                        &overlay.profile_id,
                    )?;
                }
                crate::services::claude::resolve_claude_tui_session_for_launch(
                    Path::new(cwd),
                    Some(native),
                    overlay.home.as_deref(),
                )
                .map(|s| s.resume && s.session_id == native)
                .map_err(refused)?
            }
            ProviderKind::Codex => {
                if !live {
                    crate::services::tmux_common::session_evidence::require_native_account(
                        tmux,
                        native,
                        &overlay.profile_id,
                    )?;
                }
                let sessions = codex_home.as_ref().map(|h| h.join("sessions"));
                let selected = crate::services::codex_tui::session::resolve_codex_tui_session(
                    Some(native),
                    Path::new(cwd),
                    sessions.as_deref(),
                    false,
                );
                selected.resume
                    && selected.candidate_count == 1
                    && selected.resume_session_id() == Some(native)
            }
            _ => false,
        };
        if !resumed {
            return Err(refused("same-session resume unavailable"));
        }
    }
    // An unqualified Claude resume would retain the conversation's previous model.
    let default_model = (*provider == ProviderKind::Claude && model.is_none() && native.is_some())
        .then(|| {
            crate::services::claude_tui::session::default_model_for_home(overlay.home.as_deref())
        });
    let requirement = Requirement {
        provider: provider.as_str().into(),
        tmux: tmux.into(),
        model: model.map(str::to_owned),
        default_model,
        resume: native.map(str::to_string),
    };
    let reused = live
        && context
            .as_ref()
            .zip(native)
            .is_some_and(|(context, native)| {
                proof::bind_session(provider.as_str(), tmux, &context.execution_nonce, native)
                    .is_ok()
                    && requirement.matches(&context.execution_nonce, native)
            });
    if live {
        if crate::services::discord::router::hosted_tui_promote_readiness_blocked(
            shared, provider, channel,
        )
        .await
        {
            return Err(refused("native turn is busy or unreadable"));
        }
        if !reused {
            match relaunch_channel_tmux_keeping_session(
                shared,
                provider,
                channel,
                tmux,
                "routine model change",
            )
            .await
            {
                Relaunch::Relaunched => {}
                other => return Err(refused(format!("same-session relaunch refused: {other:?}"))),
            }
            if !crate::services::tmux_common::tmux_session_auth_profile_matches(
                tmux,
                &overlay.profile_id,
            ) {
                return Err(refused("account marker was not preserved"));
            }
            if pane_liveness(tmux) != PaneLiveness::DeadOrAbsent {
                return Err(refused("old execution survived relaunch"));
            }
        }
    }
    Ok(Prepared {
        requirement,
        reused,
    })
}

pub(in crate::services::discord) fn applies(
    provider: &ProviderKind,
    channel: u64,
    tmux: Option<&str>,
) -> bool {
    use crate::services::provider_hosting::{
        ProviderSessionDriver, resolve_provider_session_selection_with_channel,
    };
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        return false;
    }
    resolve_provider_session_selection_with_channel(provider, true, Some(channel)).driver
        == ProviderSessionDriver::TuiHosting
        || tmux
            .and_then(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session)
            .is_some_and(|b| {
                matches!(
                    b.runtime_kind,
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                        | crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                )
            })
}
