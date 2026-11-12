//! Runtime-scoped metadata for tmux producers, independent of session-name parsing.

use crate::domain::session_identity::managed::ManagedIdentity;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use sha2::{Digest, Sha256};

pub(crate) fn current_owner_scope() -> Result<String, String> {
    let root = crate::config::runtime_root().ok_or("runtime root unavailable")?;
    let node = crate::services::platform::shell::hostname_short();
    Ok(owner_scope(&root.to_string_lossy(), &node))
}

fn owner_scope(root: &str, node: &str) -> String {
    let mut hash = Sha256::new();
    for part in ["agentdesk-session-owner-v1", root, node] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

pub(crate) fn runtime_identity(session: &str) -> Result<ManagedIdentity, String> {
    Ok(ManagedIdentity {
        v: 1,
        owner_scope: current_owner_scope()?,
        provider: None,
        token_namespace: None,
        channel_id: None,
        parent_channel_id: None,
        thread_id: None,
        identity_kind: None,
        name_kind: "service".into(),
        session_record_id: None,
        logical_key: None,
        spawn_nonce: None,
        display_name: session.into(),
    })
}

pub(crate) fn binding_identity(context: &BindingContext) -> Result<ManagedIdentity, String> {
    let mut identity = runtime_identity(&context.tmux_session)?;
    identity.provider = Some(context.provider.clone());
    identity.channel_id = context.channel_id.map(|id| id.to_string());
    identity.spawn_nonce = Some(context.execution_nonce.clone());
    // Launch classification stays explicit; channel presence alone also occurs in scheduled work.
    identity.name_kind = context.launch_mode.clone();
    Ok(identity)
}

pub(crate) fn provider_identity(
    session: &str,
    provider: &str,
    channel: Option<u64>,
    name_kind: &str,
) -> Result<ManagedIdentity, String> {
    let mut identity = runtime_identity(session)?;
    identity.provider = Some(provider.into());
    identity.channel_id = channel.map(|id| id.to_string());
    identity.name_kind = name_kind.into();
    Ok(identity)
}

impl crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation {
    pub(crate) fn tmux_identity(&self) -> Result<ManagedIdentity, String> {
        binding_identity(&self.context)
    }
}
