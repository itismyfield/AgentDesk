//! The transcript a busy front registers with: the running turn's native source as its host
//! proved it, read before the front's registration is observed.
use super::*;
use crate::services::discord::inflight::InflightTurnState;

/// The transcript path and native session a busy front on `session` registers with, owned.
pub(super) fn live_source(
    provider: ProviderKind,
    host: &TurnHost,
    session: &str,
    row: &InflightTurnState,
) -> Option<(String, Option<String>)> {
    // Claude's prelaunch row has no transcript until its observer hands it back. Its live pane
    // already has the binding that the idle observer used before submitting the first input.
    let binding = (provider == ProviderKind::Claude
        && matches!(host, TurnHost::Tmux)
        && row.output_path.is_none())
    .then(|| crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session))
    .flatten()
    .filter(|binding| {
        binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
    });
    let output = row
        .output_path
        .as_deref()
        .or_else(|| binding.as_ref().map(|binding| binding.output_path.as_str()));
    let native = binding
        .as_ref()
        .and_then(|binding| binding.session_id.as_deref())
        .or(row.session_id.as_deref());
    let live = output.map(|path| (path, native));
    live.map(|(path, native)| (path.to_owned(), native.map(str::to_owned)))
}
