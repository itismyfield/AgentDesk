//! The spawn marker and canonical context one source-policy judgement shares.

use super::*;

/// One read of the marker and of the canonical context it names; checks within one
/// judgement consult this instead of reopening either file.
pub(in super::super) struct CanonicalObservation {
    pub(super) marker: SpawnNonceMarker,
    /// The canonical context of the execution under check, when it is readable.
    pub(in super::super) canonical: Option<BindingContext>,
}

impl CanonicalObservation {
    pub(in super::super) fn read(tmux: &str) -> Self {
        let marker = binding_context::observe_spawn_nonce_marker(tmux);
        let canonical = match &marker {
            SpawnNonceMarker::Known(nonce) => {
                binding_context::execution_context("codex", nonce).ok()
            }
            _ => None,
        };
        Self { marker, canonical }
    }

    pub(in super::super) fn nonce(&self) -> Option<&str> {
        match &self.marker {
            SpawnNonceMarker::Known(nonce) => Some(nonce),
            _ => None,
        }
    }

    /// `context` is still the session's current verified execution, as observed.
    pub(super) fn validate(
        &self,
        authority: &TmuxSourceAuthority<'_>,
        context: &BindingContext,
    ) -> io::Result<()> {
        let canonical = (self.canonical.as_ref())
            .ok_or_else(|| invalid("Codex canonical context unavailable"))?;
        if self.nonce() != Some(context.execution_nonce.as_str())
            || canonical != context
            || context.tmux_session != authority.session()
            || context.owner_runtime_root != tc::current_tmux_owner_marker()
            || context.source_policy.as_deref() != Some("verified")
        {
            return Err(invalid("Codex execution changed"));
        }
        Ok(())
    }
}
