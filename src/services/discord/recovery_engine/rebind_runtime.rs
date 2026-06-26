use super::*;

pub(super) struct RebindRuntimeState {
    pub(super) output_path: String,
    pub(super) synthetic_initial_offset: u64,
    pub(super) input_fifo_path: Option<String>,
    pub(super) runtime_kind: Option<RuntimeHandoffKind>,
    pub(super) session_id: Option<String>,
}

pub(super) fn resolve_rebind_runtime_state(
    provider: &ProviderKind,
    tmux_session_name: &str,
    existing_saved_output_path: Option<&str>,
    existing_session_id: Option<String>,
) -> Result<RebindRuntimeState, RebindError> {
    let existing_runtime_binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    let observed_runtime_kind = crate::services::tmux_common::resolve_tmux_runtime_kind_marker(
        tmux_session_name,
    )
    .or_else(|| {
        existing_runtime_binding
            .as_ref()
            .map(|binding| binding.runtime_kind)
    });
    if provider == &ProviderKind::Codex
        && observed_runtime_kind == Some(RuntimeHandoffKind::CodexTui)
    {
        return Err(RebindError::RuntimeBindingUnavailable {
            tmux_session: tmux_session_name.to_string(),
            runtime_kind: RuntimeHandoffKind::CodexTui,
        });
    }

    let (default_output_path, default_input_fifo) = tmux_runtime_paths(tmux_session_name);
    let input_fifo_path = Some(default_input_fifo);
    let runtime_kind = observed_runtime_kind;
    let session_id = existing_session_id;
    let fallback_output_path = existing_saved_output_path
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_output_path.clone());
    let (output_path, synthetic_initial_offset) = resolve_output_path_for_rebind(
        tmux_session_name,
        &default_output_path,
        &fallback_output_path,
    )?;

    Ok(RebindRuntimeState {
        output_path,
        synthetic_initial_offset,
        input_fifo_path,
        runtime_kind,
        session_id,
    })
}

fn resolve_output_path_for_rebind(
    tmux_session_name: &str,
    default_output_path: &str,
    fallback_output_path: &str,
) -> Result<(String, u64), RebindError> {
    #[cfg(unix)]
    {
        match detect_live_tmux_output_path(tmux_session_name, fallback_output_path) {
            Ok(Some(detected)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ♻ rebind adopted live tmux output path for {}: {} -> {} (offset {})",
                    tmux_session_name,
                    default_output_path,
                    detected.path,
                    detected.initial_offset
                );
                Ok((detected.path, detected.initial_offset))
            }
            Ok(None) => Ok((
                fallback_output_path.to_string(),
                std::fs::metadata(fallback_output_path)
                    .map(|m| m.len())
                    .unwrap_or(0),
            )),
            Err(stale) => Err(RebindError::StaleOutputPath {
                tmux_session: tmux_session_name.to_string(),
                output_path: fallback_output_path.to_string(),
                live_fd: stale.fd,
                live_inode: stale.inode,
                live_path: stale.raw_path,
            }),
        }
    }
    #[cfg(not(unix))]
    {
        Ok((
            fallback_output_path.to_string(),
            std::fs::metadata(fallback_output_path)
                .map(|m| m.len())
                .unwrap_or(0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
    use std::sync::{Mutex, OnceLock};

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn direct_codex_tui_rebind_rejects_existing_runtime_binding() {
        let _guard = test_lock().lock().unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
        let tmux_session_name = "AgentDesk-codex-adk-cdx";
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            tmux_session_name,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("019f0111-fc32".to_string()),
                last_offset: 42,
                relay_last_offset: None,
            },
        );

        let result =
            resolve_rebind_runtime_state(&ProviderKind::Codex, tmux_session_name, None, None);

        assert!(matches!(
            result,
            Err(RebindError::RuntimeBindingUnavailable {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                ..
            })
        ));
        crate::services::tui_prompt_dedupe::reset_state_for_tests();
    }
}
