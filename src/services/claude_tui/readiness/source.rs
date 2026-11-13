//! A readiness observation follows the current launch and native source, never cwd's newest file.
use crate::services::tui_prompt_dedupe::{
    self,
    binding_context::{self, SpawnNonceMarker},
};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Source {
    pub session: String,
    pub nonce: Option<String>,
    pub native: String,
    pub path: PathBuf,
}
pub(super) fn nonce(session: &str) -> Option<Option<String>> {
    match binding_context::observe_spawn_nonce_marker(session) {
        SpawnNonceMarker::Known(value) => Some(Some(value)),
        SpawnNonceMarker::Absent => Some(None),
        SpawnNonceMarker::Unreadable => None,
    }
}
impl Source {
    pub(super) fn current(session: &str, explicit: Option<&Path>) -> Option<Self> {
        let nonce = nonce(session)?;
        if let Some(nonce) = &nonce {
            let ctx = binding_context::execution_context("claude", nonce).ok()?;
            if ctx.tmux_session != session {
                return None;
            }
        }
        let latest = nonce.as_ref().and_then(|nonce| {
            super::ledger()
                .starts
                .get(&(session.to_owned(), nonce.clone()))
                .map(|(_, source)| source.clone())
        });
        if let Some(binding) = tui_prompt_dedupe::runtime_binding_for_tmux_session(session) {
            if binding.runtime_kind
                != crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
            {
                return None;
            }
            let path = PathBuf::from(binding.relay_output_path());
            if let (Some(explicit), Some(latest)) = (explicit, latest.as_ref())
                && explicit == latest.path
                && (path != latest.path
                    || binding.session_id.as_deref() != Some(latest.native.as_str()))
                && super::clear::evidence(latest).is_some()
            {
                return Some(latest.clone());
            }
            if explicit.is_some_and(|given| given != path) {
                return None;
            }
            let source = Self {
                session: session.into(),
                nonce,
                native: binding
                    .session_id
                    .or_else(|| tui_prompt_dedupe::provider_session_for_tmux("claude", session))
                    .or_else(|| path.file_stem()?.to_str().map(str::to_owned))?,
                path,
            };
            if latest.as_ref().is_some_and(|latest| latest != &source) {
                return None;
            }
            return Some(source);
        }
        if let Some(latest) = latest {
            return explicit
                .is_none_or(|path| path == latest.path)
                .then_some(latest);
        }
        let state = super::ledger();
        let mut sources = state.entries.keys().filter(|source| {
            source.session == session
                && source.nonce == nonce
                && explicit.is_none_or(|path| path == source.path)
        });
        let source = sources.next()?.clone();
        sources.next().is_none().then_some(source)
    }
}

#[cfg(unix)]
mod clear;
#[cfg(unix)]
pub(crate) use clear::adopt as adopt_awaiting_clear;
#[cfg(unix)]
pub(super) use clear::note as note_clear;

#[cfg(all(test, unix))]
pub(crate) use clear::AFTER_PUBLICATION as CLEAR_AFTER_PUBLICATION;

#[cfg(unix)]
pub(crate) use clear::wakeable as clear_wakeable;
