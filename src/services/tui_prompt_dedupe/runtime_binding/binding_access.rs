//! Runtime projections are checked after releasing the cache lock.

use super::*;

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

/// Per session, the source and offset a confirmed delivery last reached. Kept apart from the
/// binding cursor, which a rehydrate seeds at the file's end and the prompt scan advances.
static RESUME_CHECKPOINTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (String, u64)>>,
> = std::sync::LazyLock::new(Default::default);

/// Advances the binding cursor after a confirmed delivery and records it as a resume checkpoint.
pub(crate) fn advance_tmux_runtime_binding_checkpoint(
    tmux_session_name: &str,
    output_path: &str,
    offset: u64,
) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if !tmux_session_name.is_empty() && !output_path.trim().is_empty() {
        let mut checkpoints = RESUME_CHECKPOINTS.lock().unwrap_or_else(|e| e.into_inner());
        checkpoints.insert(
            tmux_session_name.to_owned(),
            (output_path.to_owned(), offset),
        );
    }
    super::advance_tmux_runtime_binding_offset(tmux_session_name, output_path, offset)
}

/// The offset a confirmed delivery last reached in `output_path` for this session, if any.
pub(crate) fn runtime_binding_resume_checkpoint(
    tmux_session_name: &str,
    output_path: &str,
) -> Option<u64> {
    let checkpoints = RESUME_CHECKPOINTS.lock().unwrap_or_else(|e| e.into_inner());
    let (path, offset) = checkpoints.get(tmux_session_name.trim())?;
    (path == output_path).then_some(*offset)
}

/// Per transcript, its length when a rehydrate first observed it in this process. Not delivery
/// evidence: it only marks where output written after that observation begins.
static BOOT_READ_STARTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(Default::default);

/// Keeps the first observed length of `output_path`; later observations never move it.
pub(crate) fn record_boot_read_start(output_path: &str, offset: u64) {
    if !output_path.trim().is_empty() {
        let mut starts = BOOT_READ_STARTS.lock().unwrap_or_else(|e| e.into_inner());
        starts.entry(output_path.to_owned()).or_insert(offset);
    }
}

/// The length a rehydrate first observed `output_path` at in this process, if any.
pub(crate) fn boot_read_start(output_path: &str) -> Option<u64> {
    let starts = BOOT_READ_STARTS.lock().unwrap_or_else(|e| e.into_inner());
    starts.get(output_path).copied()
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
