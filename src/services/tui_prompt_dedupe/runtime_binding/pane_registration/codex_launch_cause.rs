use super::*;
use crate::services::codex_tui::execution_end::identity::fresh_source_matches;
use crate::services::codex_tui::session::source_observation::{
    CodexHookSourceClaim, CodexRolloutSource, verify_codex_hook_source,
};
use std::path::Path;

pub(super) fn cause(
    authority: &crate::services::tmux_common::TmuxSourceAuthority<'_>,
    channel: u64,
    binding: &TuiRuntimeBinding,
) -> Option<CauseSource> {
    let observed = Some(CauseSource::Observed);
    let tmux = authority.session();
    let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(tmux) else {
        return observed;
    };
    let Some(context) = binding_context::input_context("codex", &nonce) else {
        return observed;
    };
    if context.schema != 1
        || context.provider != "codex"
        || context.execution_nonce != nonce
        || context.tmux_session != tmux
        || context.launch_mode != "fresh"
        || context.expected_native_session_id.is_some()
        || context.source_policy.as_deref() != Some("shadow")
        || context.channel_id != Some(channel)
        || context.owner_runtime_root != crate::services::tmux_common::current_tmux_owner_marker()
    {
        return observed;
    }
    let (Some(root), Some(id)) = (
        context.provider_root.as_deref(),
        binding.session_id.as_deref(),
    ) else {
        return observed;
    };
    let path = Path::new(&binding.output_path);
    let source = match verify_codex_hook_source(
        root,
        &CodexHookSourceClaim {
            session_id: id,
            transcript_path: Some(path),
            expected_source: CodexRolloutSource::Cli,
        },
    ) {
        Ok(source) => source,
        Err(error) if error.may_resolve_later() => return None,
        Err(_) => return observed,
    };
    if source.session_id != id || path.canonicalize().ok().as_ref() != Some(&source.rollout_path) {
        return observed;
    }
    fresh_source_matches(&context, &source).map(|matches| {
        if matches {
            CauseSource::Launch
        } else {
            CauseSource::Observed
        }
    })
}
