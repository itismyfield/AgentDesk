use super::*;
use crate::services::codex_tui::canary;

// Observe the held gate without its TTL purge or publication side effects.
pub(crate) fn codex_verified_hold_detail(tmux: &str) -> &'static str {
    tc::with_tmux_source_authority(tmux, |authority| {
        let nonce = match binding_context::observe_spawn_nonce_marker(tmux) {
            SpawnNonceMarker::Known(nonce) => nonce,
            SpawnNonceMarker::Absent => return "nonce_marker_missing",
            SpawnNonceMarker::Unreadable => return "nonce_marker_unreadable",
        };
        let context = match binding_context::execution_context("codex", &nonce) {
            Ok(context) => context,
            Err(_) => return unavailable_context_reason(authority, &nonce),
        };
        if context.tmux_session != tmux {
            return "tmux_session_mismatch";
        }
        if context.owner_runtime_root != tc::current_tmux_owner_marker() {
            return "owner_root_mismatch";
        }
        if context.source_policy.as_deref() != Some("verified") {
            return "source_policy_changed";
        }
        let fold = match binding_events::codex::read_ownership(&context) {
            Ok(fold) => fold,
            Err(_) => return "ownership_unreadable",
        };
        if !fold.pending.is_empty() {
            return "ownership_pending";
        }
        if fold.verified.is_none() {
            return "ownership_missing";
        }
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        if !state.runtime_by_tmux.contains_key(tmux) {
            return "binding_missing";
        }
        "source_identity_mismatch"
    })
}

fn unavailable_context_reason(authority: &TmuxSourceAuthority<'_>, nonce: &str) -> &'static str {
    let tmux = authority.session();
    let channel = STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .channel_by_tmux
        .get(tmux)
        .map(|entry| entry.value);
    if let Some(channel) = channel {
        match binding_events::codex::context_for_nonce(channel, tmux, Some(nonce)) {
            Ok(Some(context)) if context.source_policy.as_deref() == Some("verified") => {
                return "execution_context_unreadable";
            }
            Err(_) => return "execution_history_unreadable",
            _ => {}
        }
    }
    let marker = tc::session_temp_path(tmux, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT);
    if std::fs::read(marker)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|marker| marker.get("codex_ownership").is_some())
    {
        return "proof_context_missing";
    }
    if tmux == canary::CANARY_TMUX {
        match binding_events::codex::context_for_nonce(canary::CANARY_CHANNEL, tmux, None) {
            Ok(Some(context)) if context.source_policy.as_deref() == Some("verified") => {
                return "canary_pin_unavailable";
            }
            Err(_) => return "canary_history_unreadable",
            _ => {}
        }
        if canary::enabled_for(tmux) {
            return "canary_unavailable";
        }
    }
    "execution_context_unreadable"
}
