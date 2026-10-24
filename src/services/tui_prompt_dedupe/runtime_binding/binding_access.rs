//! Runtime projections are checked after releasing the cache lock.

use super::*;

pub(super) fn same_runtime_source(
    left: Option<&TuiRuntimeBinding>,
    right: Option<&TuiRuntimeBinding>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.runtime_kind == right.runtime_kind
                && left.output_path == right.output_path
                && left.relay_output_path == right.relay_output_path
                && left.input_fifo_path == right.input_fifo_path
                && left.session_id == right.session_id
        }
        (None, None) => true,
        _ => false,
    }
}

/// Raw diagnostic state; `Present` also passed a read-only source permission check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeObservation {
    Missing,
    Expired(TuiRuntimeBinding),
    Withheld(TuiRuntimeBinding),
    Present(TuiRuntimeBinding),
}

/// Observes without TTL purge, refresh, publication or reacquiring the caller's source authority.
pub(crate) fn runtime_observation_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
) -> RuntimeObservation {
    let raw = {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.runtime_by_tmux.get(authority.session()).map(|entry| {
            (
                entry.value.clone(),
                entry.recorded_at.elapsed() > SESSION_MAPPING_TTL,
            )
        })
    };
    match raw {
        None => RuntimeObservation::Missing,
        Some((binding, true)) => RuntimeObservation::Expired(binding),
        Some((binding, false)) if observation_allowed(authority, &binding) => {
            RuntimeObservation::Present(binding)
        }
        Some((binding, false)) => RuntimeObservation::Withheld(binding),
    }
}

fn observation_allowed(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> bool {
    use super::super::binding_context::{self, SpawnNonceMarker};
    use crate::services::tmux_common as tc;
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui {
        return true;
    }
    let SpawnNonceMarker::Known(nonce) =
        binding_context::observe_spawn_nonce_marker(authority.session())
    else {
        return false;
    };
    let Ok(context) = binding_context::execution_context("codex", &nonce) else {
        return false;
    };
    if context.tmux_session != authority.session()
        || context.owner_runtime_root != tc::current_tmux_owner_marker()
    {
        return false;
    }
    match context.source_policy.as_deref() {
        None | Some("shadow") => true,
        Some("verified") => {
            let Ok(proof) = codex_verified::proof_for_binding(authority, &context, binding) else {
                return false;
            };
            let path =
                tc::session_temp_path(authority.session(), tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|marker| {
                    marker["codex_ownership"] == codex_verified::marker_proof(&context, &proof)
                        && marker["rollout_path"].as_str() == proof.source.path.to_str()
                        && marker["session_id"].as_str() == Some(&proof.source.session_id)
                })
        }
        Some(_) => false,
    }
}

pub fn register_provider_session(
    provider: &str,
    provider_session_id: &str,
    tmux_session_name: &str,
) {
    let provider_session_id = provider_session_id.trim();
    let tmux_session_name = tmux_session_name.trim();
    if provider_session_id.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let register = || {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        state.tmux_by_provider_session.insert(
            PromptKey::new(provider, provider_session_id),
            TimedValue {
                value: tmux_session_name.to_string(),
                recorded_at: Instant::now(),
            },
        );
    };
    if normalize_provider(provider) == "codex" {
        crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
            let allowed = match codex_verified::current_context(authority) {
                Ok(None) => true,
                Ok(Some(context)) => {
                    binding_events::codex::read_ownership(&context).is_ok_and(|fold| {
                        fold.verified.is_some_and(|proof| {
                            let binding = TuiRuntimeBinding {
                                runtime_kind: RuntimeHandoffKind::CodexTui,
                                output_path: proof.source.path.display().to_string(),
                                session_id: Some(proof.source.session_id.clone()),
                                relay_output_path: None,
                                input_fifo_path: None,
                                last_offset: 0,
                                relay_last_offset: Some(0),
                            };
                            proof.source.session_id == provider_session_id
                                && codex_verified::consumer_allowed(authority, &binding)
                        })
                    })
                }
                Err(_) => false,
            };
            if allowed {
                register();
            }
        });
    } else {
        register();
    }
}

pub(crate) fn runtime_binding_for_tmux_session(
    tmux_session_name: &str,
) -> Option<TuiRuntimeBinding> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        runtime_binding_for_tmux_session_under_source_authority(authority)
    })
}

pub(crate) fn runtime_binding_for_tmux_session_under_source_authority(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
) -> Option<TuiRuntimeBinding> {
    let tmux_session_name = authority.session();
    let binding = with_runtime_binding_state_under_source_authority(authority, |state| {
        state
            .runtime_by_tmux
            .get(tmux_session_name)
            .map(|entry| entry.value.clone())
    })?;
    codex_verified::consumer_allowed(authority, &binding).then_some(binding)
}

pub(crate) fn runtime_bindings_for_kind(
    runtime_kind: RuntimeHandoffKind,
) -> Vec<(String, TuiRuntimeBinding)> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let bindings = state
        .runtime_by_tmux
        .iter()
        .filter(|(_, entry)| {
            entry.value.runtime_kind == runtime_kind
                && entry.recorded_at.elapsed() <= SESSION_MAPPING_TTL
        })
        .map(|(tmux_session_name, entry)| (tmux_session_name.clone(), entry.value.clone()))
        .collect::<Vec<_>>();
    drop(state);
    bindings
        .into_iter()
        .filter(|(tmux, binding)| {
            crate::services::tmux_common::with_tmux_source_authority(tmux, |authority| {
                codex_verified::consumer_allowed(authority, binding)
            })
        })
        .collect()
}

pub(crate) fn codex_verified_publication_allowed(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    binding: &TuiRuntimeBinding,
) -> bool {
    codex_verified::publication_allowed(authority, binding)
}

pub(crate) fn codex_verified_marker_metadata(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    path: &std::path::Path,
    id: Option<&str>,
) -> Result<Option<serde_json::Value>, String> {
    codex_verified::marker_metadata(authority, path, id)
}

#[cfg(all(test, unix))]
#[path = "binding_access_tests.rs"]
mod tests;
