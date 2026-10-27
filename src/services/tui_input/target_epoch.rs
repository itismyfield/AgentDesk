//! Serializable epoch evidence is separate from a live observation that admits writes.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct PaneEpoch {
    pub host: String,
    pub endpoint: String,
    pub owner: String,
    pub root: String,
    pub execution_nonce: String,
    pub physical: [String; 5],
}

#[cfg(unix)]
pub(crate) struct LiveEpoch(PaneEpoch);

#[cfg(unix)]
impl LiveEpoch {
    pub(crate) fn observe(
        provider: &str,
        target: &serde_json::Value,
        pin: &crate::services::tui_input::submission_tmux::guarded::PaneIdentity,
    ) -> Result<Self, String> {
        use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
        let owner = target["owner"].as_str().ok_or("missing owner")?;
        let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(owner)
        else {
            return Err("unknown spawn nonce".into());
        };
        let context = binding_context::execution_context(provider, &nonce)?;
        if target["execution_nonce"] != nonce
            || target["root"] != context.owner_runtime_root
            || target["host"] != "tmux"
            || context.tmux_session != owner
            || context.host.is_none()
        {
            return Err("epoch authority mismatch".into());
        }
        Ok(Self(PaneEpoch {
            host: "tmux".into(),
            endpoint: context.host.ok_or("unknown host")?,
            owner: owner.into(),
            root: context.owner_runtime_root,
            execution_nonce: nonce,
            physical: pin.epoch_fields(),
        }))
    }
    pub(crate) fn record(&self) -> PaneEpoch {
        self.0.clone()
    }
    pub(crate) fn condition(&self, session: &str, previous: &str) -> Result<String, String> {
        use crate::services::tui_prompt_dedupe::binding_context::{
            SpawnNonceMarker, observe_spawn_nonce_marker,
        };
        if session != self.0.owner
            || observe_spawn_nonce_marker(session)
                != SpawnNonceMarker::Known(self.0.execution_nonce.clone())
        {
            return Err("epoch changed".into());
        }
        let names = ["pid", "session_id", "window_id", "pane_id", "pane_pid"];
        let mut condition = previous.to_owned();
        for (name, value) in names.iter().zip(&self.0.physical) {
            condition = format!("#{{&&:{condition},#{{==:#{{{name}}},{value}}}}}");
        }
        Ok(condition)
    }
}
